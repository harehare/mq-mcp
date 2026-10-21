<div align="center">
  <img src="assets/logo.svg" style="width: 128px; height: 128px;"/>

<h1>mq-mcp</h1>

[![ci](https://github.com/harehare/mq-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/harehare/mq-mcp/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/mq-mcp.svg)](https://crates.io/crates/mq-mcp)
[![license](https://img.shields.io/github/license/harehare/mq-mcp.svg)](./LICENSE)
[![release](https://img.shields.io/github/v/release/harehare/mq-mcp.svg)](https://github.com/harehare/mq-mcp/releases)

</div>


Model Context Protocol (MCP) server implementation for [mq](https://github.com/harehare/mq). This crate provides an MCP server that allows AI assistants to process Markdown and HTML content using mq's query language.

## Installation

You can install mq-mcp using the installation script:

```bash
curl -fsSL https://raw.githubusercontent.com/harehare/mq-mcp/main/bin/install.sh | bash
```

Or clone this repository and run the install script:

```bash
git clone https://github.com/harehare/mq-mcp.git
cd mq-mcp
./bin/install.sh
```

The script will:
- Install the `mq` binary to `~/.local/bin`
- Add `~/.local/bin` to your PATH (if not already present)
- Support macOS, Linux, and Windows
- Verify checksums for security

After installation, restart your terminal or run:

```bash
source ~/.zshrc  # or ~/.bashrc for bash users
```

## Implementation

The server implements the following MCP tools:

### Query Tools

- `html_to_markdown`: Converts HTML to Markdown and executes an mq query
- `extract_markdown`: Executes a custom mq query on Markdown content
- `extract_markdown_batch`: Executes the same mq query against multiple Markdown documents in one call, returning a JSON array with each document's index, optional id, and results (or a per-document error, without aborting the rest)

### Selector Tools

These tools apply a fixed mq selector to Markdown content:

| Tool | mq selector | Description |
|------|-------------|-------------|
| `extract_headings` | `.h` | All headings (h1–h6) |
| `extract_code_blocks` | `.code` | All fenced code blocks |
| `extract_todos` | `.todo` | Unchecked task list items |
| `extract_done_tasks` | `.done` | Checked task list items |
| `extract_links` | `.link` | All links |
| `extract_images` | `.image` | All images |
| `extract_tables` | `.table` | All table cells |
| `extract_text` | `.text` | Paragraph text nodes |
| `extract_blockquotes` | `.blockquote` | All blockquotes |

### Section Tools

These tools use the mq [section module](https://mqlang.org/book/start/example.html) to operate on document sections (heading + body):

| Tool | Description |
|------|-------------|
| `extract_sections` | Split document into all sections and return as Markdown |
| `extract_section` | Extract a single section by title (partial, case-sensitive match) |
| `extract_toc` | Generate an indented table of contents from headings |

### Discovery Tools

- `available_functions`: Returns available mq functions with descriptions and parameters
- `available_selectors`: Returns available mq selectors with descriptions

### Tool Parameters

#### html_to_markdown

- `html` (string): HTML content to process
- `query` (optional string): mq query to execute (default: `identity()`)
- `timeout_ms` (optional integer): timeout for this call, capped by `--max-timeout`

#### extract_markdown

- `markdown` (string): Markdown content to process
- `query` (string): mq query to execute
- `timeout_ms` (optional integer): timeout for this call, capped by `--max-timeout`

#### extract_markdown_batch

- `documents` (array): Documents to run the same query against, each `{ id?: string, markdown: string }` — `id` is echoed back in the result to identify which document a row came from (e.g. a file path)
- `query` (string): mq query to execute against every document

Returns one JSON text block: an array of `{ index, id?, results?, error? }`, in the same order as `documents`. A query failure on one document is reported in that entry's `error` field rather than failing the whole call.

#### extract_headings / extract_code_blocks / extract_todos / extract_done_tasks / extract_links / extract_images / extract_tables / extract_text / extract_blockquotes

- `markdown` (string): Markdown content to process

#### extract_sections / extract_toc

- `markdown` (string): Markdown content to process

#### extract_section

- `markdown` (string): Markdown content to process
- `title` (string): Section heading text to match (partial, case-sensitive)

#### available_functions

- `filter` (optional string): case-insensitive substring matched against function names and descriptions

#### available_selectors

No parameters.

## Query limits

Every query runs with a timeout so a runaway query can't hang the server.

| Option | Env var | Default | Description |
|--------|---------|---------|-------------|
| `--timeout <ms>` | `MQ_MCP_TIMEOUT_MS` | `10000` | Timeout applied when a tool call doesn't specify one |
| `--max-timeout <ms>` | `MQ_MCP_MAX_TIMEOUT_MS` | `60000` | Upper bound for a per-call `timeout_ms` (larger values are clamped) |
| `--max-call-stack-depth <n>` | `MQ_MCP_MAX_CALL_STACK_DEPTH` | mq-lang default | Maximum call stack depth of the mq evaluator |

`html_to_markdown`, `extract_markdown` and `extract_markdown_batch` accept an
optional `timeout_ms` to override the default for a single call.

Query failures (syntax errors, timeouts) are returned as tool results with
`isError: true` and a hint, so the calling model can see the message and fix its query.

## Transports

By default `mq-mcp` speaks MCP over stdio, for use as a local subprocess. It can
also run as a remote server using the
[Streamable HTTP](https://modelcontextprotocol.io/specification/2025-06-18/basic/transports#streamable-http)
transport:

```bash
mq-mcp --http                          # binds 127.0.0.1:8080, serves at /mcp
mq-mcp --http --bind 0.0.0.0:8080      # listen on all interfaces
```

The MCP endpoint is available at `http://<bind>/mcp`.

For security, Streamable HTTP validates the incoming `Host` header and only
accepts loopback hosts (`localhost`, `127.0.0.1`, `::1`) by default, to guard
against DNS rebinding. If you place `mq-mcp` behind a reverse proxy or expose
it under a real hostname, add that hostname with `--allowed-host`:

```bash
mq-mcp --http --bind 0.0.0.0:8080 --allowed-host mcp.example.com
```

By default, `mq-mcp --http` has no authentication — for anything beyond local
loopback use, either put it behind a reverse proxy that handles TLS and
access control, or use the built-in bearer token check with `--bearer-token`
(or, to keep the token out of the process list, the `MQ_MCP_BEARER_TOKEN`
env var):

```bash
mq-mcp --http --bind 0.0.0.0:8080 --bearer-token "$(openssl rand -hex 32)"
# or
MQ_MCP_BEARER_TOKEN=... mq-mcp --http --bind 0.0.0.0:8080
```

Every request to `/mcp` must then include `Authorization: Bearer <token>`, or
it's rejected with `401`. This is a single static token, not per-client
auth — for that, an auth-checking reverse proxy is still the better fit.

## Configuration

### Claude Desktop

#### Using mq-mcp binary directly

Add to `~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "mq-mcp": {
      "command": "~/.local/bin/mq-mcp",
      "args": []
    }
  }
}
```

Or simply use `mq-mcp` if `~/.local/bin` is in your PATH:

```json
{
  "mcpServers": {
    "mq-mcp": {
      "command": "mq-mcp",
      "args": []
    }
  }
}
```

#### Using mq command

Add to `~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "mq-mcp": {
      "command": "~/.local/bin/mq",
      "args": ["mcp"]
    }
  }
}
```

Or simply use `mq` if `~/.local/bin` is in your PATH:

```json
{
  "mcpServers": {
    "mq-mcp": {
      "command": "mq",
      "args": ["mcp"]
    }
  }
}
```

### VS Code with MCP Extension

#### Using mq-mcp binary directly

Add to `.vscode/settings.json`:

```json
{
  "mcp": {
    "servers": {
      "mq-mcp": {
        "type": "stdio",
        "command": "mq-mcp",
        "args": []
      }
    }
  }
}
```

#### Using mq command

Add to `.vscode/settings.json`:

```json
{
  "mcp": {
    "servers": {
      "mq-mcp": {
        "type": "stdio",
        "command": "mq",
        "args": ["mcp"]
      }
    }
  }
}
```

### Remote MCP (Streamable HTTP)

Start the server with `mq-mcp --http` (see [Transports](#transports)), then point any
MCP client that supports remote/HTTP servers at it:

```json
{
  "mcp": {
    "servers": {
      "mq-mcp": {
        "type": "http",
        "url": "http://127.0.0.1:8080/mcp"
      }
    }
  }
}
```

## License

This project is licensed under the MIT License
