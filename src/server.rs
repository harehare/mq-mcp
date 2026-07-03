use miette::miette;
use rmcp::{
    ErrorData, ServerHandler, ServiceExt,
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, ProtocolVersion, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use std::sync::Arc;
use tokio::io::{stdin, stdout};
type McpResult = Result<CallToolResult, ErrorData>;

#[derive(Debug, Clone, Default)]
pub struct Server {
    pub tool_router: ToolRouter<Self>,
}

#[derive(Debug, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct QueryForHtml {
    #[schemars(description = "The HTML to process")]
    html: String,
    #[schemars(
        description = "The mq query to execute. Selectors and functions listed in the available_selectors and available_functions tools can be used."
    )]
    query: Option<String>,
}

#[derive(Debug, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct QueryForMarkdown {
    #[schemars(description = "The markdown to process")]
    markdown: String,
    #[schemars(
        description = "The mq query to execute. Selectors and functions listed in the available_selectors and available_functions tools can be used ."
    )]
    query: String,
}

#[derive(Debug, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct MarkdownInput {
    #[schemars(description = "The markdown content to process")]
    markdown: String,
}

#[derive(Debug, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct ExtractSectionInput {
    #[schemars(description = "The markdown content to process")]
    markdown: String,
    #[schemars(description = "The section title to extract (partial match)")]
    title: String,
}

impl Server {
    fn eval_query(&self, markdown: &str, query: &str) -> McpResult {
        let mut engine = mq_lang::DefaultEngine::default();
        engine.load_builtin_module();

        let parsed = mq_markdown::Markdown::from_html_str(markdown).map_err(|e| {
            ErrorData::parse_error(
                "Failed to parse markdown",
                Some(serde_json::Value::String(e.to_string())),
            )
        })?;

        let values = engine
            .eval(
                query,
                parsed.nodes.into_iter().map(mq_lang::RuntimeValue::from),
            )
            .map_err(|e| {
                ErrorData::invalid_request(
                    "Failed to query",
                    Some(serde_json::Value::String(e.to_string())),
                )
            })?;

        Ok(CallToolResult::success(
            values
                .into_iter()
                .filter_map(|value| {
                    if value.is_none() || value.is_empty() {
                        None
                    } else {
                        Some(Content::text(value.to_string()))
                    }
                })
                .collect(),
        ))
    }

    fn eval_aggregate(&self, markdown: &str, query: &str) -> McpResult {
        let mut engine = mq_lang::DefaultEngine::default();
        engine.load_builtin_module();

        let parsed = mq_markdown::Markdown::from_html_str(markdown).map_err(|e| {
            ErrorData::parse_error(
                "Failed to parse markdown",
                Some(serde_json::Value::String(e.to_string())),
            )
        })?;

        let all_nodes: Vec<mq_lang::RuntimeValue> = parsed
            .nodes
            .into_iter()
            .map(mq_lang::RuntimeValue::from)
            .collect();
        let input = mq_lang::RuntimeValue::Array(all_nodes);

        let values = engine
            .eval(query, std::iter::once(input))
            .map_err(|e| {
                ErrorData::invalid_request(
                    "Failed to query",
                    Some(serde_json::Value::String(e.to_string())),
                )
            })?;

        Ok(CallToolResult::success(
            values
                .into_iter()
                .flat_map(|value| match value {
                    mq_lang::RuntimeValue::Array(arr) => arr
                        .into_iter()
                        .filter(|v| !v.is_none() && !v.is_empty())
                        .map(|v| Content::text(v.to_string()))
                        .collect::<Vec<_>>(),
                    v if v.is_none() || v.is_empty() => vec![],
                    v => vec![Content::text(v.to_string())],
                })
                .collect(),
        ))
    }
}

#[derive(Debug, rmcp::serde::Serialize, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct FunctionInfo {
    #[schemars(description = "The function name")]
    name: String,
    #[schemars(description = "The function description")]
    description: String,
    #[schemars(description = "The function parameters")]
    params: Vec<String>,
    #[schemars(description = "Whether this is a built-in function")]
    is_builtin: bool,
}

#[derive(Debug, rmcp::serde::Serialize, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct SelectorInfo {
    #[schemars(description = "The function name")]
    name: String,
    #[schemars(description = "The function description")]
    description: String,
    #[schemars(description = "The function parameters")]
    params: Vec<String>,
}

#[tool_router]
impl Server {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            tool_router: Self::tool_router(),
        })
    }

    #[tool(
        description = "Executes an mq query on the provided HTML content and returns the result as Markdown. Selectors and functions listed in the available_selectors and available_functions tools can be used."
    )]
    fn html_to_markdown(
        &self,
        Parameters(QueryForHtml { html, query }): Parameters<QueryForHtml>,
    ) -> McpResult {
        let mut engine = mq_lang::DefaultEngine::default();
        engine.load_builtin_module();

        let markdown = mq_markdown::Markdown::from_html_str(&html).map_err(|e| {
            ErrorData::parse_error(
                "Failed to parse html",
                Some(serde_json::Value::String(e.to_string())),
            )
        })?;
        let values = engine
            .eval(
                &query.unwrap_or("identity()".to_string()),
                markdown.nodes.into_iter().map(mq_lang::RuntimeValue::from),
            )
            .map_err(|e| {
                ErrorData::invalid_request(
                    "Failed to query",
                    Some(serde_json::Value::String(e.to_string())),
                )
            })?;

        Ok(CallToolResult::success(
            values
                .into_iter()
                .filter_map(|value| {
                    if value.is_none() || value.is_empty() {
                        None
                    } else {
                        Some(Content::text(value.to_string()))
                    }
                })
                .collect::<Vec<_>>(),
        ))
    }

    #[tool(
        description = "Extract from markdown content using a custom mq query. Selectors and functions listed in the available_selectors and available_functions tools can be used."
    )]
    fn extract_markdown(
        &self,
        Parameters(QueryForMarkdown { markdown, query }): Parameters<QueryForMarkdown>,
    ) -> McpResult {
        self.eval_query(&markdown, &query)
    }

    #[tool(description = "Extract all headings (h1–h6) from markdown content.")]
    fn extract_headings(
        &self,
        Parameters(MarkdownInput { markdown }): Parameters<MarkdownInput>,
    ) -> McpResult {
        self.eval_query(&markdown, ".h")
    }

    #[tool(description = "Extract all fenced code blocks from markdown content.")]
    fn extract_code_blocks(
        &self,
        Parameters(MarkdownInput { markdown }): Parameters<MarkdownInput>,
    ) -> McpResult {
        self.eval_query(&markdown, ".code")
    }

    #[tool(description = "Extract all unchecked task list items (todos) from markdown content.")]
    fn extract_todos(
        &self,
        Parameters(MarkdownInput { markdown }): Parameters<MarkdownInput>,
    ) -> McpResult {
        self.eval_query(&markdown, ".todo")
    }

    #[tool(description = "Extract all checked task list items (done tasks) from markdown content.")]
    fn extract_done_tasks(
        &self,
        Parameters(MarkdownInput { markdown }): Parameters<MarkdownInput>,
    ) -> McpResult {
        self.eval_query(&markdown, ".done")
    }

    #[tool(description = "Extract all links from markdown content.")]
    fn extract_links(
        &self,
        Parameters(MarkdownInput { markdown }): Parameters<MarkdownInput>,
    ) -> McpResult {
        self.eval_query(&markdown, ".link")
    }

    #[tool(description = "Extract all images from markdown content.")]
    fn extract_images(
        &self,
        Parameters(MarkdownInput { markdown }): Parameters<MarkdownInput>,
    ) -> McpResult {
        self.eval_query(&markdown, ".image")
    }

    #[tool(description = "Extract all tables from markdown content.")]
    fn extract_tables(
        &self,
        Parameters(MarkdownInput { markdown }): Parameters<MarkdownInput>,
    ) -> McpResult {
        self.eval_query(&markdown, ".table")
    }

    #[tool(description = "Extract all paragraph text nodes from markdown content.")]
    fn extract_text(
        &self,
        Parameters(MarkdownInput { markdown }): Parameters<MarkdownInput>,
    ) -> McpResult {
        self.eval_query(&markdown, ".text")
    }

    #[tool(description = "Extract all blockquotes from markdown content.")]
    fn extract_blockquotes(
        &self,
        Parameters(MarkdownInput { markdown }): Parameters<MarkdownInput>,
    ) -> McpResult {
        self.eval_query(&markdown, ".blockquote")
    }

    #[tool(
        description = "Split markdown content into sections (heading + body) and return all of them as markdown."
    )]
    fn extract_sections(
        &self,
        Parameters(MarkdownInput { markdown }): Parameters<MarkdownInput>,
    ) -> McpResult {
        self.eval_aggregate(
            &markdown,
            r#"import "section" | section::sections() | section::collect()"#,
        )
    }

    #[tool(
        description = "Extract a specific section (heading + body) from markdown content by title. Performs a partial, case-sensitive match on the heading text."
    )]
    fn extract_section(
        &self,
        Parameters(ExtractSectionInput { markdown, title }): Parameters<ExtractSectionInput>,
    ) -> McpResult {
        let escaped = title.replace('\\', r"\\").replace('"', r#"\""#);
        let query = format!(
            r#"import "section" | section::section("{escaped}") | section::collect()"#
        );
        self.eval_aggregate(&markdown, &query)
    }

    #[tool(
        description = "Generate a table of contents from the headings in markdown content. Returns a list of indented entries."
    )]
    fn extract_toc(
        &self,
        Parameters(MarkdownInput { markdown }): Parameters<MarkdownInput>,
    ) -> McpResult {
        self.eval_aggregate(
            &markdown,
            r#"import "section" | section::sections() | section::toc()"#,
        )
    }

    #[tool(description = "Get available selectors that can be used in mq query.")]
    fn available_functions(&self) -> McpResult {
        let hir = mq_hir::Hir::default();
        let mut functions = Vec::with_capacity(256);

        // Get built-in functions
        for (name, builtin_doc) in hir.builtin.functions.iter() {
            functions.push(FunctionInfo {
                name: name.to_string(),
                description: builtin_doc.description.to_string(),
                params: builtin_doc.params.iter().map(|p| p.to_string()).collect(),
                is_builtin: true,
            });
        }

        // Get internal functions
        for (name, builtin_doc) in hir.builtin.internal_functions.iter() {
            functions.push(FunctionInfo {
                name: name.to_string(),
                description: builtin_doc.description.to_string(),
                params: builtin_doc.params.iter().map(|p| p.to_string()).collect(),
                is_builtin: true,
            });
        }

        let output = serde_json::json!({
            "functions": functions,
            "examples": vec![
                r#"select(or(.[], .code, .h)) | upcase() | add(" Hello World")"#.to_string(),
                r#"select(not(.code))"#.to_string(),
                r#"select(.code.lang == "js")"#.to_string(),
            ],
        });
        let functions_json = serde_json::to_string(&output).expect("Failed to serialize functions");

        Ok(CallToolResult::success(vec![Content::text(functions_json)]))
    }

    #[tool(description = "Get available selectors that can be used in mq query.")]
    fn available_selectors(&self) -> McpResult {
        let hir = mq_hir::Hir::default();
        let mut selectors = Vec::with_capacity(256);

        // Get selectors
        for (name, selector_doc) in hir.builtin.selectors.iter() {
            selectors.push(SelectorInfo {
                name: name.to_string(),
                description: selector_doc.description.to_string(),
                params: selector_doc.params.iter().map(|p| p.to_string()).collect(),
            });
        }

        let output = serde_json::json!({
            "selectors": selectors,
        });
        let selectors_json = serde_json::to_string(&output).expect("Failed to serialize selectors");

        Ok(CallToolResult::success(vec![Content::text(selectors_json)]))
    }
}

#[tool_handler]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_protocol_version(ProtocolVersion::V_2025_06_18)
        .with_instructions("mq is a tool for processing markdown content with a jq-like syntax.")
    }
}

pub async fn start() -> miette::Result<()> {
    let transport = (stdin(), stdout());
    let server = Server::new().expect("Failed to create server");

    let service = server.serve(transport).await.map_err(|e| miette!(e))?;
    service.waiting().await.map_err(|e| miette!(e))?;

    Ok(())
}

/// Configuration for the remote (Streamable HTTP) MCP transport.
pub struct HttpConfig {
    /// Address to bind the HTTP listener to, e.g. `127.0.0.1:8080`.
    pub bind: String,
    /// Additional `Host` header values to accept, on top of the loopback
    /// defaults. Required when the server sits behind a reverse proxy or is
    /// otherwise reachable under a non-loopback hostname, since Streamable
    /// HTTP rejects unrecognized hosts to guard against DNS rebinding.
    pub allowed_hosts: Vec<String>,
}

pub async fn start_http(config: HttpConfig) -> miette::Result<()> {
    let mut server_config = StreamableHttpServerConfig::default();
    if !config.allowed_hosts.is_empty() {
        server_config.allowed_hosts.extend(config.allowed_hosts);
    }

    let service = StreamableHttpService::new(
        || Server::new().map_err(|e| std::io::Error::other(e.to_string())),
        Arc::new(LocalSessionManager::default()),
        server_config,
    );

    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(&config.bind)
        .await
        .map_err(|e| miette!(e))?;

    tracing::info!("mq-mcp listening on http://{}/mcp", config.bind);

    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|e| miette!(e))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case(
        QueryForHtml {
            html: "<h1>Test Heading</h1><p>This is a test paragraph.</p>".to_string(),
            query: Some(".h1".to_string()),
        },
        Ok("# Test Heading")
    )]
    #[case(
        QueryForHtml {
            html: "<h1>Test Heading</h1><p>This is a test paragraph.</p>".to_string(),
            query: Some(".text".to_string()),
        },
        Ok("Test Heading\n\nThis is a test paragraph.")
    )]
    #[case(
        QueryForHtml {
            html: "<h1>Test Heading</h1><p>This is a test paragraph.</p>".to_string(),
            query: None,
        },
        Ok("# Test Heading\n\nThis is a test paragraph.")
    )]
    #[case(
        QueryForHtml {
            html: "<h1>Test Heading".to_string(), // malformed HTML
            query: Some(".h1".to_string()),
        },
        Ok("# Test Heading")
    )]
    #[case(
        QueryForHtml {
            html: "<h1>Test Heading</h1>".to_string(),
            query: Some("not_a_function(".to_string()), // invalid query
        },
        Err("Failed to query")
    )]
    fn test_html_to_markdown(
        #[case] query: QueryForHtml,
        #[case] expected: Result<&'static str, &'static str>,
    ) {
        let server = Server::new().expect("Failed to create server");
        let result = server.html_to_markdown(Parameters(query));
        match expected {
            Ok(expected_text) => {
                let result = result.expect("Expected Ok result");
                assert!(!result.is_error.unwrap_or_default());
                let actual = result
                    .content
                    .into_iter()
                    .map(|c| c.as_text().map(|t| t.text.clone()).unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("\n\n");

                assert_eq!(actual, expected_text);
            }
            Err(expected_err) => {
                let err = result.expect_err("Expected error result");
                let msg = format!("{err}");
                assert!(
                    msg.contains(expected_err),
                    "Error message '{msg}' does not contain expected '{expected_err}'"
                );
            }
        }
    }

    #[rstest]
    #[case(
        QueryForMarkdown {
            markdown: "# Test Heading".to_string(),
            query: ".h1".to_string(),
        },
        Ok("# Test Heading")
    )]
    #[case(
        QueryForMarkdown {
            markdown: "# Test Heading\n\nThis is a test paragraph.".to_string(),
            query: ".text".to_string(),
        },
        Ok("Test Heading\n\nThis is a test paragraph.")
    )]
    #[case(
        QueryForMarkdown {
            markdown: "# Test Heading\n\nThis is a test paragraph.".to_string(),
            query: "identity()".to_string(),
        },
        Ok("# Test Heading\n\nThis is a test paragraph.")
    )]
    #[case(
        QueryForMarkdown {
            markdown: "# Test Heading".to_string(),
            query: "not_a_function(".to_string(), // invalid query
        },
        Err("Failed to query")
    )]
    #[case(
        QueryForMarkdown {
            markdown: "".to_string(),
            query: ".h1".to_string(),
        },
        Ok("")
    )]
    fn test_extract_markdown(
        #[case] query: QueryForMarkdown,
        #[case] expected: Result<&'static str, &'static str>,
    ) {
        let server = Server::new().expect("Failed to create server");
        let result = server.extract_markdown(Parameters(query));
        match expected {
            Ok(expected_text) => {
                let result = result.expect("Expected Ok result");
                assert!(!result.is_error.unwrap_or_default());
                let actual = result
                    .content
                    .into_iter()
                    .map(|c| c.as_text().map(|c| c.text.clone()).unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                assert_eq!(actual, expected_text);
            }
            Err(expected_err) => {
                let err = result.expect_err("Expected error result");
                let msg = format!("{err}");
                assert!(
                    msg.contains(expected_err),
                    "Error message '{msg}' does not contain expected '{expected_err}'"
                );
            }
        }
    }

    fn ok_texts(result: CallToolResult) -> Vec<String> {
        assert!(!result.is_error.unwrap_or_default());
        result
            .content
            .into_iter()
            .map(|c| c.as_text().map(|t| t.text.clone()).unwrap_or_default())
            .collect()
    }

    #[rstest]
    #[case("# H1\n\n## H2\n\n### H3", vec!["# H1", "## H2", "### H3"])]
    #[case("No headings here.", vec![])]
    fn test_extract_headings(#[case] markdown: &str, #[case] expected: Vec<&str>) {
        let server = Server::new().unwrap();
        let result = server
            .extract_headings(Parameters(MarkdownInput {
                markdown: markdown.to_string(),
            }))
            .unwrap();
        assert_eq!(ok_texts(result), expected);
    }

    #[rstest]
    #[case("```rust\nfn main() {}\n```\n\n```python\nprint(\"hi\")\n```", 2)]
    #[case("No code blocks.", 0)]
    fn test_extract_code_blocks(#[case] markdown: &str, #[case] count: usize) {
        let server = Server::new().unwrap();
        let result = server
            .extract_code_blocks(Parameters(MarkdownInput {
                markdown: markdown.to_string(),
            }))
            .unwrap();
        assert_eq!(ok_texts(result).len(), count);
    }

    #[rstest]
    #[case("- [ ] Buy milk\n- [x] Buy eggs\n- [ ] Buy bread", vec!["- [ ] Buy milk", "- [ ] Buy bread"])]
    #[case("- [x] Done task", vec![])]
    #[case("No list.", vec![])]
    fn test_extract_todos(#[case] markdown: &str, #[case] expected: Vec<&str>) {
        let server = Server::new().unwrap();
        let result = server
            .extract_todos(Parameters(MarkdownInput {
                markdown: markdown.to_string(),
            }))
            .unwrap();
        assert_eq!(ok_texts(result), expected);
    }

    #[rstest]
    #[case("- [ ] Todo\n- [x] Done", vec!["- [x] Done"])]
    #[case("- [ ] Only todo", vec![])]
    fn test_extract_done_tasks(#[case] markdown: &str, #[case] expected: Vec<&str>) {
        let server = Server::new().unwrap();
        let result = server
            .extract_done_tasks(Parameters(MarkdownInput {
                markdown: markdown.to_string(),
            }))
            .unwrap();
        assert_eq!(ok_texts(result), expected);
    }

    #[rstest]
    #[case("[Google](https://google.com) and [Rust](https://rust-lang.org)", 2)]
    #[case("No links here.", 0)]
    fn test_extract_links(#[case] markdown: &str, #[case] count: usize) {
        let server = Server::new().unwrap();
        let result = server
            .extract_links(Parameters(MarkdownInput {
                markdown: markdown.to_string(),
            }))
            .unwrap();
        assert_eq!(ok_texts(result).len(), count);
    }

    #[rstest]
    #[case("![Alt](img.png) and ![Logo](logo.svg)", 2)]
    #[case("No images.", 0)]
    fn test_extract_images(#[case] markdown: &str, #[case] count: usize) {
        let server = Server::new().unwrap();
        let result = server
            .extract_images(Parameters(MarkdownInput {
                markdown: markdown.to_string(),
            }))
            .unwrap();
        assert_eq!(ok_texts(result).len(), count);
    }

    #[rstest]
    // .table returns individual table cells; a 2-column × 2-row table (header + data) = 4 cells
    #[case("| A | B |\n|---|---|\n| 1 | 2 |", 4)]
    #[case("No tables.", 0)]
    fn test_extract_tables(#[case] markdown: &str, #[case] count: usize) {
        let server = Server::new().unwrap();
        let result = server
            .extract_tables(Parameters(MarkdownInput {
                markdown: markdown.to_string(),
            }))
            .unwrap();
        assert_eq!(ok_texts(result).len(), count);
    }

    #[rstest]
    #[case("Hello world.\n\nSecond paragraph.", 2)]
    // from_html_str treats bare "# Only heading" as a single text paragraph (not a heading)
    #[case("# Only heading", 1)]
    fn test_extract_text(#[case] markdown: &str, #[case] count: usize) {
        let server = Server::new().unwrap();
        let result = server
            .extract_text(Parameters(MarkdownInput {
                markdown: markdown.to_string(),
            }))
            .unwrap();
        assert_eq!(ok_texts(result).len(), count);
    }

    #[rstest]
    #[case("> A quote.\n\n> Another quote.", 2)]
    #[case("No blockquotes.", 0)]
    fn test_extract_blockquotes(#[case] markdown: &str, #[case] count: usize) {
        let server = Server::new().unwrap();
        let result = server
            .extract_blockquotes(Parameters(MarkdownInput {
                markdown: markdown.to_string(),
            }))
            .unwrap();
        assert_eq!(ok_texts(result).len(), count);
    }

    const SECTION_MD: &str = "\
# Introduction\n\
\n\
Welcome.\n\
\n\
## Installation\n\
\n\
Run the command.\n\
\n\
## Usage\n\
\n\
Use it like this.\n";

    #[test]
    fn test_extract_sections() {
        let server = Server::new().unwrap();
        let result = server
            .extract_sections(Parameters(MarkdownInput {
                markdown: SECTION_MD.to_string(),
            }))
            .unwrap();
        let texts = ok_texts(result);
        assert!(!texts.is_empty());
        let joined = texts.join("\n");
        assert!(joined.contains("Introduction"));
        assert!(joined.contains("Installation"));
        assert!(joined.contains("Usage"));
    }

    #[rstest]
    #[case("Installation", true)]
    #[case("Nonexistent", false)]
    fn test_extract_section(#[case] title: &str, #[case] should_have_content: bool) {
        let server = Server::new().unwrap();
        let result = server
            .extract_section(Parameters(ExtractSectionInput {
                markdown: SECTION_MD.to_string(),
                title: title.to_string(),
            }))
            .unwrap();
        let texts = ok_texts(result);
        assert_eq!(!texts.is_empty(), should_have_content);
        if should_have_content {
            assert!(texts.join("\n").contains(title));
        }
    }

    #[test]
    fn test_extract_section_title_with_special_chars() {
        let md = "## Section \"quoted\"\n\nContent.";
        let server = Server::new().unwrap();
        let result = server
            .extract_section(Parameters(ExtractSectionInput {
                markdown: md.to_string(),
                title: "Section \"quoted\"".to_string(),
            }))
            .unwrap();
        let texts = ok_texts(result);
        assert!(!texts.is_empty());
    }

    #[test]
    fn test_extract_toc() {
        let server = Server::new().unwrap();
        let result = server
            .extract_toc(Parameters(MarkdownInput {
                markdown: SECTION_MD.to_string(),
            }))
            .unwrap();
        let texts = ok_texts(result);
        assert!(!texts.is_empty());
        let joined = texts.join("\n");
        assert!(joined.contains("Introduction"));
        assert!(joined.contains("Installation"));
        assert!(joined.contains("Usage"));
    }

    #[test]
    fn test_available_functions() {
        let server = Server::new().expect("Failed to create server");
        let result = server.available_functions().unwrap();
        assert!(!result.is_error.unwrap_or_default());
        assert_eq!(result.content.into_iter().len(), 1);
    }

    #[test]
    fn test_available_selectors() {
        let server = Server::new().expect("Failed to create server");
        let result = server.available_selectors().unwrap();
        assert!(!result.is_error.unwrap_or_default());
        assert_eq!(result.content.into_iter().len(), 1);
    }

    #[test]
    fn test_get_info() {
        let server = Server::new().expect("Failed to create server");
        let info = server.get_info();
        assert_eq!(info.protocol_version, ProtocolVersion::V_2025_06_18);
        assert!(info.instructions.is_some());
        let instructions = info.instructions.unwrap();
        assert!(
            instructions.contains("mq is a tool for processing markdown content"),
            "Instructions should mention mq"
        );
    }
}
