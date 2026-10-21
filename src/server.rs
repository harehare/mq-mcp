use crate::{
    check::{self, DRY_RUN_SAMPLES, DryRun, QueryCheck},
    files::FileAccess,
    outline::{self, FrontmatterResult, Outline},
    output::{self, DEFAULT_MAX_OUTPUT_CHARS, MIN_OUTPUT_CHARS, OutputOptions},
};
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

/// Extensions `list_files` looks for when the caller doesn't ask for others.
const DEFAULT_LIST_EXTENSIONS: &[&str] = &["md", "markdown", "mdx"];
/// Entries `list_files` returns when the caller doesn't set a limit, and the most it ever returns.
const DEFAULT_LIST_LIMIT: usize = 200;
const MAX_LIST_LIMIT: usize = 1000;

/// Limits applied to every mq engine and every tool result the server produces.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Timeout used when a tool call doesn't pass `timeout_ms`.
    pub timeout: Duration,
    /// Largest timeout a tool call may request; larger values are clamped.
    pub max_timeout: Duration,
    /// Maximum call stack depth for the mq evaluator (`None` = mq-lang default).
    pub max_call_stack_depth: Option<u32>,
    /// Most characters a single tool call returns; a call may ask for less via `max_output_chars`.
    pub max_output_chars: usize,
    /// Directories the `path` parameter may read from. No roots disables file access.
    pub files: FileAccess,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
            max_timeout: Duration::from_millis(DEFAULT_MAX_TIMEOUT_MS),
            max_call_stack_depth: None,
            max_output_chars: DEFAULT_MAX_OUTPUT_CHARS,
            files: FileAccess::default(),
        }
    }
}

#[derive(Clone, Default)]
pub struct Server {
    pub tool_router: ToolRouter<Self>,
    config: EngineConfig,
}

/// A Markdown document, given inline or as a file under an allowed directory.
#[derive(Debug, Default, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct DocumentInput {
    #[schemars(description = "The markdown content to process. Give either `markdown` or `path`.")]
    markdown: Option<String>,
    #[schemars(
        description = "Path of a Markdown file to read instead of passing `markdown`; avoids sending large documents through the conversation. Must be inside a directory the server allows (see list_files); relative paths are resolved against those directories."
    )]
    path: Option<String>,
}

#[derive(Debug, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct QueryForHtml {
    #[schemars(description = "The HTML to process. Give either `html` or `path`.")]
    html: Option<String>,
    #[schemars(
        description = "Path of an HTML file to read instead of passing `html`. Must be inside a directory the server allows (see list_files)."
    )]
    path: Option<String>,
    #[schemars(
        description = "The mq query to execute. Selectors and functions listed in the available_selectors and available_functions tools can be used."
    )]
    query: Option<String>,
    #[schemars(
        description = "Optional timeout for this query in milliseconds. Defaults to the server's configured timeout and is capped at the server's maximum."
    )]
    timeout_ms: Option<u64>,
    #[serde(flatten)]
    output: OutputOptions,
}

#[derive(Debug, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct QueryForMarkdown {
    #[serde(flatten)]
    doc: DocumentInput,
    #[schemars(
        description = "The mq query to execute. Selectors and functions listed in the available_selectors and available_functions tools can be used ."
    )]
    query: String,
    #[schemars(
        description = "Optional timeout for this query in milliseconds. Defaults to the server's configured timeout and is capped at the server's maximum."
    )]
    timeout_ms: Option<u64>,
    #[serde(flatten)]
    output: OutputOptions,
}

#[derive(Debug, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct MarkdownInput {
    #[serde(flatten)]
    doc: DocumentInput,
    #[serde(flatten)]
    output: OutputOptions,
}

#[derive(Debug, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct BatchDocument {
    #[schemars(
        description = "Optional identifier for this document (e.g. a file path), echoed back in its result. Defaults to `path` when that is given."
    )]
    id: Option<String>,
    #[serde(flatten)]
    doc: DocumentInput,
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
    #[schemars(
        description = "Return at most this many results per document. A document with more reports `next_offset`."
    )]
    limit: Option<usize>,
    #[schemars(
        description = "Cap the total characters returned across all documents. Capped at the server's maximum; documents that no longer fit are returned without results."
    )]
    max_output_chars: Option<usize>,
}

#[derive(Debug, rmcp::serde::Serialize, schemars::JsonSchema)]
struct BatchDocumentResult {
    /// Position of this document in the `documents` array that was passed in.
    index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    results: Vec<String>,
    /// Number of results the query produced, including any not returned.
    total: usize,
    /// Set when results were left out because of `limit` or `max_output_chars`.
    #[serde(skip_serializing_if = "Option::is_none")]
    next_offset: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, rmcp::serde::Serialize, schemars::JsonSchema)]
struct BatchOutput {
    documents: Vec<BatchDocumentResult>,
}

#[derive(Debug, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct ExtractSectionInput {
    #[serde(flatten)]
    doc: DocumentInput,
    #[schemars(description = "The section title to extract (partial match)")]
    title: String,
    #[serde(flatten)]
    output: OutputOptions,
}

#[derive(Debug, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct OutlineInput {
    #[serde(flatten)]
    doc: DocumentInput,
    #[schemars(
        description = "List only headings up to this depth (1-6). The counts in `stats` still cover the whole document."
    )]
    max_depth: Option<u8>,
}

#[derive(Debug, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct ValidateQueryInput {
    #[schemars(description = "The mq query to check.")]
    query: String,
    #[serde(flatten)]
    sample: DocumentInput,
    #[schemars(
        description = "Optional timeout for the dry run in milliseconds. Defaults to the server's configured timeout and is capped at the server's maximum."
    )]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Default, rmcp::serde::Deserialize, schemars::JsonSchema)]
struct ListFilesInput {
    #[schemars(
        description = "Directory to list, inside an allowed directory. Defaults to all allowed directories."
    )]
    directory: Option<String>,
    #[schemars(
        description = "File extensions to include, without the dot. Default: md, markdown, mdx."
    )]
    extensions: Option<Vec<String>>,
    #[schemars(description = "Return at most this many files (default 200, maximum 1000).")]
    limit: Option<usize>,
}

#[derive(Debug, rmcp::serde::Serialize, schemars::JsonSchema)]
struct FileEntry {
    /// Absolute path; pass it as `path` to the other tools.
    path: String,
    size_bytes: u64,
}

#[derive(Debug, rmcp::serde::Serialize, schemars::JsonSchema)]
struct FileList {
    /// The directories the server allows reading from.
    roots: Vec<String>,
    files: Vec<FileEntry>,
    /// True if more files exist than were returned.
    truncated: bool,
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

    /// Returns inline `content`, or the contents of `path`; exactly one must be given.
    fn load(
        &self,
        content: Option<String>,
        path: Option<&str>,
        inline_name: &str,
    ) -> Result<String, String> {
        match (content, path) {
            (Some(_), Some(_)) => Err(format!("Pass either `{inline_name}` or `path`, not both.")),
            (Some(content), None) => Ok(content),
            (None, Some(path)) => self.config.files.read(path),
            (None, None) => Err(format!(
                "Pass `{inline_name}` with the content, or `path` with a file to read."
            )),
        }
    }

    fn load_markdown(&self, doc: DocumentInput) -> Result<String, String> {
        self.load(doc.markdown, doc.path.as_deref(), "markdown")
    }

    /// Runs `query` and returns the non-empty results, or a plain message
    /// describing what went wrong and how to fix it.
    fn run(
        &self,
        nodes: Vec<mq_markdown::Node>,
        query: &str,
        input: Input,
        timeout_ms: Option<u64>,
    ) -> Result<Vec<mq_lang::RuntimeValue>, String> {
        let mut engine = self.engine(timeout_ms);
        let values: Vec<mq_lang::RuntimeValue> =
            nodes.into_iter().map(mq_lang::RuntimeValue::from).collect();
        let output = match input {
            Input::PerNode => engine.eval(query, values.into_iter()),
            Input::Aggregate => engine.eval(
                query,
                std::iter::once(mq_lang::RuntimeValue::Array(values.into())),
            ),
        }
        .map_err(|e| {
            format!(
                "Failed to query: {e}\nHint: run validate_query to locate the problem, and check names with available_functions / available_selectors; selectors start with '.', e.g. `.h1`, `.code`, `select(.code.lang == \"rust\")`."
            )
        })?;

        Ok(output
            .into_iter()
            .flat_map(|value| match (input, value) {
                (Input::Aggregate, mq_lang::RuntimeValue::Array(arr)) => {
                    arr.iter().cloned().collect()
                }
                (_, v) => vec![v],
            })
            .filter(|v| !v.is_none() && !v.is_empty())
            .collect())
    }

    /// Tool-level failures are returned as `is_error` results (not protocol
    /// errors) so the calling model sees the message and can fix its query.
    fn finish(
        &self,
        result: Result<Vec<mq_lang::RuntimeValue>, String>,
        output: &OutputOptions,
    ) -> McpResult {
        Ok(match result {
            Ok(values) => output::render(&values, output, self.config.max_output_chars),
            Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
        })
    }

    fn parse_markdown(markdown: &str) -> Result<Vec<mq_markdown::Node>, String> {
        mq_markdown::Markdown::from_markdown_str(markdown)
            .map(|m| m.nodes)
            .map_err(|e| format!("Failed to parse markdown: {e}"))
    }

    fn eval_markdown(
        &self,
        doc: DocumentInput,
        query: &str,
        input: Input,
        timeout_ms: Option<u64>,
        output: &OutputOptions,
    ) -> McpResult {
        self.finish(
            self.load_markdown(doc)
                .and_then(|markdown| Self::parse_markdown(&markdown))
                .and_then(|nodes| self.run(nodes, query, input, timeout_ms)),
            output,
        )
    }

    fn eval_query(
        &self,
        doc: DocumentInput,
        query: &str,
        timeout_ms: Option<u64>,
        output: &OutputOptions,
    ) -> McpResult {
        self.eval_markdown(doc, query, Input::PerNode, timeout_ms, output)
    }

    fn eval_aggregate(&self, doc: DocumentInput, query: &str, output: &OutputOptions) -> McpResult {
        self.eval_markdown(doc, query, Input::Aggregate, None, output)
    }

    fn eval_selector(&self, input: MarkdownInput, selector: &str) -> McpResult {
        self.eval_query(input.doc, selector, None, &input.output)
    }

    /// A successful result carrying `value` as structured content (and as text, for clients that ignore it).
    fn structured<T: rmcp::serde::Serialize>(value: &T) -> McpResult {
        serde_json::to_value(value)
            .map(CallToolResult::structured)
            .map_err(|e| {
                ErrorData::internal_error(
                    "Failed to serialize structured result",
                    Some(serde_json::Value::String(e.to_string())),
                )
            })
    }

    fn failure(message: String) -> McpResult {
        Ok(CallToolResult::error(vec![ContentBlock::text(message)]))
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
    #[schemars(
        description = "Case-insensitive substring matched against function names and descriptions"
    )]
    filter: Option<String>,
}

#[tool_router]
impl Server {
    pub fn new(mut config: EngineConfig) -> Self {
        config.max_output_chars = config.max_output_chars.max(MIN_OUTPUT_CHARS);
        Self {
            tool_router: Self::tool_router(),
            config,
        }
    }

    #[tool(
        description = "Executes an mq query on the provided HTML content (inline or from a file) and returns the result as Markdown. Selectors and functions listed in the available_selectors and available_functions tools can be used. Use `limit`/`offset` to page through long results and `format: \"json\"` for structured results."
    )]
    fn html_to_markdown(
        &self,
        Parameters(QueryForHtml {
            html,
            path,
            query,
            timeout_ms,
            output,
        }): Parameters<QueryForHtml>,
    ) -> McpResult {
        let result = self
            .load(html, path.as_deref(), "html")
            .and_then(|html| {
                mq_markdown::Markdown::from_html_str(&html)
                    .map_err(|e| format!("Failed to parse html: {e}"))
            })
            .and_then(|m| {
                self.run(
                    m.nodes,
                    query.as_deref().unwrap_or("identity()"),
                    Input::PerNode,
                    timeout_ms,
                )
            });
        self.finish(result, &output)
    }

    #[tool(
        description = "Run a custom mq query on markdown content (inline `markdown` or a file `path`). Prefer the dedicated extract_* tools for simple cases. Examples: `.h2` (all h2 headings), `select(.code.lang == \"rust\")` (rust code blocks), `.link | to_text()` (link texts), `select(contains(\"TODO\"))`. On a syntax error, use validate_query, or check available_functions / available_selectors. Queries exceeding the timeout fail; pass timeout_ms to adjust (capped by the server). Long results are cut at max_output_chars; use limit/offset to page, and format: \"json\" for types, attributes and line positions."
    )]
    fn extract_markdown(
        &self,
        Parameters(QueryForMarkdown {
            doc,
            query,
            timeout_ms,
            output,
        }): Parameters<QueryForMarkdown>,
    ) -> McpResult {
        self.eval_query(doc, &query, timeout_ms, &output)
    }

    #[tool(
        description = "Run the same mq query against multiple markdown documents (inline or by file path) in one call, instead of one extract_markdown call per document. Returns one object per document (index, optional id, results, total, and error if that document's query failed) — a failure in one document does not abort the rest.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<BatchOutput>()
    )]
    fn extract_markdown_batch(
        &self,
        Parameters(QueryForMarkdownBatch {
            documents,
            query,
            timeout_ms,
            limit,
            max_output_chars,
        }): Parameters<QueryForMarkdownBatch>,
    ) -> McpResult {
        let options = OutputOptions {
            max_output_chars,
            ..OutputOptions::default()
        };
        let mut remaining = options.budget(self.config.max_output_chars);

        let documents: Vec<BatchDocumentResult> = documents
            .into_iter()
            .enumerate()
            .map(|(index, document)| {
                let id = document.id.or_else(|| document.doc.path.clone());
                let outcome = self
                    .load_markdown(document.doc)
                    .and_then(|markdown| Self::parse_markdown(&markdown))
                    .and_then(|nodes| self.run(nodes, &query, Input::PerNode, timeout_ms));

                match outcome {
                    Ok(values) => {
                        let texts: Vec<String> = values.iter().map(|v| v.to_string()).collect();
                        let sizes: Vec<usize> = texts.iter().map(|t| t.chars().count()).collect();
                        let page = output::paginate(&sizes, 0, limit, remaining);
                        let results: Vec<String> = (page.start..page.end)
                            .map(|i| match page.cut_to {
                                Some(max) => output::truncate_chars(&texts[i], max),
                                None => texts[i].clone(),
                            })
                            .collect();
                        remaining = remaining
                            .saturating_sub(results.iter().map(|r| r.chars().count()).sum());

                        BatchDocumentResult {
                            index,
                            id,
                            results,
                            total: page.total,
                            next_offset: page.next_offset,
                            error: None,
                        }
                    }
                    Err(error) => BatchDocumentResult {
                        index,
                        id,
                        results: Vec::new(),
                        total: 0,
                        next_offset: None,
                        error: Some(error),
                    },
                }
            })
            .collect();

        Self::structured(&BatchOutput { documents })
    }

    #[tool(description = "Extract all headings (h1–h6) from markdown content.")]
    fn extract_headings(&self, Parameters(input): Parameters<MarkdownInput>) -> McpResult {
        self.eval_selector(input, ".h")
    }

    #[tool(description = "Extract all fenced code blocks from markdown content.")]
    fn extract_code_blocks(&self, Parameters(input): Parameters<MarkdownInput>) -> McpResult {
        self.eval_selector(input, ".code")
    }

    #[tool(description = "Extract all unchecked task list items (todos) from markdown content.")]
    fn extract_todos(&self, Parameters(input): Parameters<MarkdownInput>) -> McpResult {
        self.eval_selector(input, ".todo")
    }

    #[tool(description = "Extract all checked task list items (done tasks) from markdown content.")]
    fn extract_done_tasks(&self, Parameters(input): Parameters<MarkdownInput>) -> McpResult {
        self.eval_selector(input, ".done")
    }

    #[tool(description = "Extract all links from markdown content.")]
    fn extract_links(&self, Parameters(input): Parameters<MarkdownInput>) -> McpResult {
        self.eval_selector(input, ".link")
    }

    #[tool(description = "Extract all images from markdown content.")]
    fn extract_images(&self, Parameters(input): Parameters<MarkdownInput>) -> McpResult {
        self.eval_selector(input, ".image")
    }

    #[tool(description = "Extract all tables from markdown content.")]
    fn extract_tables(&self, Parameters(input): Parameters<MarkdownInput>) -> McpResult {
        self.eval_selector(input, ".table")
    }

    #[tool(description = "Extract all paragraph text nodes from markdown content.")]
    fn extract_text(&self, Parameters(input): Parameters<MarkdownInput>) -> McpResult {
        self.eval_selector(input, ".text")
    }

    #[tool(description = "Extract all blockquotes from markdown content.")]
    fn extract_blockquotes(&self, Parameters(input): Parameters<MarkdownInput>) -> McpResult {
        self.eval_selector(input, ".blockquote")
    }

    #[tool(
        description = "Split markdown content into sections (heading + body) and return all of them as markdown."
    )]
    fn extract_sections(&self, Parameters(input): Parameters<MarkdownInput>) -> McpResult {
        self.eval_aggregate(
            input.doc,
            r#"import "section" | section::sections() | section::collect()"#,
            &input.output,
        )
    }

    #[tool(
        description = "Extract a specific section (heading + body) from markdown content by title. Performs a partial, case-sensitive match on the heading text."
    )]
    fn extract_section(
        &self,
        Parameters(ExtractSectionInput { doc, title, output }): Parameters<ExtractSectionInput>,
    ) -> McpResult {
        let escaped = title.replace('\\', r"\\").replace('"', r#"\""#);
        let query =
            format!(r#"import "section" | section::section("{escaped}") | section::collect()"#);
        self.eval_aggregate(doc, &query, &output)
    }

    #[tool(
        description = "Generate a table of contents from the headings in markdown content. Returns a list of indented entries."
    )]
    fn extract_toc(&self, Parameters(input): Parameters<MarkdownInput>) -> McpResult {
        self.eval_aggregate(
            input.doc,
            r#"import "section" | section::sections() | section::toc()"#,
            &input.output,
        )
    }

    #[tool(
        description = "Summarise a markdown document's structure without returning its content: totals (characters, lines, code blocks by language, tables, links, images, todos), whether it has front matter, and every heading with its line range, size in characters and number of subsections. Use it first on large documents to decide which section to fetch with extract_section.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<Outline>()
    )]
    fn document_outline(
        &self,
        Parameters(OutlineInput { doc, max_depth }): Parameters<OutlineInput>,
    ) -> McpResult {
        match self
            .load_markdown(doc)
            .and_then(|markdown| Self::parse_markdown(&markdown).map(|nodes| (markdown, nodes)))
        {
            Ok((markdown, nodes)) => {
                Self::structured(&outline::outline(&markdown, &nodes, max_depth))
            }
            Err(message) => Self::failure(message),
        }
    }

    #[tool(
        description = "Extract a document's YAML or TOML front matter as parsed JSON, along with its raw text. Reports found: false when the document has none.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<FrontmatterResult>()
    )]
    fn extract_frontmatter(&self, Parameters(doc): Parameters<DocumentInput>) -> McpResult {
        match self
            .load_markdown(doc)
            .and_then(|markdown| Self::parse_markdown(&markdown))
        {
            Ok(nodes) => Self::structured(&outline::parse_frontmatter(&nodes)),
            Err(message) => Self::failure(message),
        }
    }

    #[tool(
        description = "Check an mq query before running it: reports syntax errors and unknown functions/selectors with line and column and a \"did you mean\" suggestion. Optionally pass a sample (`markdown` or `path`) to also dry-run the query and see the match count and the first few results.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<QueryCheck>()
    )]
    fn validate_query(
        &self,
        Parameters(ValidateQueryInput {
            query,
            sample,
            timeout_ms,
        }): Parameters<ValidateQueryInput>,
    ) -> McpResult {
        let has_sample = sample.markdown.is_some() || sample.path.is_some();
        let sample = if has_sample {
            match self.load_markdown(sample) {
                Ok(markdown) => Some(markdown),
                Err(message) => return Self::failure(message),
            }
        } else {
            None
        };

        let issues = check::check_query(&mut self.engine(timeout_ms), &query);
        let dry_run = sample.filter(|_| issues.is_empty()).map(|markdown| {
            match Self::parse_markdown(&markdown)
                .and_then(|nodes| self.run(nodes, &query, Input::PerNode, timeout_ms))
            {
                Ok(values) => DryRun {
                    match_count: values.len(),
                    samples: values
                        .iter()
                        .take(DRY_RUN_SAMPLES)
                        .map(|v| check::shorten(&v.to_string()))
                        .collect(),
                    error: None,
                },
                Err(error) => DryRun {
                    match_count: 0,
                    samples: Vec::new(),
                    error: Some(error),
                },
            }
        });

        Self::structured(&QueryCheck {
            valid: issues.is_empty(),
            issues,
            dry_run,
        })
    }

    #[tool(
        description = "List the files the server may read through the `path` parameter, sorted by path. Defaults to Markdown files (md, markdown, mdx) in all allowed directories; pass `directory` and `extensions` to narrow or widen the search.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<FileList>()
    )]
    fn list_files(&self, Parameters(input): Parameters<ListFilesInput>) -> McpResult {
        let extensions = input.extensions.unwrap_or_else(|| {
            DEFAULT_LIST_EXTENSIONS
                .iter()
                .map(|ext| ext.to_string())
                .collect()
        });
        let extensions: Vec<String> = extensions
            .into_iter()
            .map(|ext| ext.trim_start_matches('.').to_string())
            .collect();
        let limit = input
            .limit
            .unwrap_or(DEFAULT_LIST_LIMIT)
            .clamp(1, MAX_LIST_LIMIT);

        match self
            .config
            .files
            .list(input.directory.as_deref(), &extensions, limit)
        {
            Ok((files, truncated)) => Self::structured(&FileList {
                roots: self
                    .config
                    .files
                    .roots()
                    .iter()
                    .map(|root| root.display().to_string())
                    .collect(),
                files: files
                    .into_iter()
                    .map(|(path, size_bytes)| FileEntry {
                        path: path.display().to_string(),
                        size_bytes,
                    })
                    .collect(),
                truncated,
            }),
            Err(message) => Self::failure(message),
        }
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

        Ok(CallToolResult::success(vec![ContentBlock::text(
            functions_json,
        )]))
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

        Ok(CallToolResult::success(vec![ContentBlock::text(
            selectors_json,
        )]))
    }
}

#[tool_handler]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        let mut instructions = String::from(
            "mq is a tool for processing markdown content with a jq-like syntax. \
             For a large document, start with document_outline to see its structure, then fetch only what you need. \
             Use the dedicated extract_* tools (headings, code blocks, links, sections, toc, frontmatter...) for common cases. \
             For anything else use extract_markdown (or extract_markdown_batch for many documents, html_to_markdown for HTML) with an mq query such as `.h2` or `select(.code.lang == \"rust\")`. \
             If a query fails, the error explains why; validate_query checks a query without running it, and available_functions (use `filter`) and available_selectors list valid names. \
             Queries have a timeout; pass `timeout_ms` to raise it up to the server's maximum. \
             Long results are cut at a character limit and say how to continue: use `limit` and `offset` to page, or `format: \"json\"` for result types, attributes and line positions.",
        );
        if self.config.files.is_enabled() {
            let roots = self
                .config
                .files
                .roots()
                .iter()
                .map(|root| root.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            instructions.push_str(&format!(
                " Instead of sending a document's content, pass `path` to read a file under: {roots}. list_files shows what is available."
            ));
        }

        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_protocol_version(ProtocolVersion::V_2025_06_18)
        .with_instructions(instructions)
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
        move || Ok(Server::new(engine.clone())),
        Arc::new(LocalSessionManager::default()),
        server_config,
    );

    let router = if let Some(token) = &config.bearer_token {
        axum::Router::new().nest_service("/mcp", service).layer(
            axum::middleware::from_fn_with_state(
                Arc::<str>::from(token.as_str()),
                require_bearer_token,
            ),
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
    use crate::output::OutputFormat;
    use rstest::rstest;
    use serde_json::{Value, json};
    use tempfile::TempDir;

    impl DocumentInput {
        fn inline(markdown: &str) -> Self {
            Self {
                markdown: Some(markdown.to_string()),
                path: None,
            }
        }

        fn file(path: &str) -> Self {
            Self {
                markdown: None,
                path: Some(path.to_string()),
            }
        }
    }

    fn md(markdown: &str) -> MarkdownInput {
        MarkdownInput {
            doc: DocumentInput::inline(markdown),
            output: OutputOptions::default(),
        }
    }

    fn md_with(markdown: &str, output: OutputOptions) -> MarkdownInput {
        MarkdownInput {
            doc: DocumentInput::inline(markdown),
            output,
        }
    }

    fn query_for(markdown: &str, query: &str) -> QueryForMarkdown {
        QueryForMarkdown {
            doc: DocumentInput::inline(markdown),
            query: query.to_string(),
            timeout_ms: None,
            output: OutputOptions::default(),
        }
    }

    /// A server that may read `root/`, plus a sibling `outside/` it must not reach.
    fn server_with_files(files: &[(&str, &str)]) -> (TempDir, Server) {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(tmp.path().join("outside")).unwrap();
        std::fs::write(tmp.path().join("outside/secret.md"), "# Secret").unwrap();
        for (name, content) in files {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }

        let server = Server::new(EngineConfig {
            files: FileAccess::new([root], 1024 * 1024).unwrap(),
            ..EngineConfig::default()
        });
        (tmp, server)
    }

    fn error_text(result: CallToolResult) -> String {
        assert!(
            result.is_error.unwrap_or_default(),
            "expected an error result"
        );
        result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect()
    }

    fn structured(result: CallToolResult) -> Value {
        assert!(!result.is_error.unwrap_or_default(), "{:?}", result.content);
        result.structured_content.expect("structured content")
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
    #[case(
        QueryForHtml {
            html: Some("<h1>Test Heading</h1><p>This is a test paragraph.</p>".to_string()),
            path: None,
            query: Some(".h1".to_string()),
            timeout_ms: None,
            output: OutputOptions::default(),
        },
        Ok("# Test Heading")
    )]
    #[case(
        QueryForHtml {
            html: Some("<h1>Test Heading</h1><p>This is a test paragraph.</p>".to_string()),
            path: None,
            query: Some(".text".to_string()),
            timeout_ms: None,
            output: OutputOptions::default(),
        },
        Ok("Test Heading\n\nThis is a test paragraph.")
    )]
    #[case(
        QueryForHtml {
            html: Some("<h1>Test Heading</h1><p>This is a test paragraph.</p>".to_string()),
            path: None,
            query: None,
            timeout_ms: None,
            output: OutputOptions::default(),
        },
        Ok("# Test Heading\n\nThis is a test paragraph.")
    )]
    #[case(
        QueryForHtml {
            html: Some("<h1>Test Heading".to_string()), // malformed HTML
            path: None,
            query: Some(".h1".to_string()),
            timeout_ms: None,
            output: OutputOptions::default(),
        },
        Ok("# Test Heading")
    )]
    #[case(
        QueryForHtml {
            html: Some("<h1>Test Heading</h1>".to_string()),
            path: None,
            query: Some("not_a_function(".to_string()), // invalid query
            timeout_ms: None,
            output: OutputOptions::default(),
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
                assert_eq!(ok_texts(result).join("\n\n"), expected_text);
            }
            Err(expected_err) => {
                let msg = error_text(result.expect("tool errors are returned as is_error results"));
                assert!(
                    msg.contains(expected_err),
                    "Error message '{msg}' does not contain expected '{expected_err}'"
                );
            }
        }
    }

    #[test]
    fn test_html_to_markdown_reads_html_file() {
        let (_tmp, server) = server_with_files(&[("page.html", "<h1>From file</h1>")]);
        let result = server
            .html_to_markdown(Parameters(QueryForHtml {
                html: None,
                path: Some("page.html".to_string()),
                query: Some(".h1".to_string()),
                timeout_ms: None,
                output: OutputOptions::default(),
            }))
            .unwrap();
        assert_eq!(ok_texts(result), ["# From file"]);
    }

    #[rstest]
    #[case(query_for("# Test Heading", ".h1"), Ok("# Test Heading"))]
    #[case(
        query_for("# Test Heading\n\nThis is a test paragraph.", ".text"),
        Ok("Test Heading\n\nThis is a test paragraph.")
    )]
    #[case(
        query_for("# Test Heading\n\nThis is a test paragraph.", "identity()"),
        Ok("# Test Heading\n\nThis is a test paragraph.")
    )]
    #[case(query_for("# Test Heading", "not_a_function("), Err("Failed to query"))]
    #[case(query_for("", ".h1"), Ok(""))]
    fn test_extract_markdown(
        #[case] query: QueryForMarkdown,
        #[case] expected: Result<&'static str, &'static str>,
    ) {
        let server = Server::new(EngineConfig::default());
        let result = server.extract_markdown(Parameters(query));
        match expected {
            Ok(expected_text) => {
                let result = result.expect("Expected Ok result");
                assert_eq!(ok_texts(result).join("\n\n"), expected_text);
            }
            Err(expected_err) => {
                let msg = error_text(result.expect("tool errors are returned as is_error results"));
                assert!(
                    msg.contains(expected_err),
                    "Error message '{msg}' does not contain expected '{expected_err}'"
                );
            }
        }
    }

    #[test]
    fn test_query_error_points_to_validate_query() {
        let server = Server::new(EngineConfig::default());
        let msg = error_text(
            server
                .extract_markdown(Parameters(query_for("# a", "not_a_function(")))
                .unwrap(),
        );
        assert!(msg.contains("validate_query"), "{msg}");
    }

    #[derive(Debug, rmcp::serde::Deserialize)]
    struct BatchOutputForTest {
        documents: Vec<BatchDocumentResultForTest>,
    }

    #[derive(Debug, rmcp::serde::Deserialize)]
    struct BatchDocumentResultForTest {
        index: usize,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        results: Vec<String>,
        #[serde(default)]
        total: usize,
        #[serde(default)]
        next_offset: Option<usize>,
        #[serde(default)]
        error: Option<String>,
    }

    fn batch_doc(id: Option<&str>, markdown: &str) -> BatchDocument {
        BatchDocument {
            id: id.map(str::to_string),
            doc: DocumentInput::inline(markdown),
        }
    }

    fn batch(
        documents: Vec<BatchDocument>,
        query: &str,
        limit: Option<usize>,
        max_output_chars: Option<usize>,
    ) -> McpResult {
        Server::new(EngineConfig::default()).extract_markdown_batch(Parameters(
            QueryForMarkdownBatch {
                documents,
                query: query.to_string(),
                timeout_ms: None,
                limit,
                max_output_chars,
            },
        ))
    }

    fn batch_output(result: CallToolResult) -> Vec<BatchDocumentResultForTest> {
        let parsed: BatchOutputForTest = serde_json::from_value(structured(result)).unwrap();
        parsed.documents
    }

    #[test]
    fn test_extract_markdown_batch_mixes_success_and_per_document_error() {
        let result = batch(
            vec![
                batch_doc(Some("a.md"), "# Title A"),
                batch_doc(Some("b.md"), "# Title B"),
            ],
            ".h1",
            None,
            None,
        )
        .expect("batch call should succeed even if a document's query fails");

        // The text content mirrors the structured content, for clients that ignore the latter.
        assert!(
            result.content[0]
                .as_text()
                .unwrap()
                .text
                .contains("Title A")
        );
        let parsed = batch_output(result);

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].index, 0);
        assert_eq!(parsed[0].id.as_deref(), Some("a.md"));
        assert_eq!(parsed[0].results, vec!["# Title A".to_string()]);
        assert_eq!(parsed[0].total, 1);
        assert!(parsed[0].error.is_none());
        assert_eq!(parsed[1].id.as_deref(), Some("b.md"));
        assert_eq!(parsed[1].results, vec!["# Title B".to_string()]);
    }

    #[test]
    fn test_extract_markdown_batch_reports_error_without_aborting_other_documents() {
        let parsed = batch_output(
            batch(
                vec![
                    batch_doc(None, "not valid because "),
                    batch_doc(None, "# Fine"),
                ],
                "not_a_function(",
                None,
                None,
            )
            .expect("batch call itself should still succeed"),
        );

        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].results.is_empty());
        assert!(
            parsed[0]
                .error
                .as_deref()
                .unwrap()
                .contains("Failed to query")
        );
        assert!(parsed[1].results.is_empty());
        assert!(
            parsed[1]
                .error
                .as_deref()
                .unwrap()
                .contains("Failed to query")
        );
    }

    #[test]
    fn test_extract_markdown_batch_reads_files_and_reports_file_errors_per_document() {
        let (_tmp, server) = server_with_files(&[("a.md", "# From file")]);
        let result = server
            .extract_markdown_batch(Parameters(QueryForMarkdownBatch {
                documents: vec![
                    BatchDocument {
                        id: None,
                        doc: DocumentInput::file("a.md"),
                    },
                    BatchDocument {
                        id: None,
                        doc: DocumentInput::file("../outside/secret.md"),
                    },
                    batch_doc(Some("inline"), "# Inline"),
                ],
                query: ".h1".to_string(),
                timeout_ms: None,
                limit: None,
                max_output_chars: None,
            }))
            .unwrap();
        let parsed = batch_output(result);

        assert_eq!(
            parsed[0].id.as_deref(),
            Some("a.md"),
            "id defaults to the path"
        );
        assert_eq!(parsed[0].results, ["# From file"]);
        assert!(
            parsed[1]
                .error
                .as_deref()
                .unwrap()
                .contains("outside the allowed directories")
        );
        assert!(parsed[1].results.is_empty());
        assert_eq!(parsed[2].results, ["# Inline"]);
    }

    #[test]
    fn test_extract_markdown_batch_limit_and_output_budget() {
        let headings = "# One\n\n# Two\n\n# Three\n";
        let parsed = batch_output(
            batch(
                vec![batch_doc(None, headings), batch_doc(None, headings)],
                ".h1",
                Some(2),
                None,
            )
            .unwrap(),
        );
        assert_eq!(parsed[0].results.len(), 2);
        assert_eq!(parsed[0].total, 3);
        assert_eq!(parsed[0].next_offset, Some(2));

        // A budget that only the first document fits in: the second comes back empty but flagged.
        let long = format!("# {}\n", "x".repeat(300));
        let parsed = batch_output(
            batch(
                vec![batch_doc(None, &long), batch_doc(None, &long)],
                ".h1",
                None,
                Some(400),
            )
            .unwrap(),
        );
        assert_eq!(parsed[0].results.len(), 1);
        assert!(parsed[1].results.is_empty());
        assert_eq!(parsed[1].total, 1);
        assert_eq!(parsed[1].next_offset, Some(0));
    }

    #[rstest]
    #[case("# H1\n\n## H2\n\n### H3", vec!["# H1", "## H2", "### H3"])]
    #[case("No headings here.", vec![])]
    fn test_extract_headings(#[case] markdown: &str, #[case] expected: Vec<&str>) {
        let server = Server::new(EngineConfig::default());
        let result = server.extract_headings(Parameters(md(markdown))).unwrap();
        assert_eq!(ok_texts(result), expected);
    }

    #[rstest]
    #[case("```rust\nfn main() {}\n```\n\n```python\nprint(\"hi\")\n```", 2)]
    #[case("No code blocks.", 0)]
    fn test_extract_code_blocks(#[case] markdown: &str, #[case] count: usize) {
        let server = Server::new(EngineConfig::default());
        let result = server
            .extract_code_blocks(Parameters(md(markdown)))
            .unwrap();
        assert_eq!(ok_texts(result).len(), count);
    }

    #[rstest]
    #[case("- [ ] Buy milk\n- [x] Buy eggs\n- [ ] Buy bread", vec!["- [ ] Buy milk", "- [ ] Buy bread"])]
    #[case("- [x] Done task", vec![])]
    #[case("No list.", vec![])]
    fn test_extract_todos(#[case] markdown: &str, #[case] expected: Vec<&str>) {
        let server = Server::new(EngineConfig::default());
        let result = server.extract_todos(Parameters(md(markdown))).unwrap();
        assert_eq!(ok_texts(result), expected);
    }

    #[rstest]
    #[case("- [ ] Todo\n- [x] Done", vec!["- [x] Done"])]
    #[case("- [ ] Only todo", vec![])]
    fn test_extract_done_tasks(#[case] markdown: &str, #[case] expected: Vec<&str>) {
        let server = Server::new(EngineConfig::default());
        let result = server.extract_done_tasks(Parameters(md(markdown))).unwrap();
        assert_eq!(ok_texts(result), expected);
    }

    #[rstest]
    #[case("[Google](https://google.com) and [Rust](https://rust-lang.org)", 2)]
    #[case("No links here.", 0)]
    fn test_extract_links(#[case] markdown: &str, #[case] count: usize) {
        let server = Server::new(EngineConfig::default());
        let result = server.extract_links(Parameters(md(markdown))).unwrap();
        assert_eq!(ok_texts(result).len(), count);
    }

    #[rstest]
    #[case("![Alt](img.png) and ![Logo](logo.svg)", 2)]
    #[case("No images.", 0)]
    fn test_extract_images(#[case] markdown: &str, #[case] count: usize) {
        let server = Server::new(EngineConfig::default());
        let result = server.extract_images(Parameters(md(markdown))).unwrap();
        assert_eq!(ok_texts(result).len(), count);
    }

    #[rstest]
    // .table returns individual table cells; a 2-column × 2-row table (header + data) = 4 cells
    #[case("| A | B |\n|---|---|\n| 1 | 2 |", 4)]
    #[case("No tables.", 0)]
    fn test_extract_tables(#[case] markdown: &str, #[case] count: usize) {
        let server = Server::new(EngineConfig::default());
        let result = server.extract_tables(Parameters(md(markdown))).unwrap();
        assert_eq!(ok_texts(result).len(), count);
    }

    #[rstest]
    #[case("Hello world.\n\nSecond paragraph.", 2)]
    // from_html_str treats bare "# Only heading" as a single text paragraph (not a heading)
    #[case("# Only heading", 1)]
    fn test_extract_text(#[case] markdown: &str, #[case] count: usize) {
        let server = Server::new(EngineConfig::default());
        let result = server.extract_text(Parameters(md(markdown))).unwrap();
        assert_eq!(ok_texts(result).len(), count);
    }

    #[rstest]
    #[case("> A quote.\n\n> Another quote.", 2)]
    #[case("No blockquotes.", 0)]
    fn test_extract_blockquotes(#[case] markdown: &str, #[case] count: usize) {
        let server = Server::new(EngineConfig::default());
        let result = server
            .extract_blockquotes(Parameters(md(markdown)))
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
        let result = server.extract_sections(Parameters(md(SECTION_MD))).unwrap();
        let texts = ok_texts(result);
        assert!(!texts.is_empty());
        let joined = texts.join("\n");
        assert!(joined.contains("Introduction"));
        assert!(joined.contains("Installation"));
        assert!(joined.contains("Usage"));
    }

    fn section_input(markdown: &str, title: &str) -> ExtractSectionInput {
        ExtractSectionInput {
            doc: DocumentInput::inline(markdown),
            title: title.to_string(),
            output: OutputOptions::default(),
        }
    }

    #[rstest]
    #[case("Installation", true)]
    #[case("Nonexistent", false)]
    fn test_extract_section(#[case] title: &str, #[case] should_have_content: bool) {
        let server = Server::new(EngineConfig::default());
        let result = server
            .extract_section(Parameters(section_input(SECTION_MD, title)))
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
            .extract_section(Parameters(section_input(md, "Section \"quoted\"")))
            .unwrap();
        let texts = ok_texts(result);
        assert!(!texts.is_empty());
    }

    #[test]
    fn test_extract_toc() {
        let server = Server::new(EngineConfig::default());
        let result = server.extract_toc(Parameters(md(SECTION_MD))).unwrap();
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
        assert!(
            !instructions.contains("pass `path`"),
            "file access is off, so `path` shouldn't be advertised"
        );
    }

    #[test]
    fn test_get_info_advertises_roots_when_file_access_is_enabled() {
        let (_tmp, server) = server_with_files(&[]);
        let instructions = server.get_info().instructions.unwrap();
        assert!(
            instructions.contains("pass `path`") && instructions.contains("root"),
            "{instructions}"
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
                timeout_ms: Some(3_600_000),
                ..query_for("# a", "while(true): 1 end")
            }))
            .unwrap();
        assert!(result.is_error.unwrap_or_default());
        let msg = result.content[0].as_text().unwrap().text.to_lowercase();
        assert!(msg.contains("timed out"), "unexpected error: {msg}");
    }

    // --- reading files through `path` ---

    #[test]
    fn test_path_reads_file_inside_root() {
        let (_tmp, server) = server_with_files(&[("docs/guide.md", "# Guide\n\n## Part")]);
        let result = server
            .extract_headings(Parameters(MarkdownInput {
                doc: DocumentInput::file("docs/guide.md"),
                output: OutputOptions::default(),
            }))
            .unwrap();
        assert_eq!(ok_texts(result), ["# Guide", "## Part"]);
    }

    #[rstest]
    #[case::parent_traversal("../outside/secret.md")]
    #[case::nested_traversal("docs/../../outside/secret.md")]
    fn test_path_outside_root_is_rejected(#[case] path: &str) {
        let (_tmp, server) = server_with_files(&[("docs/guide.md", "# Guide")]);
        let result = server
            .extract_headings(Parameters(MarkdownInput {
                doc: DocumentInput::file(path),
                output: OutputOptions::default(),
            }))
            .unwrap();
        let msg = error_text(result);
        assert!(msg.contains("outside the allowed directories"), "{msg}");
        assert!(!msg.contains("Secret"), "file content must not leak: {msg}");
    }

    #[test]
    fn test_path_is_rejected_when_file_access_is_disabled() {
        let server = Server::new(EngineConfig::default());
        let result = server
            .extract_headings(Parameters(MarkdownInput {
                doc: DocumentInput::file("/etc/hosts"),
                output: OutputOptions::default(),
            }))
            .unwrap();
        assert!(error_text(result).contains("disabled"));
    }

    #[rstest]
    #[case::both(DocumentInput { markdown: Some("# a".to_string()), path: Some("a.md".to_string()) }, "not both")]
    #[case::neither(DocumentInput::default(), "`path`")]
    fn test_markdown_and_path_are_exclusive(#[case] doc: DocumentInput, #[case] expected: &str) {
        let (_tmp, server) = server_with_files(&[("a.md", "# a")]);
        let result = server
            .extract_headings(Parameters(MarkdownInput {
                doc,
                output: OutputOptions::default(),
            }))
            .unwrap();
        let msg = error_text(result);
        assert!(msg.contains(expected), "{msg}");
    }

    #[test]
    fn test_mq_builtins_cannot_bypass_the_allowed_directories() {
        let (tmp, server) = server_with_files(&[("inside.md", "# Inside")]);
        let secret = tmp.path().join("outside/secret.md");
        let inside = tmp.path().join("root/inside.md");

        for query in [
            format!(r#"read_file("{}")"#, secret.display()),
            format!(r#"read_file("{}")"#, inside.display()),
            format!(r#"file_exists("{}")"#, secret.display()),
            r#"env("HOME")"#.to_string(),
        ] {
            let result = server
                .extract_markdown(Parameters(query_for("# a", &query)))
                .unwrap();
            let outcome = if result.is_error.unwrap_or_default() {
                error_text(result)
            } else {
                ok_texts(result).join("\n")
            };
            assert!(
                !outcome.contains("Secret") && !outcome.contains("Inside"),
                "`{query}` reached the filesystem: {outcome}"
            );
            assert!(
                outcome.is_empty() || outcome.contains("Failed") || outcome.contains("false"),
                "`{query}` unexpectedly succeeded: {outcome}"
            );
        }
    }

    // --- paging, limits and formats ---

    const FIVE_HEADINGS: &str = "# One\n\n# Two\n\n# Three\n\n# Four\n\n# Five\n";

    #[test]
    fn test_limit_and_offset_page_through_results() {
        let server = Server::new(EngineConfig::default());
        let page = |offset, limit| {
            ok_texts(
                server
                    .extract_headings(Parameters(md_with(
                        FIVE_HEADINGS,
                        OutputOptions {
                            offset,
                            limit,
                            ..OutputOptions::default()
                        },
                    )))
                    .unwrap(),
            )
        };

        let first = page(None, Some(2));
        assert_eq!(&first[..2], ["# One", "# Two"]);
        assert_eq!(first.len(), 3, "two results plus the continuation note");
        assert!(
            first[2].contains("1-2 of 5") && first[2].contains("offset=2"),
            "{}",
            first[2]
        );

        let last = page(Some(3), None);
        assert_eq!(
            last,
            ["# Four", "# Five"],
            "no note when nothing is left out"
        );

        let past_end = page(Some(9), None);
        assert_eq!(past_end.len(), 1);
        assert!(past_end[0].contains("past the end"), "{}", past_end[0]);
    }

    #[test]
    fn test_output_is_cut_at_max_output_chars_and_says_how_to_continue() {
        let paragraphs = (0..10)
            .map(|i| format!("# H{i} {}", "x".repeat(80)))
            .collect::<Vec<_>>()
            .join("\n\n");
        let server = Server::new(EngineConfig {
            max_output_chars: 500,
            ..EngineConfig::default()
        });
        let texts = ok_texts(
            server
                .extract_headings(Parameters(md(&paragraphs)))
                .unwrap(),
        );

        let (note, results) = texts.split_last().unwrap();
        assert!(results.len() < 10 && !results.is_empty());
        assert!(results.iter().map(|r| r.chars().count()).sum::<usize>() <= 500);
        assert!(
            note.contains(&format!("offset={}", results.len())),
            "{note}"
        );

        // A caller can't lift the server's cap.
        let greedy = ok_texts(
            server
                .extract_headings(Parameters(md_with(
                    &paragraphs,
                    OutputOptions {
                        max_output_chars: Some(1_000_000),
                        ..OutputOptions::default()
                    },
                )))
                .unwrap(),
        );
        assert_eq!(greedy.len(), texts.len());
    }

    #[test]
    fn test_a_single_oversized_result_is_cut() {
        let big = format!("# {}", "y".repeat(5000));
        let server = Server::new(EngineConfig {
            max_output_chars: 300,
            ..EngineConfig::default()
        });
        let texts = ok_texts(server.extract_headings(Parameters(md(&big))).unwrap());
        assert_eq!(
            texts.len(),
            2,
            "the cut result and a note about it: {texts:?}"
        );
        assert!(texts[0].ends_with("[truncated]"));
        assert!(texts[0].chars().count() < 400);
        assert!(texts[1].contains("cut to 300 characters"), "{}", texts[1]);
    }

    #[test]
    fn test_json_format_describes_nodes_with_attributes_and_positions() {
        let markdown = "# Title\n\n```rust\nfn main() {}\n```\n\n[Rust](https://rust-lang.org)\n";
        let server = Server::new(EngineConfig::default());
        let json = OutputOptions {
            format: Some(OutputFormat::Json),
            ..OutputOptions::default()
        };

        let code = structured(
            server
                .extract_code_blocks(Parameters(md_with(markdown, json.clone())))
                .unwrap(),
        );
        let block = &code["results"][0];
        assert_eq!(block["type"], "code");
        assert_eq!(block["lang"], "rust");
        assert_eq!(block["value"], "fn main() {}");
        assert!(block["text"].as_str().unwrap().starts_with("```rust"));
        assert_eq!(block["position"]["start"]["line"], 3);
        assert_eq!(block["position"]["end"]["line"], 5);

        let heading = structured(
            server
                .extract_headings(Parameters(md_with(markdown, json.clone())))
                .unwrap(),
        );
        assert_eq!(heading["results"][0]["type"], "h1");
        assert_eq!(heading["results"][0]["depth"], 1);
        assert_eq!(heading["results"][0]["position"]["start"]["line"], 1);

        let links = structured(
            server
                .extract_links(Parameters(md_with(markdown, json)))
                .unwrap(),
        );
        assert_eq!(links["results"][0]["type"], "link");
        assert_eq!(links["results"][0]["url"], "https://rust-lang.org");
        assert_eq!(links["total"], 1);
        assert_eq!(links["returned"], 1);
        assert!(links["next_offset"].is_null());
    }

    #[test]
    fn test_json_format_pages_results() {
        let server = Server::new(EngineConfig::default());
        let json = |offset, limit| OutputOptions {
            format: Some(OutputFormat::Json),
            offset,
            limit,
            ..OutputOptions::default()
        };

        let page = structured(
            server
                .extract_headings(Parameters(md_with(FIVE_HEADINGS, json(Some(1), Some(2)))))
                .unwrap(),
        );
        assert_eq!(page["total"], 5);
        assert_eq!(page["offset"], 1);
        assert_eq!(page["returned"], 2);
        assert_eq!(page["next_offset"], 3);
        assert_eq!(page["results"][0]["text"], "# Two");

        // Computed results come back as nodes too, with their text.
        let result = server
            .extract_markdown(Parameters(QueryForMarkdown {
                output: json(None, None),
                ..query_for("# Title", r#""abc" | upcase()"#)
            }))
            .unwrap();
        let value = structured(result);
        assert_eq!(value["results"][0]["type"], "text");
        assert_eq!(value["results"][0]["text"], "ABC");
    }

    // --- new tools ---

    #[test]
    fn test_document_outline() {
        let (_tmp, server) = server_with_files(&[("doc.md", SECTION_MD)]);
        let outline = structured(
            server
                .document_outline(Parameters(OutlineInput {
                    doc: DocumentInput::file("doc.md"),
                    max_depth: None,
                }))
                .unwrap(),
        );

        assert_eq!(outline["stats"]["headings"], 3);
        let headings = outline["headings"].as_array().unwrap();
        assert_eq!(headings.len(), 3);
        assert_eq!(headings[0]["title"], "Introduction");
        assert_eq!(headings[0]["subsections"], 2);
        assert_eq!(headings[1]["title"], "Installation");
        assert_eq!(headings[1]["line"], 5);
        assert_eq!(headings[1]["end_line"], 8);
    }

    #[test]
    fn test_document_outline_reports_input_errors() {
        let server = Server::new(EngineConfig::default());
        let result = server
            .document_outline(Parameters(OutlineInput {
                doc: DocumentInput::default(),
                max_depth: None,
            }))
            .unwrap();
        assert!(error_text(result).contains("markdown"));
    }

    #[test]
    fn test_extract_frontmatter() {
        let server = Server::new(EngineConfig::default());
        let found = structured(
            server
                .extract_frontmatter(Parameters(DocumentInput::inline(
                    "---\ntitle: Hello\ntags: [a, b]\n---\n\n# Body\n",
                )))
                .unwrap(),
        );
        assert_eq!(found["found"], true);
        assert_eq!(found["format"], "yaml");
        assert_eq!(
            found["data"],
            json!({ "title": "Hello", "tags": ["a", "b"] })
        );

        let none = structured(
            server
                .extract_frontmatter(Parameters(DocumentInput::inline("# Body\n")))
                .unwrap(),
        );
        assert_eq!(none["found"], false);
    }

    fn validate(query: &str, sample: Option<&str>) -> Value {
        let server = Server::new(EngineConfig::default());
        structured(
            server
                .validate_query(Parameters(ValidateQueryInput {
                    query: query.to_string(),
                    sample: sample.map_or_else(DocumentInput::default, DocumentInput::inline),
                    timeout_ms: None,
                }))
                .unwrap(),
        )
    }

    #[test]
    fn test_validate_query_accepts_a_good_query() {
        let result = validate(".h2", None);
        assert_eq!(result["valid"], true);
        assert_eq!(result["issues"], json!([]));
        assert!(result.get("dry_run").is_none_or(Value::is_null));
    }

    #[test]
    fn test_validate_query_reports_syntax_errors_and_unknown_functions() {
        let syntax = validate("select(.code | upcase(", None);
        assert_eq!(syntax["valid"], false);
        assert_eq!(syntax["issues"][0]["kind"], "syntax");
        assert!(syntax["issues"][0]["line"].is_number());

        let unknown = validate("upcasee()", None);
        assert_eq!(unknown["valid"], false);
        assert_eq!(unknown["issues"][0]["kind"], "unresolved");
        assert!(
            unknown["issues"][0]["message"]
                .as_str()
                .unwrap()
                .contains("upcase")
        );
    }

    #[test]
    fn test_validate_query_dry_run() {
        let ok = validate(".h", Some(FIVE_HEADINGS));
        assert_eq!(ok["valid"], true);
        assert_eq!(ok["dry_run"]["match_count"], 5);
        assert_eq!(ok["dry_run"]["samples"].as_array().unwrap().len(), 3);
        assert_eq!(ok["dry_run"]["samples"][0], "# One");

        // A query that parses but fails at runtime is still "valid"; the failure is in dry_run.
        let runtime = validate("1 / 0", Some("# a"));
        assert_eq!(runtime["valid"], true);
        assert!(runtime["dry_run"]["error"].is_string(), "{runtime}");

        // No dry run when the query is already known to be broken.
        let broken = validate("upcasee()", Some("# a"));
        assert!(broken.get("dry_run").is_none_or(Value::is_null));
    }

    #[test]
    fn test_list_files() {
        let (_tmp, server) = server_with_files(&[
            ("a.md", ""),
            ("docs/b.md", "# b"),
            ("docs/page.html", "<p>x</p>"),
            (".hidden/c.md", ""),
        ]);

        let list =
            |input: ListFilesInput| structured(server.list_files(Parameters(input)).unwrap());
        let names = |value: &Value| -> Vec<String> {
            value["files"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| {
                    std::path::Path::new(f["path"].as_str().unwrap())
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .to_string()
                })
                .collect()
        };

        let all = list(ListFilesInput::default());
        assert_eq!(names(&all), ["a.md", "b.md"]);
        assert_eq!(all["truncated"], false);
        assert_eq!(all["roots"].as_array().unwrap().len(), 1);
        assert_eq!(all["files"][1]["size_bytes"], 3);

        let html = list(ListFilesInput {
            directory: Some("docs".to_string()),
            extensions: Some(vec![".html".to_string()]),
            limit: None,
        });
        assert_eq!(names(&html), ["page.html"]);

        let limited = list(ListFilesInput {
            limit: Some(1),
            ..ListFilesInput::default()
        });
        assert_eq!(limited["truncated"], true);

        // Every listed path is accepted by the other tools.
        let path = all["files"][1]["path"].as_str().unwrap();
        let headings = server
            .extract_headings(Parameters(MarkdownInput {
                doc: DocumentInput::file(path),
                output: OutputOptions::default(),
            }))
            .unwrap();
        assert_eq!(ok_texts(headings), ["# b"]);
    }

    #[test]
    fn test_list_files_rejects_outside_directory_and_disabled_access() {
        let (_tmp, server) = server_with_files(&[]);
        let result = server
            .list_files(Parameters(ListFilesInput {
                directory: Some("../outside".to_string()),
                ..ListFilesInput::default()
            }))
            .unwrap();
        assert!(error_text(result).contains("outside the allowed directories"));

        let disabled = Server::new(EngineConfig::default())
            .list_files(Parameters(ListFilesInput::default()))
            .unwrap();
        assert!(error_text(disabled).contains("disabled"));
    }

    #[test]
    fn test_tools_with_structured_output_declare_a_schema() {
        let server = Server::new(EngineConfig::default());
        let tools = server.tool_router.list_all();
        let has_schema = |name: &str| {
            tools
                .iter()
                .find(|t| t.name == name)
                .unwrap_or_else(|| panic!("missing tool {name}"))
                .output_schema
                .is_some()
        };

        for name in [
            "document_outline",
            "extract_frontmatter",
            "validate_query",
            "list_files",
            "extract_markdown_batch",
        ] {
            assert!(has_schema(name), "{name} should declare an output schema");
        }
        // These return text or JSON depending on `format`, so they can't promise one shape.
        for name in ["extract_markdown", "extract_headings", "html_to_markdown"] {
            assert!(
                !has_schema(name),
                "{name} must not declare an output schema"
            );
        }
    }

    #[test]
    fn test_inputs_accept_the_wire_format_of_flattened_parameters() {
        // MCP clients send flat JSON objects; flattened structs must accept them.
        let input: QueryForMarkdown = serde_json::from_value(json!({
            "path": "docs/a.md",
            "query": ".h",
            "format": "json",
            "limit": 5,
            "offset": 2,
            "max_output_chars": 1000,
            "timeout_ms": 500,
        }))
        .unwrap();
        assert_eq!(input.doc.path.as_deref(), Some("docs/a.md"));
        assert_eq!(input.output.format, Some(OutputFormat::Json));
        assert_eq!(input.output.limit, Some(5));
        assert_eq!(input.output.offset, Some(2));
        assert_eq!(input.output.max_output_chars, Some(1000));
        assert_eq!(input.timeout_ms, Some(500));

        let inline: MarkdownInput = serde_json::from_value(json!({ "markdown": "# a" })).unwrap();
        assert_eq!(inline.doc.markdown.as_deref(), Some("# a"));
        assert_eq!(inline.output.format, None);
    }
}
