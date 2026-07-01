pub mod server;

use server::HttpConfig;
use tracing_subscriber::EnvFilter;

fn print_help() {
    println!(
        "mq-mcp - Model Context Protocol server for mq

USAGE:
    mq-mcp [OPTIONS]

OPTIONS:
    --http                   Serve over Streamable HTTP instead of stdio (remote MCP)
    --bind <ADDR>            Address to bind the HTTP server to [default: 127.0.0.1:8080]
    --allowed-host <HOST>    Additional Host header value to accept (repeatable); needed
                             when the server is reached under a non-loopback hostname
    -h, --help               Print this help message"
    );
}

struct Args {
    http: bool,
    bind: String,
    allowed_hosts: Vec<String>,
}

fn parse_args() -> miette::Result<Option<Args>> {
    let mut http = false;
    let mut bind = "127.0.0.1:8080".to_string();
    let mut allowed_hosts = Vec::new();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--http" => http = true,
            "--bind" => {
                i += 1;
                bind = args
                    .get(i)
                    .cloned()
                    .ok_or_else(|| miette::miette!("--bind requires a value"))?;
            }
            "--allowed-host" => {
                i += 1;
                allowed_hosts.push(
                    args.get(i)
                        .cloned()
                        .ok_or_else(|| miette::miette!("--allowed-host requires a value"))?,
                );
            }
            "-h" | "--help" => {
                print_help();
                return Ok(None);
            }
            other => return Err(miette::miette!("Unknown argument: {other}")),
        }
        i += 1;
    }

    Ok(Some(Args {
        http,
        bind,
        allowed_hosts,
    }))
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_thread_names(true)
        .with_target(true)
        .with_line_number(true)
        .init();

    let Some(args) = parse_args()? else {
        return Ok(());
    };

    if args.http {
        server::start_http(HttpConfig {
            bind: args.bind,
            allowed_hosts: args.allowed_hosts,
        })
        .await
    } else {
        server::start().await
    }
}
