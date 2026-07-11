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
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::io::{stdin, stdout};
type McpResult = Result<CallToolResult, ErrorData>;

/// Shared, mutable handle to the loaded `mq-db` store. Guarded by a plain
/// (synchronous) `Mutex` — DB tool methods are synchronous, so there's no
/// `.await` while held, and this avoids pulling in tokio's `sync` feature.
type SharedDb = Arc<Mutex<mq_db::DocumentStore>>;

/// Loads the store at `db_path` if it exists, otherwise starts with an
/// empty one (so `db_index` can populate and later save it to that path).
fn load_or_create_db(db_path: &Path) -> mq_db::DocumentStore {
    if db_path.exists() {
        match mq_db::DocumentStore::load(db_path) {
            Ok(store) => return store,
            Err(e) => tracing::error!(
                "failed to load database at {}: {e} — starting with an empty store",
                db_path.display()
            ),
        }
    }
    mq_db::DocumentStore::new()
}

#[derive(Clone, Default)]
pub struct Server {
    pub tool_router: ToolRouter<Self>,
    /// Configured `--db` path, if any. `None` means no database was
    /// configured at startup — DB tools report a clear error rather than
    /// silently operating on an empty, unsaveable store.
    db_path: Option<PathBuf>,
    db: SharedDb,
}

#[derive(Debug, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct DbSqlInput {
    #[schemars(
        description = "SQL query to run against the loaded mq-db database (SELECT, CREATE TABLE, INSERT INTO, DROP TABLE, DESC, SHOW TABLES). Virtual schema: documents(id, path, title, tags), blocks(id, document_id, block_type, content, pre, post, depth, lang, properties)."
    )]
    query: String,
}

#[derive(Debug, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct DbMqInput {
    #[schemars(
        description = "mq program to run against every document in the loaded mq-db database (only documents indexed from a file path — not ones added as raw strings)"
    )]
    code: String,
}

#[derive(Debug, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct DbIndexInput {
    #[schemars(description = "Markdown files or directories to (re)index into the database")]
    paths: Vec<String>,
    #[schemars(description = "Recursively walk directories (default: false)")]
    recursive: Option<bool>,
    #[schemars(
        description = "Remove catalogued documents whose path is no longer present in `paths` (default: false)"
    )]
    prune: Option<bool>,
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
    pub fn new(db_path: Option<PathBuf>) -> Result<Self, Box<dyn std::error::Error>> {
        let db = db_path
            .as_deref()
            .map(load_or_create_db)
            .unwrap_or_default();
        Ok(Self {
            tool_router: Self::tool_router(),
            db_path,
            db: Arc::new(Mutex::new(db)),
        })
    }

    /// Builds a new `Server` sharing an already-loaded database — used by
    /// the Streamable HTTP transport, which constructs one `Server` per
    /// session and would otherwise reload the store from disk every time.
    fn with_shared_db(db_path: Option<PathBuf>, db: SharedDb) -> Self {
        Self {
            tool_router: Self::tool_router(),
            db_path,
            db,
        }
    }

    /// Returns the locked store, or a descriptive error if no `--db` path
    /// was configured at startup.
    fn require_db(&self) -> Result<std::sync::MutexGuard<'_, mq_db::DocumentStore>, ErrorData> {
        if self.db_path.is_none() {
            return Err(ErrorData::invalid_request(
                "no database configured — restart mq-mcp with --db <path> to enable db_* tools",
                None,
            ));
        }
        Ok(self.db.lock().unwrap_or_else(|e| e.into_inner()))
    }

    #[tool(
        description = "Run a read-only SQL query against the loaded mq-db database and return matching rows as JSON. Requires mq-mcp to have been started with --db <path>."
    )]
    fn db_sql(&self, Parameters(DbSqlInput { query }): Parameters<DbSqlInput>) -> McpResult {
        let store = self.require_db()?;
        let engine = mq_db::SqlEngine::new(&store).map_err(|e| {
            ErrorData::internal_error(
                "Failed to build SQL engine",
                Some(serde_json::Value::String(e.to_string())),
            )
        })?;
        let out = engine.execute(&query).map_err(|e| {
            ErrorData::invalid_request(
                "SQL query failed",
                Some(serde_json::Value::String(e.to_string())),
            )
        })?;
        Ok(CallToolResult::success(vec![Content::text(out.to_json())]))
    }

    #[tool(
        description = "Run an mq program against every document in the loaded mq-db database and return the results. Requires mq-mcp to have been started with --db <path>."
    )]
    fn db_mq(&self, Parameters(DbMqInput { code }): Parameters<DbMqInput>) -> McpResult {
        let store = self.require_db()?;
        let results = mq_db::MqEngine::eval_store(&code, &store).map_err(|e| {
            ErrorData::invalid_request(
                "mq query failed",
                Some(serde_json::Value::String(e.to_string())),
            )
        })?;
        Ok(CallToolResult::success(
            results.into_iter().map(Content::text).collect(),
        ))
    }

    #[tool(
        description = "List every document currently indexed in the loaded mq-db database (id, path, title, tags, block count). Requires mq-mcp to have been started with --db <path>."
    )]
    fn db_list_documents(&self) -> McpResult {
        let store = self.require_db()?;
        let docs: Vec<serde_json::Value> = store
            .documents()
            .iter()
            .map(|d| {
                serde_json::json!({
                    "id": d.id,
                    "path": d.path.as_ref().and_then(|p| p.to_str()),
                    "title": d.zone_maps.title,
                    "tags": d.zone_maps.tags,
                    "block_count": d.block_count,
                })
            })
            .collect();
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string(&docs).unwrap_or_default(),
        )]))
    }

    #[tool(
        description = "Return block-type and code-language statistics for the loaded mq-db database. Requires mq-mcp to have been started with --db <path>."
    )]
    fn db_stats(&self) -> McpResult {
        let store = self.require_db()?;
        let stats = store.stats();
        let json = serde_json::json!({
            "documents": stats.documents,
            "blocks": stats.blocks,
            "block_type_counts": stats.block_type_counts.iter()
                .map(|(bt, count)| serde_json::json!({"block_type": bt.as_str(), "count": count}))
                .collect::<Vec<_>>(),
            "code_lang_counts": stats.code_lang_counts.iter()
                .map(|(lang, count)| serde_json::json!({"lang": lang, "count": count}))
                .collect::<Vec<_>>(),
        });
        Ok(CallToolResult::success(vec![Content::text(
            json.to_string(),
        )]))
    }

    #[tool(
        description = "Index or re-index Markdown files/directories into the loaded mq-db database, then persist it to the configured --db path. Skips files whose content hasn't changed since the last index; use `prune` to drop catalogued documents whose file no longer exists. Requires mq-mcp to have been started with --db <path>."
    )]
    fn db_index(
        &self,
        Parameters(DbIndexInput {
            paths,
            recursive,
            prune,
        }): Parameters<DbIndexInput>,
    ) -> McpResult {
        let db_path = self.db_path.clone().ok_or_else(|| {
            ErrorData::invalid_request(
                "no database configured — restart mq-mcp with --db <path> to enable db_* tools",
                None,
            )
        })?;

        let files = mq_db::discover::collect_markdown_files(
            &paths.iter().map(PathBuf::from).collect::<Vec<_>>(),
            recursive.unwrap_or(false),
        );
        if files.is_empty() {
            return Err(ErrorData::invalid_request(
                "No Markdown files found in the given paths",
                None,
            ));
        }

        let mut store = self.db.lock().unwrap_or_else(|e| e.into_inner());
        let report = store
            .reindex_paths(&files, prune.unwrap_or(false))
            .map_err(|e| {
                ErrorData::internal_error(
                    "Reindex failed",
                    Some(serde_json::Value::String(e.to_string())),
                )
            })?;
        store.save(&db_path).map_err(|e| {
            ErrorData::internal_error(
                "Failed to save database",
                Some(serde_json::Value::String(e.to_string())),
            )
        })?;

        let json = serde_json::json!({
            "added": report.added.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
            "updated": report.updated.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
            "unchanged": report.unchanged,
            "removed": report.removed.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
            "failed": report.failed.iter().map(|(p, e)| serde_json::json!({"path": p.to_string_lossy(), "error": e})).collect::<Vec<_>>(),
        });
        Ok(CallToolResult::success(vec![Content::text(
            json.to_string(),
        )]))
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

pub async fn start(db_path: Option<PathBuf>) -> miette::Result<()> {
    let transport = (stdin(), stdout());
    let server = Server::new(db_path).expect("Failed to create server");

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

pub async fn start_http(config: HttpConfig, db_path: Option<PathBuf>) -> miette::Result<()> {
    let mut server_config = StreamableHttpServerConfig::default();
    if !config.allowed_hosts.is_empty() {
        server_config.allowed_hosts.extend(config.allowed_hosts);
    }

    // Load the database once and share it across every session — sessions
    // would otherwise each reload the store from disk (and not observe each
    // other's `db_index` writes).
    let shared_db: SharedDb = Arc::new(Mutex::new(
        db_path
            .as_deref()
            .map(load_or_create_db)
            .unwrap_or_default(),
    ));
    let service = StreamableHttpService::new(
        move || Ok(Server::with_shared_db(db_path.clone(), shared_db.clone())),
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
        let server = Server::new(None).expect("Failed to create server");
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
        let server = Server::new(None).expect("Failed to create server");
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
        let server = Server::new(None).unwrap();
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
        let server = Server::new(None).unwrap();
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
        let server = Server::new(None).unwrap();
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
        let server = Server::new(None).unwrap();
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
        let server = Server::new(None).unwrap();
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
        let server = Server::new(None).unwrap();
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
        let server = Server::new(None).unwrap();
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
        let server = Server::new(None).unwrap();
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
        let server = Server::new(None).unwrap();
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
        let server = Server::new(None).unwrap();
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
        let server = Server::new(None).unwrap();
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
        let server = Server::new(None).unwrap();
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
        let server = Server::new(None).unwrap();
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
        let server = Server::new(None).expect("Failed to create server");
        let result = server.available_functions().unwrap();
        assert!(!result.is_error.unwrap_or_default());
        assert_eq!(result.content.into_iter().len(), 1);
    }

    #[test]
    fn test_available_selectors() {
        let server = Server::new(None).expect("Failed to create server");
        let result = server.available_selectors().unwrap();
        assert!(!result.is_error.unwrap_or_default());
        assert_eq!(result.content.into_iter().len(), 1);
    }

    #[test]
    fn test_get_info() {
        let server = Server::new(None).expect("Failed to create server");
        let info = server.get_info();
        assert_eq!(info.protocol_version, ProtocolVersion::V_2025_06_18);
        assert!(info.instructions.is_some());
        let instructions = info.instructions.unwrap();
        assert!(
            instructions.contains("mq is a tool for processing markdown content"),
            "Instructions should mention mq"
        );
    }

    // ── db_* tools ───────────────────────────────────────────────────────────

    #[test]
    fn db_tools_report_error_without_configured_db() {
        let server = Server::new(None).expect("Failed to create server");
        let err = server
            .db_sql(Parameters(DbSqlInput {
                query: "SELECT 1".to_string(),
            }))
            .expect_err("expected an error with no --db configured");
        assert!(err.message.contains("no database configured"));
    }

    #[test]
    fn db_index_then_sql_and_mq_and_list_and_stats_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "# Title\n\nHello world\n").unwrap();
        let db_path = dir.path().join("store.mq-db");

        let server = Server::new(Some(db_path)).expect("Failed to create server");

        let index_result = server
            .db_index(Parameters(DbIndexInput {
                paths: vec![dir.path().to_string_lossy().to_string()],
                recursive: Some(false),
                prune: Some(false),
            }))
            .expect("db_index should succeed");
        let index_json = ok_texts(index_result).join("");
        assert!(index_json.contains("\"added\":[") && index_json.contains("a.md"));

        let sql_result = server
            .db_sql(Parameters(DbSqlInput {
                query: "SELECT content FROM blocks WHERE block_type = 'heading'".to_string(),
            }))
            .expect("db_sql should succeed");
        assert!(ok_texts(sql_result).join("").contains("Title"));

        let mq_result = server
            .db_mq(Parameters(DbMqInput {
                code: ".h1".to_string(),
            }))
            .expect("db_mq should succeed");
        assert!(ok_texts(mq_result).join("").contains("Title"));

        let list_result = server
            .db_list_documents()
            .expect("db_list_documents should succeed");
        let list_json = ok_texts(list_result).join("");
        assert!(list_json.contains("a.md"));

        let stats_result = server.db_stats().expect("db_stats should succeed");
        let stats_json = ok_texts(stats_result).join("");
        assert!(stats_json.contains("\"documents\":1"));
    }

    #[test]
    fn db_index_second_run_reports_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "# Title\n\nHello world\n").unwrap();
        let db_path = dir.path().join("store.mq-db");

        let server = Server::new(Some(db_path)).expect("Failed to create server");
        let index_input = || DbIndexInput {
            paths: vec![dir.path().to_string_lossy().to_string()],
            recursive: Some(false),
            prune: Some(false),
        };

        server
            .db_index(Parameters(index_input()))
            .expect("first db_index should succeed");
        let second = server
            .db_index(Parameters(index_input()))
            .expect("second db_index should succeed");
        let second_json = ok_texts(second).join("");
        assert!(second_json.contains("\"unchanged\":1"));
    }
}
