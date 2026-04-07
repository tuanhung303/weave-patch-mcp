use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};
use weave_patch_mcp::server;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Handle --version before setting up tracing/stdin
    let args: Vec<String> = std::env::args().collect();
    if args.contains(&"--version".to_string()) || args.contains(&"-V".to_string()) {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // Setup tracing to file in ~/.weave-patch/logs/
    let log_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".weave-patch/logs");
    std::fs::create_dir_all(&log_dir)?;
    let file_appender = RollingFileAppender::new(Rotation::DAILY, log_dir, "weave-patch.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(non_blocking))
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    tracing::info!("weave-patch starting");
    server::run().await
}
