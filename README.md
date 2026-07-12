<h1 align="center">mq-mcp</h1>

[![ci](https://github.com/harehare/mq-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/harehare/mq-mcp/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/mq-mcp.svg)](https://crates.io/crates/mq-mcp)
[![license](https://img.shields.io/github/license/harehare/mq-mcp.svg)](./LICENSE)
[![release](https://img.shields.io/github/v/release/harehare/mq-mcp.svg)](https://github.com/harehare/mq-mcp/releases)


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

### Database Tools

These tools query a persistent [`mq-db`](https://github.com/harehare/mq-db)
knowledge base instead of inline content — only available when `mq-mcp` is
started with `--db <path>` (see [Database mode](#database-mode) below).

| Tool | Description |
|------|-------------|
| `db_sql` | Run a read-only SQL query against the database |
| `db_mq` | Run an mq program against every document in the database |
| `db_list_documents` | List indexed documents (id, path, title, tags, block count) |
| `db_stats` | Block-type / code-language statistics |
| `db_index` | (Re-)index files/directories into the database and persist it |

Write-back (`UPDATE`/`DELETE` that edits source Markdown) is intentionally
not exposed here — it's a CLI/library-only feature in `mq-db` gated behind
an explicit `--write-back` flag, since an MCP tool call can be triggered
autonomously by an agent without a human confirming each one.

### Tool Parameters

#### html_to_markdown

- `html` (string): HTML content to process
- `query` (optional string): mq query to execute (default: `identity()`)

#### extract_markdown

- `markdown` (string): Markdown content to process
- `query` (string): mq query to execute

#### extract_headings / extract_code_blocks / extract_todos / extract_done_tasks / extract_links / extract_images / extract_tables / extract_text / extract_blockquotes

- `markdown` (string): Markdown content to process

#### extract_sections / extract_toc

- `markdown` (string): Markdown content to process

#### extract_section

- `markdown` (string): Markdown content to process
- `title` (string): Section heading text to match (partial, case-sensitive)

#### available_functions / available_selectors

No parameters.

#### db_sql

- `query` (string): SQL query to run (`SELECT`, `CREATE TABLE`, `INSERT INTO`, `DROP TABLE`, `DESC`, `SHOW TABLES`)

#### db_mq

- `code` (string): mq program to run against every indexed document

#### db_list_documents / db_stats

No parameters.

#### db_index

- `paths` (array of strings): Markdown files or directories to (re)index
- `recursive` (optional bool): recursively walk directories (default: `false`)
- `prune` (optional bool): drop catalogued documents whose file no longer exists (default: `false`)

## Database mode

Pass `--db <path>` to load (or create, via `db_index`) a persistent
[`mq-db`](https://github.com/harehare/mq-db) store and enable the
[Database Tools](#database-tools) above:

```bash
mq-mcp --db knowledge.mq-db
```

Without `--db`, `db_*` tool calls return an error asking you to restart
with the flag — the rest of the tools (which operate on inline
markdown/HTML content) work either way.

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

`mq-mcp --http` has no built-in authentication — put it behind a reverse proxy
that handles TLS and access control (e.g. an API gateway, VPN, or an
auth-checking proxy) before exposing it beyond your local machine.

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

To enable the [Database Tools](#database-tools), add `--db <path>` to `args`:

```json
{
  "mcpServers": {
    "mq-mcp": {
      "command": "~/.local/bin/mq-mcp",
      "args": ["--db", "/absolute/path/to/knowledge.mq-db"]
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
