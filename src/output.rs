use rmcp::{
    model::{CallToolResult, ContentBlock},
    schemars,
};
use serde_json::{Map, Value, json};

/// Default cap on the characters a single tool call may return.
pub const DEFAULT_MAX_OUTPUT_CHARS: usize = 100_000;

/// Smallest output budget. Anything lower can't hold a useful (truncated) result,
/// and a budget of zero would make paging with `offset` stall.
pub const MIN_OUTPUT_CHARS: usize = 200;

/// Rough per-result overhead of the JSON wrapper (type, position, attributes),
/// counted against the output budget in `json` format.
const JSON_ITEM_OVERHEAD: usize = 64;

/// Node attributes copied into a result in `json` format, when the node has them.
const ATTRIBUTES: &[&str] = &[
    "depth", "lang", "meta", "url", "title", "alt", "checked", "ordered", "level", "index", "row",
    "column", "ident", "name", "kind",
];

/// Node types whose raw content differs from their rendered Markdown (`text`).
const RAW_VALUE_TYPES: &[&str] = &[
    "code",
    "code_inline",
    "math",
    "math_inline",
    "yaml",
    "toml",
    "html",
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    /// One text block per result, rendered as Markdown.
    #[default]
    Text,
    /// A single JSON document: each result is an object with its type, text,
    /// type-specific attributes (`lang`, `url`, `depth`, ...) and source position.
    Json,
}

/// Options shared by every tool that returns a list of results.
#[derive(Debug, Clone, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct OutputOptions {
    #[schemars(
        description = "Output format: \"text\" (default, one Markdown block per result) or \"json\" (structured results with type, attributes and line/column positions)."
    )]
    pub format: Option<OutputFormat>,
    #[schemars(
        description = "Skip this many results before returning any (for paging). Default 0."
    )]
    pub offset: Option<usize>,
    #[schemars(
        description = "Return at most this many results. Default: as many as fit in max_output_chars."
    )]
    pub limit: Option<usize>,
    #[schemars(
        description = "Cap the total characters returned. Capped at the server's maximum; a result cut off by the cap says how to fetch the rest."
    )]
    pub max_output_chars: Option<usize>,
}

impl OutputOptions {
    /// The character budget for a call: the requested one, never above the server's.
    pub fn budget(&self, server_max: usize) -> usize {
        self.max_output_chars
            .unwrap_or(server_max)
            .min(server_max)
            .max(MIN_OUTPUT_CHARS)
    }
}

/// Which slice of a result list fits the limits.
#[derive(Debug, PartialEq, Eq)]
pub struct Page {
    pub start: usize,
    pub end: usize,
    pub total: usize,
    /// Offset to pass to get the results after this page, if any remain.
    pub next_offset: Option<usize>,
    /// Set when the only result returned is larger than the budget and was cut to it.
    pub cut_to: Option<usize>,
}

/// Picks the results from `offset` on that fit `limit` and `budget` (in characters).
///
/// If even the first result exceeds the budget, it is returned alone and flagged
/// for truncation, unless the budget is too small to be useful.
pub fn paginate(sizes: &[usize], offset: usize, limit: Option<usize>, budget: usize) -> Page {
    let total = sizes.len();
    let start = offset.min(total);
    let limit = limit.map_or(usize::MAX, |l| l.max(1));

    let mut end = start;
    let mut used = 0usize;
    let mut cut_to = None;
    while end < total && end - start < limit {
        let size = sizes[end];
        if used + size > budget {
            if end == start && budget >= MIN_OUTPUT_CHARS {
                cut_to = Some(budget);
                end += 1;
            }
            break;
        }
        used += size;
        end += 1;
    }

    Page {
        start,
        end,
        total,
        next_offset: (end < total).then_some(end),
        cut_to,
    }
}

/// Cuts `text` to at most `max_chars` characters, marking the cut.
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((byte_index, _)) => format!("{}… [truncated]", &text[..byte_index]),
        None => text.to_string(),
    }
}

fn position_json(node: &mq_markdown::Node) -> Option<Value> {
    node.position().map(|p| {
        json!({
            "start": { "line": p.start.line, "column": p.start.column },
            "end": { "line": p.end.line, "column": p.end.column },
        })
    })
}

/// Describes one query result as a JSON object: its type, rendered `text`, and for
/// Markdown nodes their attributes and source position.
pub fn describe(value: &mq_lang::RuntimeValue) -> Value {
    let text = value.to_string();
    let mq_lang::RuntimeValue::Markdown(node, _) = value else {
        return json!({ "type": value.name(), "text": text });
    };

    let kind = match node.name().as_str() {
        "" => "fragment".to_string(),
        name => name.to_string(),
    };

    let mut object = Map::new();
    object.insert("type".into(), json!(kind));
    object.insert("text".into(), json!(text));

    if RAW_VALUE_TYPES.contains(&kind.as_str()) {
        object.insert("value".into(), json!(node.value()));
    }
    for key in ATTRIBUTES {
        // Headings expose their depth under both `depth` and `level`.
        if *key == "level" && object.contains_key("depth") {
            continue;
        }
        let attribute = match node.attr(key) {
            Some(mq_markdown::AttrValue::String(s)) => json!(s),
            Some(mq_markdown::AttrValue::Integer(i)) => json!(i),
            Some(mq_markdown::AttrValue::Number(n)) => json!(n),
            Some(mq_markdown::AttrValue::Boolean(b)) => json!(b),
            _ => continue,
        };
        object.insert((*key).into(), attribute);
    }
    if let Some(position) = position_json(node) {
        object.insert("position".into(), position);
    }

    Value::Object(object)
}

/// Renders query results as a tool result, applying paging and the output budget.
///
/// In `text` format each result is its own content block, followed by a note when
/// results were left out. In `json` format the result is a single structured object.
pub fn render(
    values: &[mq_lang::RuntimeValue],
    options: &OutputOptions,
    server_max: usize,
) -> CallToolResult {
    let format = options.format.unwrap_or_default();
    let texts: Vec<String> = values.iter().map(|v| v.to_string()).collect();
    let overhead = if format == OutputFormat::Json {
        JSON_ITEM_OVERHEAD
    } else {
        0
    };
    let sizes: Vec<usize> = texts.iter().map(|t| t.chars().count() + overhead).collect();

    let page = paginate(
        &sizes,
        options.offset.unwrap_or_default(),
        options.limit,
        options.budget(server_max),
    );
    let cut_index = page.cut_to.map(|_| page.start);

    match format {
        OutputFormat::Text => {
            let mut blocks: Vec<ContentBlock> = (page.start..page.end)
                .map(|i| match page.cut_to.filter(|_| Some(i) == cut_index) {
                    Some(max) => ContentBlock::text(truncate_chars(&texts[i], max)),
                    None => ContentBlock::text(texts[i].clone()),
                })
                .collect();
            if let Some(note) = continuation_note(&page) {
                blocks.push(ContentBlock::text(note));
            }
            CallToolResult::success(blocks)
        }
        OutputFormat::Json => {
            let results: Vec<Value> = (page.start..page.end)
                .map(|i| {
                    let mut item = describe(&values[i]);
                    if let (Some(max), true) = (page.cut_to, Some(i) == cut_index) {
                        item["text"] = json!(truncate_chars(&texts[i], max));
                        item["truncated"] = json!(true);
                    }
                    item
                })
                .collect();

            CallToolResult::structured(json!({
                "results": results,
                "total": page.total,
                "offset": page.start,
                "returned": page.end - page.start,
                "next_offset": page.next_offset,
            }))
        }
    }
}

/// A short note telling the caller what was left out and how to get it.
pub fn continuation_note(page: &Page) -> Option<String> {
    let mut parts = Vec::new();
    if page.start > 0 && page.start >= page.total {
        parts.push(format!(
            "offset {} is past the end: there are only {} results.",
            page.start, page.total
        ));
    }
    if let Some(max) = page.cut_to {
        parts.push(format!(
            "Result {} was cut to {max} characters.",
            page.start + 1
        ));
    }
    if let Some(next) = page.next_offset {
        parts.push(format!(
            "Showing results {}-{} of {}; call again with offset={next} to continue.",
            page.start + 1,
            page.end,
            page.total
        ));
    }

    (!parts.is_empty()).then(|| format!("[{}]", parts.join(" ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::everything_fits(&[10, 10, 10], 0, None, 1000, Page { start: 0, end: 3, total: 3, next_offset: None, cut_to: None })]
    #[case::limit(&[10, 10, 10], 0, Some(2), 1000, Page { start: 0, end: 2, total: 3, next_offset: Some(2), cut_to: None })]
    #[case::offset(&[10, 10, 10], 1, None, 1000, Page { start: 1, end: 3, total: 3, next_offset: None, cut_to: None })]
    #[case::budget(&[300, 300, 300], 0, None, 700, Page { start: 0, end: 2, total: 3, next_offset: Some(2), cut_to: None })]
    #[case::offset_past_end(&[10], 5, None, 1000, Page { start: 1, end: 1, total: 1, next_offset: None, cut_to: None })]
    #[case::zero_limit_still_progresses(&[10, 10], 0, Some(0), 1000, Page { start: 0, end: 1, total: 2, next_offset: Some(1), cut_to: None })]
    #[case::oversize_first_is_cut(&[5000, 10], 0, None, 300, Page { start: 0, end: 1, total: 2, next_offset: Some(1), cut_to: Some(300) })]
    #[case::oversize_later_is_deferred(&[100, 5000], 0, None, 300, Page { start: 0, end: 1, total: 2, next_offset: Some(1), cut_to: None })]
    #[case::budget_too_small_returns_nothing(&[500], 0, None, 50, Page { start: 0, end: 0, total: 1, next_offset: Some(0), cut_to: None })]
    fn test_paginate(
        #[case] sizes: &[usize],
        #[case] offset: usize,
        #[case] limit: Option<usize>,
        #[case] budget: usize,
        #[case] expected: Page,
    ) {
        assert_eq!(paginate(sizes, offset, limit, budget), expected);
    }

    #[test]
    fn test_budget_is_clamped_to_server_max_and_minimum() {
        let options = |max| OutputOptions {
            max_output_chars: max,
            ..Default::default()
        };
        assert_eq!(options(None).budget(1000), 1000);
        assert_eq!(options(Some(500)).budget(1000), 500);
        assert_eq!(options(Some(9999)).budget(1000), 1000);
        assert_eq!(options(Some(0)).budget(1000), MIN_OUTPUT_CHARS);
    }

    #[test]
    fn test_describe_values_that_are_not_markdown_nodes() {
        assert_eq!(
            describe(&mq_lang::RuntimeValue::String("abc".to_string().into())),
            json!({ "type": "string", "text": "abc" })
        );
        assert_eq!(
            describe(&mq_lang::RuntimeValue::Boolean(true)),
            json!({ "type": "bool", "text": "true" })
        );
        assert_eq!(describe(&mq_lang::RuntimeValue::from(3))["type"], "number");
    }

    #[test]
    fn test_truncate_chars_respects_char_boundaries() {
        assert_eq!(truncate_chars("あいうえお", 2), "あい… [truncated]");
        assert_eq!(truncate_chars("abc", 3), "abc");
    }

    #[test]
    fn test_continuation_note() {
        let page = Page {
            start: 0,
            end: 2,
            total: 5,
            next_offset: Some(2),
            cut_to: None,
        };
        let note = continuation_note(&page).unwrap();
        assert!(
            note.contains("1-2 of 5") && note.contains("offset=2"),
            "{note}"
        );

        let complete = Page {
            start: 0,
            end: 5,
            total: 5,
            next_offset: None,
            cut_to: None,
        };
        assert_eq!(continuation_note(&complete), None);
    }
}
