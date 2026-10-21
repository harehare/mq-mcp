use miette::miette;
use rmcp::{
    ErrorData, ServerHandler, ServiceExt,
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ProtocolVersion, ServerCapabilities, ServerConfig},
    schemars, tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use std::{sync::Arc, time::Duration};
use tokio::io::{stdin, stdout};
type McpResult = Result<CallToolResult, ErrorData>;

/// Default per-query timeout, applied when neither the CLI nor the tool call
/// overrides it.
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;
/// Default upper bound for a per-call `timeout_ms`, so a client can't lift the
/// server's limit arbitrarily.
pub const DEFAULT_MAX_TIMEOUT_MS: u64 = 60_000;

/// Limits applied to every mq engine the server creates.
#[derive(Debug, Clone, Copy)]
pub struct EngineConfig {
    /// Timeout used when a tool call doesn't pass `timeout_ms`.
    pub timeout: Duration,
    /// Largest timeout a tool call may request; larger values are clamped.
    pub max_timeout: Duration,
    /// Maximum call stack depth for the mq evaluator (`None` = mq-lang default).
    pub max_call_stack_depth: Option<u32>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
            max_timeout: Duration::from_millis(DEFAULT_MAX_TIMEOUT_MS),
            max_call_stack_depth: None,
        }
    }
}

#[derive(Clone, Default)]
pub struct Server {
    pub tool_router: ToolRouter<Self>,
    config: EngineConfig,
}

#[derive(Debug, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct QueryForHtml {
    #[schemars(description = "The HTML to process")]
    html: String,
    #[schemars(
        description = "The mq query to execute. Selectors and functions listed in the available_selectors and available_functions tools can be used."
    )]
    query: Option<String>,
    #[schemars(
        description = "Optional timeout for this query in milliseconds. Defaults to the server's configured timeout and is capped at the server's maximum."
    )]
    timeout_ms: Option<u64>,
}

#[derive(Debug, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct QueryForMarkdown {
    #[schemars(description = "The markdown to process")]
    markdown: String,
    #[schemars(
        description = "The mq query to execute. Selectors and functions listed in the available_selectors and available_functions tools can be used ."
    )]
    query: String,
    #[schemars(
        description = "Optional timeout for this query in milliseconds. Defaults to the server's configured timeout and is capped at the server's maximum."
    )]
    timeout_ms: Option<u64>,
}

#[derive(Debug, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct MarkdownInput {
    #[schemars(description = "The markdown content to process")]
    markdown: String,
}

#[derive(Debug, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct BatchDocument {
    #[schemars(description = "Optional identifier for this document (e.g. a file path), echoed back in its result")]
    id: Option<String>,
    #[schemars(description = "The markdown content to process")]
    markdown: String,
}

#[derive(Debug, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct QueryForMarkdownBatch {
    #[schemars(description = "Documents to run the same query against")]
    documents: Vec<BatchDocument>,
    #[schemars(
        description = "The mq query to execute against every document. Selectors and functions listed in the available_selectors and available_functions tools can be used."
    )]
    query: String,
    #[schemars(
        description = "Optional timeout for this query in milliseconds. Defaults to the server's configured timeout and is capped at the server's maximum."
    )]
    timeout_ms: Option<u64>,
}

#[derive(Debug, rmcp::serde::Serialize, schemars::JsonSchema)]
struct BatchDocumentResult {
    /// Position of this document in the `documents` array that was passed in.
    index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    results: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct ExtractSectionInput {
    #[schemars(description = "The markdown content to process")]
    markdown: String,
    #[schemars(description = "The section title to extract (partial match)")]
    title: String,
}

/// Which markdown-to-mq input shape a query runs against.
#[derive(Clone, Copy)]
enum Input {
    /// Each top-level node is a separate input (`.h`, `.code`, ...).
    PerNode,
    /// All nodes as a single array input (section functions).
    Aggregate,
}

impl Server {
    fn engine(&self, timeout_ms: Option<u64>) -> mq_lang::DefaultEngine {
        let mut engine = mq_lang::DefaultEngine::default();
        engine.load_builtin_module();
        let timeout = timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.config.timeout)
            .min(self.config.max_timeout);
        engine.set_timeout(timeout);
        if let Some(depth) = self.config.max_call_stack_depth {
            engine.set_max_call_stack_depth(depth);
        }
        engine
    }

    /// Runs `query` and returns the non-empty results as strings, or a plain
    /// message describing what went wrong and how to fix it.
    fn run(
        &self,
        nodes: Vec<mq_markdown::Node>,
        query: &str,
        input: Input,
        timeout_ms: Option<u64>,
    ) -> Result<Vec<String>, String> {
        let mut engine = self.engine(timeout_ms);
        let values: Vec<mq_lang::RuntimeValue> = nodes.into_iter().map(mq_lang::RuntimeValue::from).collect();
        let output = match input {
            Input::PerNode => engine.eval(query, values.into_iter()),
            Input::Aggregate => engine.eval(
                query,
                std::iter::once(mq_lang::RuntimeValue::Array(values.into())),
            ),
        }
        .map_err(|e| {
            format!(
                "Failed to query: {e}\nHint: check the syntax with available_functions / available_selectors; selectors start with '.', e.g. `.h1`, `.code`, `select(.code.lang == \"rust\")`."
            )
        })?;

        Ok(output
            .into_iter()
            .flat_map(|value| match (input, value) {
                (Input::Aggregate, mq_lang::RuntimeValue::Array(arr)) => arr.iter().cloned().collect(),
                (_, v) => vec![v],
            })
            .filter(|v| !v.is_none() && !v.is_empty())
            .map(|v| v.to_string())
            .collect())
    }

    /// Tool-level failures are returned as `is_error` results (not protocol
    /// errors) so the calling model sees the message and can fix its query.
    fn finish(result: Result<Vec<String>, String>) -> McpResult {
        Ok(match result {
            Ok(texts) => CallToolResult::success(texts.into_iter().map(ContentBlock::text).collect()),
            Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
        })
    }

    fn parse_markdown(markdown: &str) -> Result<Vec<mq_markdown::Node>, String> {
        mq_markdown::Markdown::from_markdown_str(markdown)
            .map(|m| m.nodes)
            .map_err(|e| format!("Failed to parse markdown: {e}"))
    }

    fn eval_query(&self, markdown: &str, query: &str, timeout_ms: Option<u64>) -> McpResult {
        Self::finish(
            Self::parse_markdown(markdown)
                .and_then(|nodes| self.run(nodes, query, Input::PerNode, timeout_ms)),
        )
    }

    fn eval_aggregate(&self, markdown: &str, query: &str) -> McpResult {
        Self::finish(
            Self::parse_markdown(markdown)
                .and_then(|nodes| self.run(nodes, query, Input::Aggregate, None)),
        )
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

#[derive(Debug, Default, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct FunctionFilter {
    #[schemars(description = "Case-insensitive substring matched against function names and descriptions")]
    filter: Option<String>,
}

#[tool_router]
impl Server {
    pub fn new(config: EngineConfig) -> Self {
        Self {
            tool_router: Self::tool_router(),
            config,
        }
    }

    #[tool(
        description = "Executes an mq query on the provided HTML content and returns the result as Markdown. Selectors and functions listed in the available_selectors and available_functions tools can be used."
    )]
    fn html_to_markdown(
        &self,
        Parameters(QueryForHtml {
            html,
            query,
            timeout_ms,
        }): Parameters<QueryForHtml>,
    ) -> McpResult {
        let result = mq_markdown::Markdown::from_html_str(&html)
            .map_err(|e| format!("Failed to parse html: {e}"))
            .and_then(|m| {
                self.run(
                    m.nodes,
                    query.as_deref().unwrap_or("identity()"),
                    Input::PerNode,
                    timeout_ms,
                )
            });
        Self::finish(result)
    }

    #[tool(
        description = "Run a custom mq query on markdown content. Prefer the dedicated extract_* tools for simple cases. Examples: `.h2` (all h2 headings), `select(.code.lang == \"rust\")` (rust code blocks), `.link | to_text()` (link texts), `select(contains(\"TODO\"))`. On a syntax error, check available_functions / available_selectors. Queries exceeding the timeout fail; pass timeout_ms to adjust (capped by the server)."
    )]
    fn extract_markdown(
        &self,
        Parameters(QueryForMarkdown {
            markdown,
            query,
            timeout_ms,
        }): Parameters<QueryForMarkdown>,
    ) -> McpResult {
        self.eval_query(&markdown, &query, timeout_ms)
    }

    #[tool(
        description = "Run the same mq query against multiple markdown documents in one call, instead of one extract_markdown call per document. Returns a JSON array with one object per document (index, optional id, results, and error if that document's query failed) — a failure in one document does not abort the rest."
    )]
    fn extract_markdown_batch(
        &self,
        Parameters(QueryForMarkdownBatch {
            documents,
            query,
            timeout_ms,
        }): Parameters<QueryForMarkdownBatch>,
    ) -> McpResult {
        let results: Vec<BatchDocumentResult> = documents
            .into_iter()
            .enumerate()
            .map(|(index, doc)| match Self::parse_markdown(&doc.markdown)
                .and_then(|nodes| self.run(nodes, &query, Input::PerNode, timeout_ms))
            {
                Ok(results) => BatchDocumentResult {
                    index,
                    id: doc.id,
                    results,
                    error: None,
                },
                Err(error) => BatchDocumentResult {
                    index,
                    id: doc.id,
                    results: Vec::new(),
                    error: Some(error),
                },
            })
            .collect();

        let json = serde_json::to_string_pretty(&results).map_err(|e| {
            ErrorData::internal_error(
                "Failed to serialize batch results",
                Some(serde_json::Value::String(e.to_string())),
            )
        })?;

        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    #[tool(description = "Extract all headings (h1–h6) from markdown content.")]
    fn extract_headings(
        &self,
        Parameters(MarkdownInput { markdown }): Parameters<MarkdownInput>,
    ) -> McpResult {
        self.eval_query(&markdown, ".h", None)
    }

    #[tool(description = "Extract all fenced code blocks from markdown content.")]
    fn extract_code_blocks(
        &self,
        Parameters(MarkdownInput { markdown }): Parameters<MarkdownInput>,
    ) -> McpResult {
        self.eval_query(&markdown, ".code", None)
    }

    #[tool(description = "Extract all unchecked task list items (todos) from markdown content.")]
    fn extract_todos(
        &self,
        Parameters(MarkdownInput { markdown }): Parameters<MarkdownInput>,
    ) -> McpResult {
        self.eval_query(&markdown, ".todo", None)
    }

    #[tool(description = "Extract all checked task list items (done tasks) from markdown content.")]
    fn extract_done_tasks(
        &self,
        Parameters(MarkdownInput { markdown }): Parameters<MarkdownInput>,
    ) -> McpResult {
        self.eval_query(&markdown, ".done", None)
    }

    #[tool(description = "Extract all links from markdown content.")]
    fn extract_links(
        &self,
        Parameters(MarkdownInput { markdown }): Parameters<MarkdownInput>,
    ) -> McpResult {
        self.eval_query(&markdown, ".link", None)
    }

    #[tool(description = "Extract all images from markdown content.")]
    fn extract_images(
        &self,
        Parameters(MarkdownInput { markdown }): Parameters<MarkdownInput>,
    ) -> McpResult {
        self.eval_query(&markdown, ".image", None)
    }

    #[tool(description = "Extract all tables from markdown content.")]
    fn extract_tables(
        &self,
        Parameters(MarkdownInput { markdown }): Parameters<MarkdownInput>,
    ) -> McpResult {
        self.eval_query(&markdown, ".table", None)
    }

    #[tool(description = "Extract all paragraph text nodes from markdown content.")]
    fn extract_text(
        &self,
        Parameters(MarkdownInput { markdown }): Parameters<MarkdownInput>,
    ) -> McpResult {
        self.eval_query(&markdown, ".text", None)
    }

    #[tool(description = "Extract all blockquotes from markdown content.")]
    fn extract_blockquotes(
        &self,
        Parameters(MarkdownInput { markdown }): Parameters<MarkdownInput>,
    ) -> McpResult {
        self.eval_query(&markdown, ".blockquote", None)
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

    #[tool(
        description = "List mq functions usable in queries (name, description, params). Pass `filter` to match names/descriptions case-insensitively and keep the output small."
    )]
    fn available_functions(
        &self,
        Parameters(FunctionFilter { filter }): Parameters<FunctionFilter>,
    ) -> McpResult {
        let hir = mq_hir::Hir::default();
        let mut functions = Vec::with_capacity(256);

        // Get built-in functions
        for builtin_doc in hir.builtin.docs.functions() {
            functions.push(FunctionInfo {
                name: builtin_doc.name.to_string(),
                description: builtin_doc.description.to_string(),
                params: builtin_doc.params.iter().map(|p| p.to_string()).collect(),
                is_builtin: true,
            });
        }

        // Get internal functions
        for builtin_doc in hir.builtin.docs.internal_functions() {
            functions.push(FunctionInfo {
                name: builtin_doc.name.to_string(),
                description: builtin_doc.description.to_string(),
                params: builtin_doc.params.iter().map(|p| p.to_string()).collect(),
                is_builtin: true,
            });
        }

        if let Some(filter) = filter.as_deref().map(str::to_lowercase) {
            functions.retain(|f| {
                f.name.to_lowercase().contains(&filter)
                    || f.description.to_lowercase().contains(&filter)
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

        Ok(CallToolResult::success(vec![ContentBlock::text(functions_json)]))
    }

    #[tool(description = "Get available selectors that can be used in mq query.")]
    fn available_selectors(&self) -> McpResult {
        let hir = mq_hir::Hir::default();
        let mut selectors = Vec::with_capacity(256);

        // Get selectors
        for selector_doc in hir.builtin.docs.selectors() {
            selectors.push(SelectorInfo {
                name: selector_doc.name.to_string(),
                description: selector_doc.description.to_string(),
                params: selector_doc.params.iter().map(|p| p.to_string()).collect(),
            });
        }

        let output = serde_json::json!({
            "selectors": selectors,
        });
        let selectors_json = serde_json::to_string(&output).expect("Failed to serialize selectors");

        Ok(CallToolResult::success(vec![ContentBlock::text(selectors_json)]))
    }
}

#[tool_handler]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_protocol_version(ProtocolVersion::V_2025_06_18)
        .with_instructions(
            "mq is a tool for processing markdown content with a jq-like syntax. \
             Start with the dedicated extract_* tools (headings, code blocks, links, sections, toc...). \
             For anything else use extract_markdown (or extract_markdown_batch for many documents, html_to_markdown for HTML) with an mq query such as `.h2` or `select(.code.lang == \"rust\")`. \
             If a query fails, the error explains why; look up valid names with available_functions (use `filter`) and available_selectors. \
             Queries have a timeout; pass `timeout_ms` to raise it up to the server's maximum.",
        )
    }
}

pub async fn start(config: EngineConfig) -> miette::Result<()> {
    let transport = (stdin(), stdout());
    let server = Server::new(config);

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
    /// If set, every request to `/mcp` must carry `Authorization: Bearer
    /// <token>` matching this value, or it's rejected with 401.
    pub bearer_token: Option<String>,
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn require_bearer_token(
    axum::extract::State(expected): axum::extract::State<Arc<str>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let authorized = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|token| constant_time_eq(token.as_bytes(), expected.as_bytes()));

    if authorized {
        next.run(req).await
    } else {
        (axum::http::StatusCode::UNAUTHORIZED, "Unauthorized").into_response()
    }
}

pub async fn start_http(config: HttpConfig, engine: EngineConfig) -> miette::Result<()> {
    let mut server_config = StreamableHttpServerConfig::default();
    if !config.allowed_hosts.is_empty() {
        server_config.allowed_hosts.extend(config.allowed_hosts);
    }

    let service = StreamableHttpService::new(
        move || Ok(Server::new(engine)),
        Arc::new(LocalSessionManager::default()),
        server_config,
    );

    let router = if let Some(token) = &config.bearer_token {
        axum::Router::new().nest_service("/mcp", service).layer(
            axum::middleware::from_fn_with_state(Arc::<str>::from(token.as_str()), require_bearer_token),
        )
    } else {
        axum::Router::new().nest_service("/mcp", service)
    };
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
            timeout_ms: None,
        },
        Ok("# Test Heading")
    )]
    #[case(
        QueryForHtml {
            html: "<h1>Test Heading</h1><p>This is a test paragraph.</p>".to_string(),
            query: Some(".text".to_string()),
            timeout_ms: None,
        },
        Ok("Test Heading\n\nThis is a test paragraph.")
    )]
    #[case(
        QueryForHtml {
            html: "<h1>Test Heading</h1><p>This is a test paragraph.</p>".to_string(),
            query: None,
            timeout_ms: None,
        },
        Ok("# Test Heading\n\nThis is a test paragraph.")
    )]
    #[case(
        QueryForHtml {
            html: "<h1>Test Heading".to_string(), // malformed HTML
            query: Some(".h1".to_string()),
            timeout_ms: None,
        },
        Ok("# Test Heading")
    )]
    #[case(
        QueryForHtml {
            html: "<h1>Test Heading</h1>".to_string(),
            query: Some("not_a_function(".to_string()), // invalid query
            timeout_ms: None,
        },
        Err("Failed to query")
    )]
    fn test_html_to_markdown(
        #[case] query: QueryForHtml,
        #[case] expected: Result<&'static str, &'static str>,
    ) {
        let server = Server::new(EngineConfig::default());
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
                let result = result.expect("tool errors are returned as is_error results");
                assert!(result.is_error.unwrap_or_default());
                let msg = result
                    .content
                    .iter()
                    .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                    .collect::<String>();
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
            timeout_ms: None,
        },
        Ok("# Test Heading")
    )]
    #[case(
        QueryForMarkdown {
            markdown: "# Test Heading\n\nThis is a test paragraph.".to_string(),
            query: ".text".to_string(),
            timeout_ms: None,
        },
        Ok("Test Heading\n\nThis is a test paragraph.")
    )]
    #[case(
        QueryForMarkdown {
            markdown: "# Test Heading\n\nThis is a test paragraph.".to_string(),
            query: "identity()".to_string(),
            timeout_ms: None,
        },
        Ok("# Test Heading\n\nThis is a test paragraph.")
    )]
    #[case(
        QueryForMarkdown {
            markdown: "# Test Heading".to_string(),
            query: "not_a_function(".to_string(), // invalid query
            timeout_ms: None,
        },
        Err("Failed to query")
    )]
    #[case(
        QueryForMarkdown {
            markdown: "".to_string(),
            query: ".h1".to_string(),
            timeout_ms: None,
        },
        Ok("")
    )]
    fn test_extract_markdown(
        #[case] query: QueryForMarkdown,
        #[case] expected: Result<&'static str, &'static str>,
    ) {
        let server = Server::new(EngineConfig::default());
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
                let result = result.expect("tool errors are returned as is_error results");
                assert!(result.is_error.unwrap_or_default());
                let msg = result
                    .content
                    .iter()
                    .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                    .collect::<String>();
                assert!(
                    msg.contains(expected_err),
                    "Error message '{msg}' does not contain expected '{expected_err}'"
                );
            }
        }
    }

    #[test]
    fn test_extract_markdown_batch_mixes_success_and_per_document_error() {
        let server = Server::new(EngineConfig::default());
        let result = server
            .extract_markdown_batch(Parameters(QueryForMarkdownBatch {
                documents: vec![
                    BatchDocument {
                        id: Some("a.md".to_string()),
                        markdown: "# Title A".to_string(),
                    },
                    BatchDocument {
                        id: Some("b.md".to_string()),
                        markdown: "# Title B".to_string(),
                    },
                ],
                query: ".h1".to_string(),
            timeout_ms: None,
            }))
            .expect("batch call should succeed even if a document's query fails");

        assert!(!result.is_error.unwrap_or_default());
        let text = result.content[0].as_text().unwrap().text.clone();
        let parsed: Vec<BatchDocumentResultForTest> = serde_json::from_str(&text).unwrap();

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].index, 0);
        assert_eq!(parsed[0].id.as_deref(), Some("a.md"));
        assert_eq!(parsed[0].results, vec!["# Title A".to_string()]);
        assert!(parsed[0].error.is_none());
        assert_eq!(parsed[1].id.as_deref(), Some("b.md"));
        assert_eq!(parsed[1].results, vec!["# Title B".to_string()]);
    }

    #[test]
    fn test_extract_markdown_batch_reports_error_without_aborting_other_documents() {
        let server = Server::new(EngineConfig::default());
        let result = server
            .extract_markdown_batch(Parameters(QueryForMarkdownBatch {
                documents: vec![
                    BatchDocument {
                        id: None,
                        markdown: "not valid because ".to_string(),
                    },
                    BatchDocument {
                        id: None,
                        markdown: "# Fine".to_string(),
                    },
                ],
                query: "not_a_function(".to_string(),
            timeout_ms: None,
            }))
            .expect("batch call itself should still succeed");

        let text = result.content[0].as_text().unwrap().text.clone();
        let parsed: Vec<BatchDocumentResultForTest> = serde_json::from_str(&text).unwrap();

        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].results.is_empty());
        assert!(parsed[0].error.as_deref().unwrap().contains("Failed to query"));
        assert!(parsed[1].results.is_empty());
        assert!(parsed[1].error.as_deref().unwrap().contains("Failed to query"));
    }

    #[derive(Debug, rmcp::serde::Deserialize)]
    struct BatchDocumentResultForTest {
        index: usize,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        results: Vec<String>,
        #[serde(default)]
        error: Option<String>,
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
        let server = Server::new(EngineConfig::default());
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
        let server = Server::new(EngineConfig::default());
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
        let server = Server::new(EngineConfig::default());
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
        let server = Server::new(EngineConfig::default());
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
        let server = Server::new(EngineConfig::default());
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
        let server = Server::new(EngineConfig::default());
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
        let server = Server::new(EngineConfig::default());
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
        let server = Server::new(EngineConfig::default());
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
        let server = Server::new(EngineConfig::default());
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
        let server = Server::new(EngineConfig::default());
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
        let server = Server::new(EngineConfig::default());
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
        let server = Server::new(EngineConfig::default());
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
        let server = Server::new(EngineConfig::default());
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
        let server = Server::new(EngineConfig::default());
        let result = server
            .available_functions(Parameters(FunctionFilter::default()))
            .unwrap();
        assert!(!result.is_error.unwrap_or_default());
        assert_eq!(result.content.into_iter().len(), 1);
    }

    #[test]
    fn test_available_selectors() {
        let server = Server::new(EngineConfig::default());
        let result = server.available_selectors().unwrap();
        assert!(!result.is_error.unwrap_or_default());
        assert_eq!(result.content.into_iter().len(), 1);
    }

    #[test]
    fn test_get_info() {
        let server = Server::new(EngineConfig::default());
        let info = server.get_info();
        assert_eq!(info.protocol_version, ProtocolVersion::V_2025_06_18);
        assert!(info.instructions.is_some());
        let instructions = info.instructions.unwrap();
        assert!(
            instructions.contains("mq is a tool for processing markdown content"),
            "Instructions should mention mq"
        );
    }

    #[test]
    fn test_available_functions_filter() {
        let server = Server::new(EngineConfig::default());
        let result = server
            .available_functions(Parameters(FunctionFilter {
                filter: Some("upcase".to_string()),
            }))
            .unwrap();
        let json: serde_json::Value = serde_json::from_str(&ok_texts(result)[0]).unwrap();
        let functions = json["functions"].as_array().unwrap();
        assert!(!functions.is_empty());
        assert!(functions.len() < 20);
    }

    #[test]
    fn test_timeout_is_enforced_and_clamped() {
        let server = Server::new(EngineConfig {
            max_timeout: Duration::from_millis(50),
            ..EngineConfig::default()
        });
        // Requested 1h, clamped to 50ms: an endless loop must be cut off.
        let result = server
            .extract_markdown(Parameters(QueryForMarkdown {
                markdown: "# a".to_string(),
                query: "while(true): 1 end".to_string(),
                timeout_ms: Some(3_600_000),
            }))
            .unwrap();
        assert!(result.is_error.unwrap_or_default());
        let msg = result.content[0].as_text().unwrap().text.to_lowercase();
        assert!(msg.contains("timed out"), "unexpected error: {msg}");
    }
}
