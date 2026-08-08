#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// 整个程序最外层入口：

use koharu::app;
use koharu::panic;
use koharu::sentry;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _guard = sentry::initialize();
    panic::install();
    app::run().await
}
