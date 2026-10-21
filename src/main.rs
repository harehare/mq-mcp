use std::{path::PathBuf, time::Duration};

use clap::Parser;
use miette::miette;
use mq_mcp::{
    FileAccess,
    files::DEFAULT_MAX_FILE_SIZE,
    output::DEFAULT_MAX_OUTPUT_CHARS,
    server::{self, EngineConfig, HttpConfig},
};
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

    /// Allow tools to read files under this directory through their `path` parameter
    /// (repeatable; separate several in MQ_MCP_ROOTS with ':', or ';' on Windows).
    /// Without any root, `path` is rejected and only inline content can be processed.
    #[cfg_attr(
        not(windows),
        arg(long = "root", env = "MQ_MCP_ROOTS", value_delimiter = ':')
    )]
    #[cfg_attr(
        windows,
        arg(long = "root", env = "MQ_MCP_ROOTS", value_delimiter = ';')
    )]
    roots: Vec<PathBuf>,

    /// Largest file that can be read through `path`, in bytes
    #[arg(long = "max-file-size", env = "MQ_MCP_MAX_FILE_SIZE", default_value_t = DEFAULT_MAX_FILE_SIZE)]
    max_file_size: u64,

    /// Largest number of characters a single tool call may return
    #[arg(long = "max-output-chars", env = "MQ_MCP_MAX_OUTPUT_CHARS", default_value_t = DEFAULT_MAX_OUTPUT_CHARS)]
    max_output_chars: usize,
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
        max_output_chars: cli.max_output_chars,
        files: FileAccess::new(cli.roots, cli.max_file_size).map_err(|e| miette!(e))?,
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
