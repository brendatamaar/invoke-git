//! Language detection: `.gitattributes` `linguist-language`, then file
//! extension, then shebang. Everything arrives as data.

/// The grammars shipped for v0.1. `Tsx` is the TypeScript grammar's JSX
/// variant; the rest are one per spec entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Language {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Go,
    Java,
    Kotlin,
    Python,
    Json,
    Yaml,
    Toml,
    Markdown,
    Html,
    Css,
    Sql,
    Bash,
}

impl Language {
    pub const ALL: [Language; 16] = [
        Language::Rust,
        Language::TypeScript,
        Language::Tsx,
        Language::JavaScript,
        Language::Go,
        Language::Java,
        Language::Kotlin,
        Language::Python,
        Language::Json,
        Language::Yaml,
        Language::Toml,
        Language::Markdown,
        Language::Html,
        Language::Css,
        Language::Sql,
        Language::Bash,
    ];

    /// Match a linguist language name (`Rust`, `Shell`, `TSX`), ignoring
    /// ASCII case and surrounding whitespace.
    pub fn from_linguist_name(name: &str) -> Option<Language> {
        Some(match name.trim().to_ascii_lowercase().as_str() {
            "rust" => Language::Rust,
            "typescript" => Language::TypeScript,
            "tsx" => Language::Tsx,
            "javascript" | "jsx" => Language::JavaScript,
            "go" => Language::Go,
            "java" => Language::Java,
            "kotlin" => Language::Kotlin,
            "python" => Language::Python,
            "json" | "json with comments" => Language::Json,
            "yaml" => Language::Yaml,
            "toml" => Language::Toml,
            "markdown" => Language::Markdown,
            "html" => Language::Html,
            "css" => Language::Css,
            "sql" => Language::Sql,
            "shell" | "bash" | "sh" => Language::Bash,
            _ => return None,
        })
    }

    /// Match a file name's extension (without the dot), ignoring ASCII case.
    pub fn from_extension(ext: &str) -> Option<Language> {
        Some(match ext.to_ascii_lowercase().as_str() {
            "rs" => Language::Rust,
            "ts" | "mts" | "cts" => Language::TypeScript,
            "tsx" => Language::Tsx,
            "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
            "go" => Language::Go,
            "java" => Language::Java,
            "kt" | "kts" => Language::Kotlin,
            "py" | "pyi" => Language::Python,
            "json" | "jsonc" => Language::Json,
            "yaml" | "yml" => Language::Yaml,
            "toml" => Language::Toml,
            "md" | "markdown" => Language::Markdown,
            "html" | "htm" => Language::Html,
            "css" => Language::Css,
            "sql" => Language::Sql,
            "sh" | "bash" => Language::Bash,
            _ => return None,
        })
    }

    /// Match a `#!` line: the interpreter, or the program after `env`.
    pub fn from_shebang(first_line: &str) -> Option<Language> {
        let rest = first_line.strip_prefix("#!")?;
        let mut words = rest.split_whitespace();
        let mut program = basename(words.next()?);
        if program == "env" {
            // `env -S python3 -u`, `env FOO=1 python3`: first plain word.
            program = basename(words.find(|w| !w.starts_with('-') && !w.contains('='))?);
        }
        if program == "sh" || program == "bash" {
            Some(Language::Bash)
        } else if program.starts_with("python") {
            Some(Language::Python)
        } else if matches!(program, "node" | "nodejs" | "deno" | "bun") {
            Some(Language::JavaScript)
        } else {
            None
        }
    }
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Pick the grammar for a file, or `None` for plain text.
///
/// - `linguist_language`: the value of the `linguist-language` attribute for
///   this path, if `.gitattributes` sets one. When set, it decides alone: a
///   name with no shipped grammar means plain text, and detection does not
///   fall through to the extension.
/// - `path`: the repo path, with `/` or `\` separators.
/// - `first_line`: the first line of the file, for a shebang. Only used when
///   the extension gives nothing.
pub fn detect(
    path: &str,
    linguist_language: Option<&str>,
    first_line: Option<&str>,
) -> Option<Language> {
    if let Some(name) = linguist_language {
        return Language::from_linguist_name(name);
    }
    let file = basename(path);
    // `.bashrc` has no extension in the `Path` sense; dotfiles are by name.
    let by_extension = match file.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => Language::from_extension(ext),
        _ => None,
    };
    by_extension
        .or_else(|| {
            matches!(file, ".bashrc" | ".bash_profile" | ".bash_login").then_some(Language::Bash)
        })
        .or_else(|| first_line.and_then(Language::from_shebang))
}
