use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use koharu_rpc::BootstrapManager;
use koharu_rpc::server::{
    AssetResolver, serve_with_listener_and_assets, serve_with_listener_and_assets_until_shutdown,
};
use koharu_runtime::{ComputePolicy, RuntimeManager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

#[tokio::test]
async fn graceful_shutdown_completes_after_serving_and_releases_listener() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(2), graceful_shutdown_scenario()).await??;
    Ok(())
}

async fn graceful_shutdown_scenario() -> Result<()> {
    let data_root = tempfile::tempdir()?;
    let runtime = Arc::new(RuntimeManager::new(
        data_root.path(),
        ComputePolicy::CpuOnly,
    )?);
    let app = BootstrapManager::new(runtime);
    let assets: AssetResolver = Arc::new(|path| {
        (path == "health.txt").then(|| (b"ready".to_vec(), "text/plain".to_string()))
    });

    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let address = listener.local_addr()?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let serve_task = tokio::spawn(serve_with_listener_and_assets_until_shutdown(
        listener,
        app,
        assets,
        async move {
            let _ = shutdown_rx.await;
        },
    ));

    let mut connection = TcpStream::connect(address).await?;
    connection
        .write_all(b"GET /health.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await?;
    let mut response = Vec::new();
    connection.read_to_end(&mut response).await?;
    let response = String::from_utf8(response)?;
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(response.ends_with("\r\n\r\nready"));

    shutdown_tx
        .send(())
        .expect("serve task still owns shutdown");
    serve_task.await??;

    let reopened = TcpListener::bind(address).await?;
    assert_eq!(reopened.local_addr()?, address);
    Ok(())
}

#[tokio::test]
async fn desktop_asset_server_remains_available_until_its_runtime_stops() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(2), async {
        let data_root = tempfile::tempdir()?;
        let runtime = Arc::new(RuntimeManager::new(
            data_root.path(),
            ComputePolicy::CpuOnly,
        )?);
        let app = BootstrapManager::new(runtime);
        let assets: AssetResolver = Arc::new(|path| {
            (path == "desktop.txt").then(|| (b"desktop".to_vec(), "text/plain".to_string()))
        });
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let address = listener.local_addr()?;
        let serve_task = tokio::spawn(serve_with_listener_and_assets(listener, app, assets));

        let mut connection = TcpStream::connect(address).await?;
        connection
            .write_all(b"GET /desktop.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await?;
        let mut response = Vec::new();
        connection.read_to_end(&mut response).await?;
        let response = String::from_utf8(response)?;
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.ends_with("\r\n\r\ndesktop"));

        serve_task.abort();
        assert!(
            serve_task
                .await
                .expect_err("aborted task must be cancelled")
                .is_cancelled()
        );
        let reopened = TcpListener::bind(address).await?;
        assert_eq!(reopened.local_addr()?, address);
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}
