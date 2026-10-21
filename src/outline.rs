use mq_markdown::Node;
use rmcp::schemars;
use serde_json::Value;
use std::collections::BTreeMap;

/// Most headings listed in an outline; the counts in `stats` always cover the whole document.
pub const MAX_OUTLINE_HEADINGS: usize = 500;

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct Outline {
    /// Totals for the whole document.
    pub stats: Stats,
    /// Front matter format (`yaml` or `toml`), if the document starts with front matter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frontmatter_format: Option<String>,
    /// Headings in document order, each with the extent of its section.
    pub headings: Vec<OutlineHeading>,
    /// True if `headings` was cut at the server's limit.
    pub headings_truncated: bool,
}

#[derive(Debug, Default, serde::Serialize, schemars::JsonSchema)]
pub struct Stats {
    pub chars: usize,
    pub lines: usize,
    pub headings: usize,
    pub code_blocks: usize,
    /// Code blocks per language (`none` for blocks without one).
    pub code_languages: BTreeMap<String, usize>,
    pub tables: usize,
    pub links: usize,
    pub images: usize,
    pub blockquotes: usize,
    /// Unchecked task list items.
    pub todos: usize,
    /// Checked task list items.
    pub done_tasks: usize,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct OutlineHeading {
    /// Heading depth, 1 for `#` through 6.
    pub level: u8,
    pub title: String,
    /// 1-based line of the heading.
    pub line: usize,
    /// 1-based last line of the section: up to the next heading of the same or a shallower level.
    pub end_line: usize,
    /// Characters in the section, including its heading and subsections.
    pub chars: usize,
    /// Number of headings nested inside this section.
    pub subsections: usize,
}

/// Summarises a document's structure so a caller can decide what to read without fetching all of it.
pub fn outline(source: &str, nodes: &[Node], max_depth: Option<u8>) -> Outline {
    let mut stats = Stats {
        chars: source.chars().count(),
        lines: source.lines().count(),
        ..Stats::default()
    };
    for node in nodes {
        count(node, &mut stats);
    }

    // Prefix sums of characters per line, so a section's size is a subtraction.
    let mut chars_before_line = vec![0usize];
    for line in source.split_inclusive('\n') {
        chars_before_line
            .push(chars_before_line.last().copied().unwrap_or_default() + line.chars().count());
    }
    let last_line = stats.lines.max(1);

    let headings: Vec<(u8, String, usize)> = nodes
        .iter()
        .filter_map(|node| match node {
            Node::Heading(h) => node
                .position()
                .map(|p| (h.depth, node.value(), p.start.line)),
            _ => None,
        })
        .collect();

    let listed: Vec<OutlineHeading> = headings
        .iter()
        .enumerate()
        .filter(|(_, (level, _, _))| max_depth.is_none_or(|max| *level <= max))
        .map(|(i, (level, title, line))| {
            let rest = &headings[i + 1..];
            let end_line = rest
                .iter()
                .find(|(l, _, _)| l <= level)
                .map_or(last_line, |(_, _, next_line)| {
                    next_line.saturating_sub(1).max(*line)
                });
            let subsections = rest.iter().take_while(|(l, _, _)| l > level).count();
            let chars = chars_before_line
                .get(end_line)
                .zip(chars_before_line.get(line - 1))
                .map_or(0, |(end, start)| end - start);

            OutlineHeading {
                level: *level,
                title: title.clone(),
                line: *line,
                end_line,
                chars,
                subsections,
            }
        })
        .collect();

    let headings_truncated = listed.len() > MAX_OUTLINE_HEADINGS;
    Outline {
        stats,
        frontmatter_format: frontmatter(nodes).map(|f| f.format),
        headings: listed.into_iter().take(MAX_OUTLINE_HEADINGS).collect(),
        headings_truncated,
    }
}

fn count(node: &Node, stats: &mut Stats) {
    match node {
        Node::Heading(_) => stats.headings += 1,
        Node::Code(code) => {
            stats.code_blocks += 1;
            let lang = code
                .lang
                .as_deref()
                .filter(|l| !l.is_empty())
                .unwrap_or("none");
            *stats.code_languages.entry(lang.to_string()).or_default() += 1;
        }
        // A table is flattened into rows and cells; each has exactly one alignment row.
        Node::TableAlign(_) => stats.tables += 1,
        Node::Link(_) => stats.links += 1,
        Node::Image(_) => stats.images += 1,
        Node::Blockquote(_) => stats.blockquotes += 1,
        Node::List(list) => match list.checked {
            Some(false) => stats.todos += 1,
            Some(true) => stats.done_tasks += 1,
            None => {}
        },
        _ => {}
    }
    for child in node.children() {
        count(&child, stats);
    }
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct FrontmatterResult {
    /// Whether the document starts with front matter.
    pub found: bool,
    /// `yaml` or `toml`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// The parsed front matter, as JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// The front matter source text, without the `---` / `+++` fences.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    /// Why `data` is missing even though front matter was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_error: Option<String>,
}

pub struct Frontmatter {
    pub format: String,
    pub raw: String,
}

/// Finds the front matter node, which only counts as such at the very start of the document.
pub fn frontmatter(nodes: &[Node]) -> Option<Frontmatter> {
    match nodes.first()? {
        Node::Yaml(yaml) => Some(Frontmatter {
            format: "yaml".to_string(),
            raw: yaml.value.clone(),
        }),
        Node::Toml(toml) => Some(Frontmatter {
            format: "toml".to_string(),
            raw: toml.value.clone(),
        }),
        _ => None,
    }
}

/// Parses the document's front matter into JSON.
pub fn parse_frontmatter(nodes: &[Node]) -> FrontmatterResult {
    let Some(Frontmatter { format, raw }) = frontmatter(nodes) else {
        return FrontmatterResult {
            found: false,
            format: None,
            data: None,
            raw: None,
            parse_error: None,
        };
    };

    let parsed = match format.as_str() {
        "toml" => toml::from_str::<Value>(&raw).map_err(|e| e.to_string()),
        _ => parse_yaml(&raw),
    };
    let (data, parse_error) = match parsed {
        Ok(value) => (Some(value), None),
        Err(error) => (None, Some(error)),
    };

    FrontmatterResult {
        found: true,
        format: Some(format),
        data,
        raw: Some(raw),
        parse_error,
    }
}

fn parse_yaml(raw: &str) -> Result<Value, String> {
    let documents = yaml_rust2::YamlLoader::load_from_str(raw).map_err(|e| e.to_string())?;
    Ok(documents.first().map_or(Value::Null, yaml_to_json))
}

fn yaml_to_json(yaml: &yaml_rust2::Yaml) -> Value {
    use yaml_rust2::Yaml;

    match yaml {
        Yaml::Integer(i) => Value::from(*i),
        Yaml::Real(s) => s
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map_or_else(|| Value::String(s.clone()), Value::Number),
        Yaml::String(s) => Value::String(s.clone()),
        Yaml::Boolean(b) => Value::Bool(*b),
        Yaml::Array(items) => Value::Array(items.iter().map(yaml_to_json).collect()),
        Yaml::Hash(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (yaml_key(k), yaml_to_json(v)))
                .collect(),
        ),
        Yaml::Null | Yaml::Alias(_) | Yaml::BadValue => Value::Null,
    }
}

/// JSON object keys are strings, so scalar YAML keys are stringified.
fn yaml_key(key: &yaml_rust2::Yaml) -> String {
    use yaml_rust2::Yaml;

    match key {
        Yaml::String(s) | Yaml::Real(s) => s.clone(),
        Yaml::Integer(i) => i.to_string(),
        Yaml::Boolean(b) => b.to_string(),
        other => yaml_to_json(other).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn nodes(markdown: &str) -> Vec<Node> {
        mq_markdown::Markdown::from_markdown_str(markdown)
            .unwrap()
            .nodes
    }

    const DOC: &str = "\
# Title\n\
\n\
Intro [link](https://example.com).\n\
\n\
## Setup\n\
\n\
```rust\n\
fn main() {}\n\
```\n\
\n\
- [ ] todo\n\
- [x] done\n\
\n\
## Usage\n\
\n\
| A | B |\n\
|---|---|\n\
| 1 | 2 |\n\
\n\
### Details\n\
\n\
> quote\n\
\n\
![img](a.png)\n";

    #[test]
    fn test_outline_stats() {
        let outline = outline(DOC, &nodes(DOC), None);
        let stats = &outline.stats;
        assert_eq!(stats.chars, DOC.chars().count());
        assert_eq!(stats.headings, 4);
        assert_eq!(stats.code_blocks, 1);
        assert_eq!(stats.code_languages.get("rust"), Some(&1));
        assert_eq!(stats.tables, 1);
        assert_eq!(stats.links, 1);
        assert_eq!(stats.images, 1);
        assert_eq!(stats.blockquotes, 1);
        assert_eq!(stats.todos, 1);
        assert_eq!(stats.done_tasks, 1);
    }

    #[test]
    fn test_outline_section_extents() {
        let outline = outline(DOC, &nodes(DOC), None);
        let by_title = |title: &str| outline.headings.iter().find(|h| h.title == title).unwrap();

        let title = by_title("Title");
        assert_eq!((title.level, title.line, title.subsections), (1, 1, 3));
        assert_eq!(title.end_line, DOC.lines().count());
        assert_eq!(title.chars, DOC.chars().count());

        let setup = by_title("Setup");
        assert_eq!((setup.line, setup.subsections), (5, 0));
        assert_eq!(setup.end_line, 13, "Setup ends right before ## Usage");

        let usage = by_title("Usage");
        assert_eq!(usage.subsections, 1);
        assert_eq!(usage.end_line, DOC.lines().count());
    }

    #[test]
    fn test_outline_max_depth_limits_listed_headings_only() {
        let outline = outline(DOC, &nodes(DOC), Some(2));
        assert_eq!(outline.headings.len(), 3);
        assert_eq!(outline.stats.headings, 4);
    }

    #[test]
    fn test_outline_of_empty_document() {
        let outline = outline("", &nodes(""), None);
        assert!(outline.headings.is_empty());
        assert_eq!(outline.stats.chars, 0);
    }

    #[test]
    fn test_outline_reports_frontmatter_format() {
        let md = "---\ntitle: x\n---\n\n# H\n";
        assert_eq!(
            outline(md, &nodes(md), None).frontmatter_format.as_deref(),
            Some("yaml")
        );
        assert_eq!(outline(DOC, &nodes(DOC), None).frontmatter_format, None);
    }

    #[test]
    fn test_parse_yaml_frontmatter() {
        let md = "---\ntitle: Hello\ncount: 3\nratio: 1.5\ndraft: true\ntags:\n  - a\n  - b\nnested:\n  key: value\nempty:\n---\n\n# H\n";
        let result = parse_frontmatter(&nodes(md));
        assert!(result.found);
        assert_eq!(result.format.as_deref(), Some("yaml"));
        assert_eq!(
            result.data,
            Some(json!({
                "title": "Hello",
                "count": 3,
                "ratio": 1.5,
                "draft": true,
                "tags": ["a", "b"],
                "nested": { "key": "value" },
                "empty": null,
            }))
        );
        assert!(result.raw.unwrap().contains("title: Hello"));
        assert!(result.parse_error.is_none());
    }

    #[test]
    fn test_parse_toml_frontmatter() {
        let md = "+++\ntitle = \"Hello\"\ntags = [\"a\", \"b\"]\n+++\n\n# H\n";
        let result = parse_frontmatter(&nodes(md));
        assert_eq!(result.format.as_deref(), Some("toml"));
        assert_eq!(
            result.data,
            Some(json!({ "title": "Hello", "tags": ["a", "b"] }))
        );
    }

    #[test]
    fn test_invalid_frontmatter_keeps_raw_and_reports_error() {
        let md = "---\ntitle: [unclosed\n---\n\n# H\n";
        let result = parse_frontmatter(&nodes(md));
        assert!(result.found);
        assert!(result.data.is_none());
        assert!(result.parse_error.is_some());
        assert!(result.raw.unwrap().contains("unclosed"));
    }

    #[test]
    fn test_no_frontmatter() {
        let result = parse_frontmatter(&nodes("# Just a heading\n"));
        assert!(!result.found);
        assert!(result.format.is_none() && result.data.is_none() && result.raw.is_none());
    }
}
