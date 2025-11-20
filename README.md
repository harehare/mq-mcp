<h1 align="center">mq-mcp</h1>

[![ci](https://github.com/harehare/mq-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/harehare/mq-mcp/actions/workflows/ci.yml)

Model Context Protocol (MCP) server implementation for mq. This crate provides an MCP server that allows AI assistants to process Markdown and HTML content using mq's query language.

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
- Install the `mq` binary to `~/.mq/bin`
- Add `~/.mq/bin` to your PATH (if not already present)
- Support macOS, Linux, and Windows
- Verify checksums for security

After installation, restart your terminal or run:

```bash
source ~/.zshrc  # or ~/.bashrc for bash users
```

## Implementation

The server implements four MCP tools:

- `html_to_markdown`: Converts HTML to Markdown and executes mq queries
- `extract_markdown`: Executes mq queries on Markdown content
- `available_functions`: Returns available functions for mq queries
- `available_selectors`: Returns available selectors for mq queries

### Tool Parameters

#### html_to_markdown

- `html` (string): HTML content to process
- `query` (optional string): mq query to execute

#### extract_markdown

- `markdown` (string): Markdown content to process
- `query` (string): mq query to execute

#### available_functions

No parameters. Returns JSON with function names, descriptions, parameters, and example queries.

#### available_selectors

No parameters. Returns JSON with selector names, descriptions, and parameters.

## Configuration

### Claude Desktop

Add to `~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "mq-mcp": {
      "command": "/Users/YOUR_USERNAME/.mq/bin/mq",
      "args": ["mcp"]
    }
  }
}
```

Or simply use `mq` if `~/.mq/bin` is in your PATH:

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
