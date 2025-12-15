pub mod server;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> miette::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_thread_names(true)
        .with_target(true)
        .with_line_number(true)
        .init();

    server::start().await
}
