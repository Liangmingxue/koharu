use clap::Parser;

#[derive(Parser)]
#[command(version = crate::version::APP_VERSION, about)]
pub(crate) struct Cli {
    #[arg(short, long, help = "Download dynamic libraries and exit")]
    pub(crate) download: bool,
    #[arg(long, help = "Force CPU even if GPU is available")]
    pub(crate) cpu: bool,
    #[arg(short, long, value_name = "PORT", help = "Bind to a specific port")]
    pub(crate) port: Option<u16>,
    #[arg(
        long,
        help = "Bind the HTTP service to a specific host instead of 127.0.0.1"
    )]
    pub(crate) host: Option<String>,
    #[arg(long, help = "Run without GUI")]
    pub(crate) headless: bool,
    #[arg(long, help = "Enable debug console output")]
    pub(crate) debug: bool,
}

// --download    下载运行库后退出
// --cpu         强制只使用 CPU   ,  后续这里改成使用GPU
// --port        指定 HTTP 端口
// --host        指定监听地址
// --headless    不启动桌面窗口
// --debug       开启调试控制台
// koharu --headless --host 127.0.0.1 --port 4000
