//! 程序启动入口。这里只初始化日志、解析参数，再把具体工作交给 `cli` 模块。

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
    // parse() 来自 clap::Parser；await 等待命令执行结束，Result 将错误交给运行环境输出。
    a2alinker::cli::execute(a2alinker::cli::Cli::parse()).await
}
