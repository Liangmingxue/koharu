use std::future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
async fn shutdown_signal_runs_application_shutdown_and_awaits_server_once() -> Result<()> {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let shutdown_count = Arc::new(AtomicUsize::new(0));
    let app_stopped = Arc::new(AtomicBool::new(false));
    let server_completed = Arc::new(AtomicBool::new(false));
    let server_completed_in_task = server_completed.clone();
    let app_stopped_for_server = app_stopped.clone();
    let server = tokio::spawn(async move {
        shutdown_rx
            .await
            .map_err(|_| anyhow!("shutdown sender dropped"))?;
        while !app_stopped_for_server.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        server_completed_in_task.store(true, Ordering::SeqCst);
        Ok(())
    });
    let app_stopped_in_shutdown = app_stopped.clone();
    let shutdown_count_in_signal = shutdown_count.clone();

    tokio::time::timeout(
        Duration::from_secs(1),
        supervise_headless_server(
            server,
            shutdown_tx,
            async { Ok(()) },
            async { Ok(()) },
            async move {
                shutdown_count_in_signal.fetch_add(1, Ordering::SeqCst);
                app_stopped_in_shutdown.store(true, Ordering::SeqCst);
                Ok(())
            },
        ),
    )
    .await??;

    assert_eq!(shutdown_count.load(Ordering::SeqCst), 1);
    assert!(server_completed.load(Ordering::SeqCst));
    Ok(())
}

#[tokio::test]
async fn cancelled_shutdown_receiver_aborts_unstoppable_server() -> Result<()> {
    let server = tokio::spawn(future::pending::<Result<()>>());
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    drop(shutdown_rx);

    let error = tokio::time::timeout(
        Duration::from_secs(1),
        supervise_headless_server(
            server,
            shutdown_tx,
            async { Ok(()) },
            async { Ok(()) },
            async { Ok(()) },
        ),
    )
    .await?
    .expect_err("cancelled receiver must fail shutdown");

    assert!(format!("{error:#}").contains("shutdown receiver"));
    Ok(())
}
