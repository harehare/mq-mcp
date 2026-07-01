pub mod server;

use clap::Parser;
use server::HttpConfig;
use tracing_subscriber::EnvFilter;

/// Model Context Protocol server for mq
#[derive(Debug, Parser)]
#[command(name = "mq-mcp", version)]
struct Cli {
    /// Serve over Streamable HTTP instead of stdio (remote MCP)
    #[arg(long)]
    http: bool,

    /// Address to bind the HTTP server to
    #[arg(long, default_value = "127.0.0.1:8080")]
    bind: String,

    /// Additional Host header value to accept (repeatable); needed when the
    /// server is reached under a non-loopback hostname
    #[arg(long = "allowed-host")]
    allowed_hosts: Vec<String>,
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_thread_names(true)
        .with_target(true)
        .with_line_number(true)
        .init();

    let cli = Cli::parse();

    if cli.http {
        server::start_http(HttpConfig {
            bind: cli.bind,
            allowed_hosts: cli.allowed_hosts,
        })
        .await
    } else {
        server::start().await
    }
}
