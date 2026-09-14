use std::future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use koharu::app::supervise_headless_server;
use tokio::sync::oneshot;

#[tokio::test]
async fn early_serve_error_is_returned_without_waiting_for_shutdown_signal() -> Result<()> {
    let server = tokio::spawn(async { Err(anyhow!("serve failed")) });
    let (shutdown_tx, _shutdown_rx) = oneshot::channel();
    let (cancel_bootstrap_tx, cancel_bootstrap_rx) = oneshot::channel();

    let error = tokio::time::timeout(
        Duration::from_secs(1),
        supervise_headless_server(
            server,
            shutdown_tx,
            async move {
                cancel_bootstrap_rx.await?;
                Ok(())
            },
            move || {
                let _ = cancel_bootstrap_tx.send(());
            },
            future::pending::<Result<()>>(),
            async { Ok(()) },
        ),
    )
    .await?
    .expect_err("serve error must propagate");

    assert!(format!("{error:#}").contains("serve failed"));
    Ok(())
}

#[tokio::test]
async fn accepted_project_history_route_drains_before_application_cleanup() -> Result<()> {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let shutdown_count = Arc::new(AtomicUsize::new(0));
    let route_persisted = Arc::new(AtomicBool::new(false));
    let server_completed = Arc::new(AtomicBool::new(false));
    let server_completed_in_task = server_completed.clone();
    let route_persisted_in_task = route_persisted.clone();
    let server = tokio::spawn(async move {
        shutdown_rx
            .await
            .map_err(|_| anyhow!("shutdown sender dropped"))?;
        // Represents an already-accepted /projects or /history/apply request
        // finishing its dirty scene + receipt write during Axum's drain.
        route_persisted_in_task.store(true, Ordering::SeqCst);
        server_completed_in_task.store(true, Ordering::SeqCst);
        Ok(())
    });
    let server_completed_in_shutdown = server_completed.clone();
    let shutdown_count_in_signal = shutdown_count.clone();

    tokio::time::timeout(
        Duration::from_secs(1),
        supervise_headless_server(
            server,
            shutdown_tx,
            async { Ok(()) },
            || {},
            async { Ok(()) },
            async move {
                anyhow::ensure!(
                    server_completed_in_shutdown.load(Ordering::SeqCst),
                    "application cleanup raced an accepted project/history route"
                );
                shutdown_count_in_signal.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        ),
    )
    .await??;

    assert_eq!(shutdown_count.load(Ordering::SeqCst), 1);
    assert!(server_completed.load(Ordering::SeqCst));
    assert!(route_persisted.load(Ordering::SeqCst));
    Ok(())
}

#[tokio::test]
async fn closed_shutdown_receiver_propagates_simultaneous_serve_error() -> Result<()> {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (receiver_closed_tx, receiver_closed_rx) = oneshot::channel();
    let (finish_server_tx, finish_server_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        drop(shutdown_rx);
        let _ = receiver_closed_tx.send(());
        let _ = finish_server_rx.await;
        Err(anyhow!("serve failed after closing shutdown receiver"))
    });

    let error = tokio::time::timeout(
        Duration::from_secs(1),
        supervise_headless_server(
            server,
            shutdown_tx,
            async { Ok(()) },
            || {},
            async move {
                receiver_closed_rx.await?;
                let _ = finish_server_tx.send(());
                Ok(())
            },
            async { Ok(()) },
        ),
    )
    .await?
    .expect_err("serve error must win the closed-receiver race");

    assert!(format!("{error:#}").contains("serve failed after closing shutdown receiver"));
    Ok(())
}

#[tokio::test]
async fn shutdown_during_non_cancellation_safe_bootstrap_joins_before_drain() -> Result<()> {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (signal_tx, signal_rx) = oneshot::channel();
    let (cancel_requested_tx, cancel_requested_rx) = oneshot::channel();
    let (bootstrap_started_tx, bootstrap_started_rx) = oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let bootstrap_finished = Arc::new(AtomicBool::new(false));
    let drain_started = Arc::new(AtomicBool::new(false));
    let cleanup_started = Arc::new(AtomicBool::new(false));
    let server_completed = Arc::new(AtomicBool::new(false));
    let bootstrap_finished_in_server = bootstrap_finished.clone();
    let drain_started_in_server = drain_started.clone();
    let server_completed_in_task = server_completed.clone();
    let server = tokio::spawn(async move {
        shutdown_rx.await?;
        drain_started_in_server.store(true, Ordering::SeqCst);
        anyhow::ensure!(
            bootstrap_finished_in_server.load(Ordering::SeqCst),
            "server drain began before non-cancellation-safe bootstrap work joined"
        );
        server_completed_in_task.store(true, Ordering::SeqCst);
        Ok(())
    });
    let bootstrap_finished_in_task = bootstrap_finished.clone();
    let bootstrap = async move {
        tokio::task::spawn_blocking(move || -> Result<()> {
            let _ = bootstrap_started_tx.send(());
            release_rx
                .recv_timeout(Duration::from_secs(1))
                .map_err(|error| anyhow!("blocking bootstrap release failed: {error}"))?;
            bootstrap_finished_in_task.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await??;
        Ok::<(), anyhow::Error>(())
    };
    let bootstrap_finished_in_cleanup = bootstrap_finished.clone();
    let cleanup_started_in_task = cleanup_started.clone();
    let server_completed_in_cleanup = server_completed.clone();

    let supervisor = tokio::spawn(supervise_headless_server(
        server,
        shutdown_tx,
        bootstrap,
        move || {
            let _ = cancel_requested_tx.send(());
        },
        async move { signal_rx.await.map_err(Into::into) },
        async move {
            cleanup_started_in_task.store(true, Ordering::SeqCst);
            anyhow::ensure!(
                bootstrap_finished_in_cleanup.load(Ordering::SeqCst),
                "bootstrap work was not joined before application cleanup"
            );
            anyhow::ensure!(
                server_completed_in_cleanup.load(Ordering::SeqCst),
                "application cleanup ran before server drain"
            );
            Ok(())
        },
    ));

    tokio::time::timeout(Duration::from_secs(1), bootstrap_started_rx).await??;
    signal_tx
        .send(())
        .map_err(|_| anyhow!("supervisor dropped shutdown signal receiver"))?;
    tokio::time::timeout(Duration::from_secs(1), cancel_requested_rx).await??;
    tokio::task::yield_now().await;

    assert!(
        !supervisor.is_finished(),
        "supervisor completed before blocking bootstrap JoinHandle"
    );
    assert!(!drain_started.load(Ordering::SeqCst));
    assert!(!cleanup_started.load(Ordering::SeqCst));

    release_tx
        .send(())
        .map_err(|_| anyhow!("blocking bootstrap dropped release receiver"))?;
    tokio::time::timeout(Duration::from_secs(1), supervisor).await???;

    assert!(bootstrap_finished.load(Ordering::SeqCst));
    assert!(drain_started.load(Ordering::SeqCst));
    assert!(cleanup_started.load(Ordering::SeqCst));
    assert!(server_completed.load(Ordering::SeqCst));
    Ok(())
}

#[tokio::test]
async fn shutdown_racing_bootstrap_error_drains_then_propagates_error() -> Result<()> {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (cancel_bootstrap_tx, cancel_bootstrap_rx) = oneshot::channel();
    let (bootstrap_started_tx, bootstrap_started_rx) = oneshot::channel();
    let bootstrap_finished = Arc::new(AtomicBool::new(false));
    let server_completed = Arc::new(AtomicBool::new(false));
    let cleanup_count = Arc::new(AtomicUsize::new(0));

    let bootstrap_finished_in_server = bootstrap_finished.clone();
    let server_completed_in_task = server_completed.clone();
    let server = tokio::spawn(async move {
        shutdown_rx.await?;
        anyhow::ensure!(
            bootstrap_finished_in_server.load(Ordering::SeqCst),
            "server drain raced unfinished bootstrap error"
        );
        server_completed_in_task.store(true, Ordering::SeqCst);
        Ok(())
    });

    let bootstrap_finished_in_task = bootstrap_finished.clone();
    let bootstrap = async move {
        let _ = bootstrap_started_tx.send(());
        cancel_bootstrap_rx.await?;
        bootstrap_finished_in_task.store(true, Ordering::SeqCst);
        Err(anyhow!("bootstrap failed after cancellation request"))
    };
    let server_completed_in_cleanup = server_completed.clone();
    let cleanup_count_in_task = cleanup_count.clone();

    let error = tokio::time::timeout(
        Duration::from_secs(1),
        supervise_headless_server(
            server,
            shutdown_tx,
            bootstrap,
            move || {
                let _ = cancel_bootstrap_tx.send(());
            },
            async move {
                bootstrap_started_rx.await?;
                Ok(())
            },
            async move {
                anyhow::ensure!(
                    server_completed_in_cleanup.load(Ordering::SeqCst),
                    "application cleanup ran before server drain"
                );
                cleanup_count_in_task.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        ),
    )
    .await?
    .expect_err("bootstrap error racing shutdown must propagate");

    assert!(format!("{error:#}").contains("bootstrap failed after cancellation request"));
    assert!(bootstrap_finished.load(Ordering::SeqCst));
    assert!(server_completed.load(Ordering::SeqCst));
    assert_eq!(cleanup_count.load(Ordering::SeqCst), 1);
    Ok(())
}
