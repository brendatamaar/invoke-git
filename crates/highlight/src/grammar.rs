//! Grammar and highlight-query loading. Queries are compiled once per
//! language and kept (pure memoisation, no IO).

use std::sync::OnceLock;

use tree_sitter::Query;

use crate::Language;

/// Highlight query for Kotlin, whose grammar crate ships none. Written for
/// `tree-sitter-kotlin-ng` node names; the grammar test guards it.
const KOTLIN_QUERY: &str = r#"
[
  "abstract" "annotation" "as" "as?" "by" "catch" "class" "companion" "const"
  "constructor" "data" "do" "else" "enum" "finally" "for" "fun" "if" "import"
  "in" "inline" "interface" "internal" "is" "lateinit" "object" "open"
  "override" "package" "private" "protected" "public" "return" "sealed"
  "try" "typealias" "val" "var" "when" "while" "throw"
] @keyword
(string_literal) @string
(multiline_string_literal) @string
(character_literal) @string
(number_literal) @number
(float_literal) @number
(line_comment) @comment
(block_comment) @comment
"#;

pub(crate) fn language(lang: Language) -> tree_sitter::Language {
    match lang {
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
        Language::Java => tree_sitter_java::LANGUAGE.into(),
        Language::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::Json => tree_sitter_json::LANGUAGE.into(),
        Language::Yaml => tree_sitter_yaml::LANGUAGE.into(),
        Language::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
        Language::Markdown => tree_sitter_md::LANGUAGE.into(),
        Language::Html => tree_sitter_html::LANGUAGE.into(),
        Language::Css => tree_sitter_css::LANGUAGE.into(),
        Language::Sql => tree_sitter_sequel::LANGUAGE.into(),
        Language::Bash => tree_sitter_bash::LANGUAGE.into(),
    }
}

fn query_source(lang: Language) -> String {
    match lang {
        Language::Rust => tree_sitter_rust::HIGHLIGHTS_QUERY.to_string(),
        // The TypeScript query only adds to the JavaScript one.
        Language::TypeScript | Language::Tsx => format!(
            "{}\n{}",
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_typescript::HIGHLIGHTS_QUERY
        ),
        Language::JavaScript => tree_sitter_javascript::HIGHLIGHT_QUERY.to_string(),
        Language::Go => tree_sitter_go::HIGHLIGHTS_QUERY.to_string(),
        Language::Java => tree_sitter_java::HIGHLIGHTS_QUERY.to_string(),
        Language::Kotlin => KOTLIN_QUERY.to_string(),
        Language::Python => tree_sitter_python::HIGHLIGHTS_QUERY.to_string(),
        Language::Json => tree_sitter_json::HIGHLIGHTS_QUERY.to_string(),
        Language::Yaml => tree_sitter_yaml::HIGHLIGHTS_QUERY.to_string(),
        Language::Toml => tree_sitter_toml_ng::HIGHLIGHTS_QUERY.to_string(),
        // Block structure only; the inline grammar is a second parse that
        // v0.1 does not do.
        Language::Markdown => tree_sitter_md::HIGHLIGHT_QUERY_BLOCK.to_string(),
        Language::Html => tree_sitter_html::HIGHLIGHTS_QUERY.to_string(),
        Language::Css => tree_sitter_css::HIGHLIGHTS_QUERY.to_string(),
        Language::Sql => tree_sitter_sequel::HIGHLIGHTS_QUERY.to_string(),
        Language::Bash => tree_sitter_bash::HIGHLIGHT_QUERY.to_string(),
    }
}

static QUERIES: [OnceLock<Result<Query, String>>; Language::ALL.len()] =
    [const { OnceLock::new() }; Language::ALL.len()];

pub(crate) fn query(lang: Language) -> Result<&'static Query, String> {
    QUERIES[lang as usize]
        .get_or_init(|| {
            Query::new(&language(lang), &query_source(lang))
                .map_err(|e| format!("{lang:?} highlight query: {e}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}
