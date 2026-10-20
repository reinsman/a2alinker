use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "a2alinker=info".into()),
        )
        .init();
    a2alinker::cli::execute(a2alinker::cli::Cli::parse()).await
}
