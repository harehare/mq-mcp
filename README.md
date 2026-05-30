<h1 align="center">mq-mcp</h1>

[![ci](https://github.com/harehare/mq-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/harehare/mq-mcp/actions/workflows/ci.yml)
[![mq language](https://img.shields.io/badge/mq-language-orange.svg)](https://github.com/harehare/mq)


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

## License

This project is licensed under the MIT License
