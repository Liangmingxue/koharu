use std::future::{self, Future};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Result, anyhow};
use koharu::app::supervise_headless_server;
use tokio::sync::oneshot;

#[tokio::test]
async fn early_serve_error_is_returned_without_waiting_for_shutdown_signal() -> Result<()> {
    let server = tokio::spawn(async { Err(anyhow!("serve failed")) });
    let (shutdown_tx, _shutdown_rx) = oneshot::channel();

    let error = tokio::time::timeout(
        Duration::from_secs(1),
        supervise_headless_server(
            server,
            shutdown_tx,
            future::pending::<Result<()>>(),
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

struct DropFlag(Arc<AtomicBool>);

impl Future for DropFlag {
    type Output = Result<()>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Pending
    }
}

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn shutdown_before_bootstrap_completion_cancels_bootstrap_then_drains() -> Result<()> {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let bootstrap_dropped = Arc::new(AtomicBool::new(false));
    let server_completed = Arc::new(AtomicBool::new(false));
    let server_completed_in_task = server_completed.clone();
    let server = tokio::spawn(async move {
        shutdown_rx.await?;
        server_completed_in_task.store(true, Ordering::SeqCst);
        Ok(())
    });
    let bootstrap_dropped_in_cleanup = bootstrap_dropped.clone();
    let server_completed_in_cleanup = server_completed.clone();

    tokio::time::timeout(
        Duration::from_secs(1),
        supervise_headless_server(
            server,
            shutdown_tx,
            DropFlag(bootstrap_dropped.clone()),
            async { Ok(()) },
            async move {
                anyhow::ensure!(
                    bootstrap_dropped_in_cleanup.load(Ordering::SeqCst),
                    "bootstrap ownership remained live during cleanup"
                );
                anyhow::ensure!(
                    server_completed_in_cleanup.load(Ordering::SeqCst),
                    "application cleanup ran before server drain"
                );
                Ok(())
            },
        ),
    )
    .await??;

    assert!(bootstrap_dropped.load(Ordering::SeqCst));
    assert!(server_completed.load(Ordering::SeqCst));
    Ok(())
}
