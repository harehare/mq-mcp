pub mod server;

#[tokio::main]
async fn main() -> miette::Result<()> {
    tracing_subscriber::fmt().init();
    server::start().await
}
