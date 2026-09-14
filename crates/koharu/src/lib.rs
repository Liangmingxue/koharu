// # 我使用的是linux 环境，后续使用vscode投射出去
// # 声明主程序可用的模块。

pub mod app;
pub mod assets;
pub mod cli;
pub mod panic;
pub mod sentry;
pub mod tracing;
pub mod version;
#[cfg(target_os = "windows")]
pub mod windows;
