//! 二进制只处理日志、参数和进程信号；服务由 a2alinker 库提供。

mod cli;

use clap::Parser;

// 普通 Rust main 不能直接等待异步操作；此宏创建 Tokio 运行时来执行 async main。
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 日志使用 stderr，避免与命令的标准输出混在一起。RUST_LOG 可覆盖默认级别。
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "a2alinker=info".into()),
        )
        .init();
    let options = cli::Cli::parse().into_options()?;
    a2alinker::Server::bind(options).await?.run(signal()).await
}

async fn signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
