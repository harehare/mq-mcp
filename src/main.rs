pub mod server;

use std::time::Duration;

use clap::Parser;
use server::{EngineConfig, HttpConfig};
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

    /// Require `Authorization: Bearer <token>` on every /mcp request (HTTP transport only).
    #[arg(long = "bearer-token", env = "MQ_MCP_BEARER_TOKEN")]
    bearer_token: Option<String>,

    /// Default timeout for a query, in milliseconds
    #[arg(long, env = "MQ_MCP_TIMEOUT_MS", default_value_t = server::DEFAULT_TIMEOUT_MS)]
    timeout: u64,

    /// Largest timeout a tool call may request via `timeout_ms`, in milliseconds
    #[arg(long = "max-timeout", env = "MQ_MCP_MAX_TIMEOUT_MS", default_value_t = server::DEFAULT_MAX_TIMEOUT_MS)]
    max_timeout: u64,

    /// Maximum call stack depth for the mq evaluator (mq-lang default if omitted)
    #[arg(long = "max-call-stack-depth", env = "MQ_MCP_MAX_CALL_STACK_DEPTH")]
    max_call_stack_depth: Option<u32>,
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
    let engine = EngineConfig {
        timeout: Duration::from_millis(cli.timeout),
        max_timeout: Duration::from_millis(cli.max_timeout),
        max_call_stack_depth: cli.max_call_stack_depth,
    };

    if cli.http {
        server::start_http(
            HttpConfig {
                bind: cli.bind,
                allowed_hosts: cli.allowed_hosts,
                bearer_token: cli.bearer_token,
            },
            engine,
        )
        .await
    } else {
        server::start(engine).await
    }
}
