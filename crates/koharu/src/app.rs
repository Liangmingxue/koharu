//! Binary entry point. Wires `koharu-app::App` to the axum router plus
//! (optionally) Tauri.

use std::future::Future;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use koharu_app::{App, AppConfig, config as app_config};
use koharu_rpc::{BootstrapManager, server};
use koharu_runtime::{ComputePolicy, RuntimeHttpConfig, RuntimeManager};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::cli::Cli;

async fn bootstrap_app(
    state: Arc<BootstrapManager>,
    config: AppConfig,
    cpu_only: bool,
    mut cancellation: Option<watch::Receiver<bool>>,
) -> Result<()> {
    let runtime = state.runtime();
    {
        let prepare = runtime.prepare();
        tokio::pin!(prepare);
        if let Some(cancellation) = cancellation.as_mut() {
            tokio::select! {
                biased;
                result = &mut prepare => result.context("failed to prepare runtime")?,
                changed = cancellation.changed() => {
                    if changed.is_ok() && *cancellation.borrow() {
                        return Ok(());
                    }
                    prepare.await.context("failed to prepare runtime")?;
                }
            }
        } else {
            prepare.await.context("failed to prepare runtime")?;
        }
    }

    if cancellation
        .as_ref()
        .is_some_and(|cancellation| *cancellation.borrow())
    {
        return Ok(());
    }

    let shared_state = state.shared_state();
    let app = tokio::task::spawn_blocking(move || {
        App::new_with_shared_state(
            config,
            runtime,
            cpu_only,
            shared_state,
            crate::version::current(),
        )
        .map(Arc::new)
    })
    .await
    .context("application bootstrap task failed")??;

    if cancellation
        .as_ref()
        .is_some_and(|cancellation| *cancellation.borrow())
    {
        return Ok(());
    }

    koharu_llm::suppress_native_logs();
    app.spawn_llm_forwarder();
    state
        .set_app(app)
        .map_err(|_| anyhow::anyhow!("app already initialized"))?;
    Ok(())
}

pub async fn supervise_headless_server<
    Bootstrap,
    RequestBootstrapCancellation,
    ShutdownSignal,
    ApplicationShutdown,
>(
    mut server_handle: JoinHandle<Result<()>>,
    shutdown_tx: oneshot::Sender<()>,
    bootstrap: Bootstrap,
    request_bootstrap_cancellation: RequestBootstrapCancellation,
    shutdown_signal: ShutdownSignal,
    application_shutdown: ApplicationShutdown,
) -> Result<()>
where
    Bootstrap: Future<Output = Result<()>>,
    RequestBootstrapCancellation: FnOnce(),
    ShutdownSignal: Future<Output = Result<()>>,
    ApplicationShutdown: Future<Output = Result<()>>,
{
    let mut bootstrap = Box::pin(bootstrap);
    let mut shutdown_signal = Box::pin(shutdown_signal);
    let mut request_bootstrap_cancellation = Some(request_bootstrap_cancellation);
    let initial_event = tokio::select! {
        result = &mut server_handle => HeadlessEvent::Server(result),
        result = &mut shutdown_signal => HeadlessEvent::Shutdown(result),
        result = &mut bootstrap => HeadlessEvent::Bootstrap(result),
    };

    let (bootstrap_result, lifecycle_result) = match initial_event {
        HeadlessEvent::Server(result) => {
            request_bootstrap_cancellation
                .take()
                .expect("bootstrap cancellation is requested at most once")();
            let bootstrap_result = bootstrap.await;
            drop(shutdown_signal);
            let application_result = application_shutdown.await;
            premature_server_result(result)?;
            application_result?;
            return bootstrap_result;
        }
        HeadlessEvent::Shutdown(result) => {
            request_bootstrap_cancellation
                .take()
                .expect("bootstrap cancellation is requested at most once")();
            let bootstrap_result = bootstrap.await;
            drop(shutdown_signal);
            (bootstrap_result, result)
        }
        HeadlessEvent::Bootstrap(Ok(())) => {
            drop(bootstrap);
            let event = tokio::select! {
                result = &mut server_handle => HeadlessEvent::Server(result),
                result = &mut shutdown_signal => HeadlessEvent::Shutdown(result),
            };
            drop(shutdown_signal);
            match event {
                HeadlessEvent::Server(result) => {
                    let application_result = application_shutdown.await;
                    premature_server_result(result)?;
                    application_result?;
                    return Ok(());
                }
                HeadlessEvent::Shutdown(result) => (Ok(()), result),
                HeadlessEvent::Bootstrap(_) => unreachable!(),
            }
        }
        HeadlessEvent::Bootstrap(Err(error)) => {
            drop(bootstrap);
            drop(shutdown_signal);
            (Err(error), Ok(()))
        }
    };

    let shutdown_receiver_closed = shutdown_tx.send(()).is_err();
    let server_result = server_handle.await;
    let application_result = application_shutdown.await;
    let server_result = server_result.context("server task failed")?;
    server_result?;
    application_result?;
    if shutdown_receiver_closed {
        anyhow::bail!("server shutdown receiver was cancelled");
    }
    bootstrap_result?;
    lifecycle_result
}

enum HeadlessEvent {
    Bootstrap(Result<()>),
    Shutdown(Result<()>),
    Server(std::result::Result<Result<()>, tokio::task::JoinError>),
}

fn premature_server_result(
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> Result<()> {
    match result {
        Ok(Ok(())) => anyhow::bail!("server stopped before headless shutdown"),
        Ok(Err(error)) => Err(error).context("server stopped before headless shutdown"),
        Err(error) => Err(error).context("server task failed before headless shutdown"),
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;
    use koharu_app::AppConfig;
    use koharu_rpc::BootstrapManager;
    use koharu_runtime::{ComputePolicy, RuntimeManager};
    use tokio::sync::watch;

    use super::bootstrap_app;

    #[tokio::test]
    async fn ready_runtime_prepare_error_wins_ready_cancellation() -> Result<()> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let tempdir = env::temp_dir().join(format!(
            "koharu-bootstrap-ready-error-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&tempdir)?;
        let invalid_root = tempdir.join("not-a-directory");
        fs::write(&invalid_root, b"file blocks runtime directory creation")?;
        let runtime = Arc::new(RuntimeManager::new(&invalid_root, ComputePolicy::CpuOnly)?);
        let state = BootstrapManager::new(runtime);

        for _ in 0..64 {
            let (cancel_tx, cancel_rx) = watch::channel(false);
            cancel_tx.send(true)?;

            let error = bootstrap_app(state.clone(), AppConfig::default(), true, Some(cancel_rx))
                .await
                .expect_err("a ready runtime prepare error must win ready cancellation");

            assert!(format!("{error:#}").contains("failed to prepare runtime"));
        }

        fs::remove_file(invalid_root)?;
        fs::remove_dir(tempdir)?;
        Ok(())
    }
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();

    #[cfg(target_os = "windows")]
    {
        let attached = crate::windows::attach_parent_console();
        if !attached && (cli.headless || cli.debug) {
            crate::windows::create_console_window();
        }
        crate::windows::enable_ansi_support().ok();
    }

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::filter::EnvFilter::builder()
                .with_default_directive(tracing::Level::INFO.into())
                .from_env_lossy(),
        )
        .with(crate::sentry::tracing_layer())
        .with(crate::tracing::TimingLayer::new())
        .init();

    let config: AppConfig = app_config::load()?;
    let http = RuntimeHttpConfig {
        connect_timeout_secs: config.http.connect_timeout.max(1),
        read_timeout_secs: config.http.read_timeout.max(1),
        max_retries: config.http.max_retries,
    };
    let compute = if cli.cpu {
        ComputePolicy::CpuOnly
    } else {
        ComputePolicy::PreferGpu
    };

    if cli.download {
        return RuntimeManager::new_with_http(config.data.path.as_std_path(), compute, http)?
            .prepare()
            .await
            .context("failed to download runtime packages");
    }

    let state = BootstrapManager::new(Arc::new(RuntimeManager::new_with_http(
        config.data.path.as_std_path(),
        compute,
        http,
    )?));
    state.spawn_download_forwarder();

    #[cfg(target_os = "windows")]
    crate::windows::register_khr().ok();

    let bind_host = cli.host.as_deref().unwrap_or("127.0.0.1");
    let bind_port = cli.port.unwrap_or(4000);
    let listener: TcpListener = if cfg!(debug_assertions) || cli.port.is_some() {
        TcpListener::bind((bind_host, bind_port)).await?
    } else {
        let mut port = bind_port;
        loop {
            match TcpListener::bind((bind_host, port)).await {
                Ok(listener) => break listener,
                Err(err) if err.kind() == std::io::ErrorKind::AddrInUse && port < u16::MAX => {
                    port += 1;
                }
                Err(err) => return Err(err.into()),
            }
        }
    };
    let port = listener.local_addr()?.port();
    tracing::info!(port, "starting server");

    let mut context = tauri::generate_context!();
    let assets = crate::assets::from_context(&mut context);

    if cli.headless {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_state = state.clone();
        let server_handle = tokio::spawn(server::serve_with_listener_and_assets_until_shutdown(
            listener,
            server_state,
            assets,
            async move {
                let _ = shutdown_rx.await;
            },
        ));
        tracing::info!(port, "headless: open http://127.0.0.1:{port}/ in a browser");
        let bootstrap_state = state.clone();
        let shutdown_state = state.clone();
        let (cancel_bootstrap_tx, cancel_bootstrap_rx) = watch::channel(false);
        return supervise_headless_server(
            server_handle,
            shutdown_tx,
            bootstrap_app(bootstrap_state, config, cli.cpu, Some(cancel_bootstrap_rx)),
            move || {
                let _ = cancel_bootstrap_tx.send(true);
            },
            async {
                tokio::signal::ctrl_c().await?;
                Ok(())
            },
            async move {
                if let Some(app) = shutdown_state.app() {
                    app.close_project()
                        .await
                        .context("failed to close project during shutdown")?;
                }
                Ok(())
            },
        )
        .await;
    }

    let server_state = state.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(error) =
            server::serve_with_listener_and_assets(listener, server_state, assets).await
        {
            tracing::error!(%error, "server stopped");
        }
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .setup(move |handle| {
            tauri::async_runtime::spawn(async move {
                bootstrap_app(state, config, cli.cpu, None)
                    .await
                    .expect("failed to bootstrap app");
            });

            let cfg = handle.config();
            let url: tauri::Url = if cfg!(debug_assertions) {
                cfg.build
                    .dev_url
                    .as_ref()
                    .expect("dev_url must be set in dev mode")
                    .as_str()
                    .parse()?
            } else {
                format!("http://127.0.0.1:{port}").parse()?
            };
            let wc = cfg
                .app
                .windows
                .iter()
                .find(|w| w.label == "main")
                .expect("main window config not found");
            tauri::webview::WebviewWindowBuilder::from_config(handle, wc)?
                .build()?
                .navigate(url)?;

            Ok(())
        })
        .run(context)?;

    Ok(())
}
