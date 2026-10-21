use rmcp::schemars;

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct QueryCheck {
    /// True if the query has no syntax errors and calls only functions that exist.
    /// A runtime failure on the sample document is reported in `dry_run` instead.
    pub valid: bool,
    pub issues: Vec<QueryIssue>,
    /// Present when a sample document was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<DryRun>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct QueryIssue {
    /// `syntax` for a query that can't be parsed, `unresolved` for an unknown function, variable or module.
    pub kind: String,
    pub message: String,
    /// 1-based position in the query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct DryRun {
    /// How many results the query produced on the sample document.
    pub match_count: usize,
    /// The first few results, shortened.
    pub samples: Vec<String>,
    /// Set if running the query on the sample failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Number of results shown in a dry run.
pub const DRY_RUN_SAMPLES: usize = 3;
/// Longest sample shown in a dry run, in characters.
const SAMPLE_CHARS: usize = 200;

pub fn shorten(sample: &str) -> String {
    crate::output::truncate_chars(sample, SAMPLE_CHARS)
}

/// 1-based line and column of a byte `offset` in `text`.
fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let boundary = (0..=offset.min(text.len()))
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or_default();
    let before = &text[..boundary];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, column)
}

/// Finds problems in `query` without running it: syntax errors first, and if
/// there are none, names that don't resolve (with a "did you mean" suggestion).
pub fn check_query(engine: &mut mq_lang::DefaultEngine, query: &str) -> Vec<QueryIssue> {
    if let Err(error) = engine.compile(query) {
        let (line, column) = line_column(query, error.location.offset());
        return vec![QueryIssue {
            kind: "syntax".to_string(),
            message: error.to_string(),
            line: Some(line),
            column: Some(column),
        }];
    }

    let mut hir = mq_hir::Hir::default();
    hir.add_code(None, query);

    // The HIR only reports missing `include`d modules, not `import`ed ones.
    let missing_imports =
        hir.symbols()
            .filter_map(|(_, symbol)| match (&symbol.kind, &symbol.value) {
                (mq_hir::SymbolKind::Import(_), Some(name))
                    if engine.get_module_path(name).is_err() =>
                {
                    Some((
                        format!("Module not found: {name}"),
                        symbol.source.text_range.unwrap_or_default(),
                    ))
                }
                _ => None,
            });

    hir.error_ranges()
        .into_iter()
        .chain(missing_imports)
        .map(|(message, range)| QueryIssue {
            kind: "unresolved".to_string(),
            message,
            line: Some(range.start.line as usize),
            column: Some(range.start.column),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn engine() -> mq_lang::DefaultEngine {
        let mut engine = mq_lang::DefaultEngine::default();
        engine.load_builtin_module();
        engine
    }

    #[rstest]
    #[case::selector(".h2")]
    #[case::pipeline(r#"select(.code.lang == "rust") | upcase()"#)]
    #[case::module_import(r#"import "section" | section::sections() | section::toc()"#)]
    #[case::empty("")]
    fn test_valid_queries_have_no_issues(#[case] query: &str) {
        let issues = check_query(&mut engine(), query);
        assert!(issues.is_empty(), "{query}: {issues:?}");
    }

    #[test]
    fn test_syntax_error_is_reported_with_position() {
        let issues = check_query(&mut engine(), "select(.code\n| upcase(");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, "syntax");
        assert!(issues[0].line.is_some() && issues[0].column.is_some());
    }

    #[test]
    fn test_unknown_function_is_reported_with_suggestion() {
        let issues = check_query(&mut engine(), "upcasee()");
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].kind, "unresolved");
        assert!(
            issues[0].message.contains("upcase"),
            "{}",
            issues[0].message
        );
        assert_eq!((issues[0].line, issues[0].column), (Some(1), Some(1)));
    }

    #[test]
    fn test_unknown_module_is_reported() {
        for query in [r#"import "no_such_module""#, r#"include "no_such_module""#] {
            let issues = check_query(&mut engine(), query);
            assert_eq!(issues.len(), 1, "{query}: {issues:?}");
            assert!(
                issues[0].message.contains("no_such_module"),
                "{}",
                issues[0].message
            );
        }
    }

    #[test]
    fn test_line_column() {
        assert_eq!(line_column("abc", 0), (1, 1));
        assert_eq!(line_column("abc\ndef", 5), (2, 2));
        assert_eq!(line_column("あい", 3), (1, 2));
        assert_eq!(line_column("abc", 99), (1, 4));
    }
}
