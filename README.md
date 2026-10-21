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
- `extract_markdown_batch`: Executes the same mq query against multiple Markdown documents (inline or by file `path`) in one call, returning each document's index, optional id, results and total (or a per-document error, without aborting the rest)

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

### Document Tools

These tools help a model work out what to read before reading it:

| Tool | Description |
|------|-------------|
| `document_outline` | Document structure without its content: totals (characters, lines, code blocks by language, tables, links, images, todos), front matter format, and every heading with its line range, size in characters and number of subsections |
| `extract_frontmatter` | YAML or TOML front matter as parsed JSON, plus its raw text (`found: false` if there is none) |
| `validate_query` | Checks an mq query without running it: syntax errors and unknown functions, selectors or modules, with line/column and a "did you mean" suggestion. Pass a sample (`markdown` or `path`) to also dry-run it and get the match count and first results |
| `list_files` | Lists the files the server may read through `path` (see [Reading files](#reading-files)) |

`document_outline`, `extract_frontmatter`, `validate_query`, `list_files` and
`extract_markdown_batch` declare an `outputSchema` and return `structuredContent`.

### Discovery Tools

- `available_functions`: Returns available mq functions with descriptions and parameters
- `available_selectors`: Returns available mq selectors with descriptions

### Tool Parameters

Every tool that takes a Markdown document accepts it either inline as
`markdown` or as a file `path` (see [Reading files](#reading-files)) — give
exactly one. `html_to_markdown` does the same with `html`. The examples below
list `markdown`; `path` is always an alternative to it.

Tools that return a list of results also accept the options described in
[Output controls](#output-controls).

#### html_to_markdown

- `html` (string): HTML content to process
- `query` (optional string): mq query to execute (default: `identity()`)
- `timeout_ms` (optional integer): timeout for this call, capped by `--max-timeout`

#### extract_markdown

- `markdown` (string): Markdown content to process
- `query` (string): mq query to execute
- `timeout_ms` (optional integer): timeout for this call, capped by `--max-timeout`

#### extract_markdown_batch

- `documents` (array): Documents to run the same query against, each `{ id?: string, markdown?: string, path?: string }` — `id` is echoed back in the result to identify which document a row came from (it defaults to `path`)
- `query` (string): mq query to execute against every document
- `timeout_ms` (optional integer): timeout for this call, capped by `--max-timeout`
- `limit` (optional integer): at most this many results per document
- `max_output_chars` (optional integer): cap on the characters returned across all documents

Returns `{ "documents": [{ index, id?, results, total, next_offset?, error? }] }`, in the same order as `documents`. A query or file error on one document is reported in that entry's `error` field rather than failing the whole call. `next_offset` is set when results were left out because of `limit` or `max_output_chars`.

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

#### document_outline

- `markdown` / `path`: the document
- `max_depth` (optional integer): list only headings up to this depth (counts in `stats` still cover the whole document)

#### extract_frontmatter

- `markdown` / `path`: the document

#### validate_query

- `query` (string): mq query to check
- `markdown` / `path` (optional): sample document for a dry run
- `timeout_ms` (optional integer): timeout for the dry run

#### list_files

- `directory` (optional string): directory to list, inside an allowed directory (default: all of them)
- `extensions` (optional array of strings): extensions to include, without the dot (default: `md`, `markdown`, `mdx`)
- `limit` (optional integer): at most this many files (default 200, maximum 1000)

## Reading files

Passing a large document inline means the model has to write it out in full,
and the same text is paid for twice (once read, once sent). With file access
enabled, tools take a `path` instead and the document never passes through the
conversation.

File access is **off by default**. Allow one or more directories with `--root`:

```bash
mq-mcp --root ~/notes --root ~/projects/docs
MQ_MCP_ROOTS=~/notes:~/projects/docs mq-mcp     # ':' separated (';' on Windows)
```

| Option | Env var | Default | Description |
|--------|---------|---------|-------------|
| `--root <dir>` | `MQ_MCP_ROOTS` | none (disabled) | Directory the `path` parameter may read from (repeatable) |
| `--max-file-size <bytes>` | `MQ_MCP_MAX_FILE_SIZE` | `10485760` | Largest file that can be read |

- A `path` is accepted only if it resolves to a file inside a root. Relative
  paths are tried against each root in turn; absolute paths work too.
  `..` and symlinks are resolved *before* the check, so neither can escape a root.
- A missing file and a file outside the roots give the same error, so paths
  outside the roots can't be probed for existence.
- Files are read-only, must be UTF-8 text, and must be under `--max-file-size`.
  Any text file under a root can be read, not only `.md` — don't point `--root`
  at a directory holding secrets.
- mq queries cannot read files themselves: functions such as `read_file` and
  `env` are denied inside queries, whatever the roots are.
- `list_files` shows what is available; it skips dot-directories, `node_modules`,
  `target` and symlinks. When file access is enabled, the roots are also
  announced in the server instructions.

## Output controls

Tools that return a list of results (`html_to_markdown`, `extract_markdown`,
`extract_*`) accept:

| Parameter | Description |
|-----------|-------------|
| `format` | `"text"` (default): one Markdown block per result. `"json"`: a single structured document (below) |
| `offset` | Skip this many results (paging) |
| `limit` | Return at most this many results |
| `max_output_chars` | Cap the characters returned, up to the server's `--max-output-chars` |

Results beyond `limit`, or beyond the character cap, are left out, and the
response says how to get them (`call again with offset=N`). A single result
larger than the cap is cut and marked `[truncated]`. Nothing is added when
everything fits.

With `format: "json"` the result is `structuredContent` (also sent as text):

```json
{
  "results": [
    {
      "type": "code",
      "text": "```rust\nfn main() {}\n```",
      "value": "fn main() {}",
      "lang": "rust",
      "position": { "start": { "line": 3, "column": 1 }, "end": { "line": 5, "column": 4 } }
    }
  ],
  "total": 1, "offset": 0, "returned": 1, "next_offset": null
}
```

`type` is the node type (`h1`…`h6`, `code`, `link`, `image`, `list`, `table_cell`, …);
type-specific attributes such as `depth`, `lang`, `url`, `title`, `alt`, `checked`,
`row` and `column` are included when present, and `position` gives 1-based
line/column of the node in the source document.

| Option | Env var | Default | Description |
|--------|---------|---------|-------------|
| `--max-output-chars <n>` | `MQ_MCP_MAX_OUTPUT_CHARS` | `100000` | Most characters one tool call returns |

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
`isError: true` and a hint, so the calling model can see the message and fix its query
(`validate_query` pinpoints the problem without running anything).

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
