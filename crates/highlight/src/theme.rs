//! Capture names to design tokens.
//!
//! `docs/design/README.md` defines no syntax colours yet, so this is the
//! temporary mapping from `docs/PLAN.md` Phase 0 (see tasks/009.DONE.md).
//! Colours come only from the `tokens` crate.

use tokens::{Rgba, Theme};

/// The short fixed list of capture kinds. Everything a grammar's query
/// captures that is not on this list (variables, punctuation, operators,
/// properties) stays in the default ink colour and produces no span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Capture {
    Keyword,
    String,
    Comment,
    Type,
    Function,
    Number,
}

impl Capture {
    pub const ALL: [Capture; 6] = [
        Capture::Keyword,
        Capture::String,
        Capture::Comment,
        Capture::Type,
        Capture::Function,
        Capture::Number,
    ];

    /// Map a query capture name (`@function.method`, `@keyword.return`, ...)
    /// by its first dotted segment. `None` means "leave unstyled".
    pub fn from_name(name: &str) -> Option<Capture> {
        let head = name.split('.').next().unwrap_or(name);
        Some(match head {
            "keyword" | "conditional" | "repeat" | "include" | "storageclass" | "exception" => {
                Capture::Keyword
            }
            "string" | "character" => Capture::String,
            "comment" => Capture::Comment,
            "type" | "constructor" | "tag" => Capture::Type,
            "function" | "method" => Capture::Function,
            "number" | "float" | "boolean" | "constant" => Capture::Number,
            "text" if name == "text.title" => Capture::Keyword,
            _ => return None,
        })
    }

    /// The token name from `docs/design/tokens.json`.
    pub const fn token_name(self) -> &'static str {
        match self {
            Capture::Keyword => "accent",
            Capture::String => "add-ink",
            Capture::Comment => "ink-faint",
            Capture::Type => "lane-2",
            Capture::Function => "lane-7",
            Capture::Number => "warning",
        }
    }

    /// The token colour in `theme`.
    pub const fn token(self, theme: Theme) -> Rgba {
        let p = theme.palette();
        match self {
            Capture::Keyword => p.accent,
            Capture::String => p.add_ink,
            Capture::Comment => p.ink_faint,
            Capture::Type => p.lane_2,
            Capture::Function => p.lane_7,
            Capture::Number => p.warning,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_matches_its_name() {
        for theme in Theme::ALL {
            let by_name: std::collections::HashMap<_, _> =
                theme.palette().colors().into_iter().collect();
            for c in Capture::ALL {
                assert_eq!(by_name[c.token_name()], c.token(theme), "{c:?}");
            }
        }
    }

    #[test]
    fn names_map_by_first_segment() {
        assert_eq!(Capture::from_name("keyword.return"), Some(Capture::Keyword));
        assert_eq!(
            Capture::from_name("function.method"),
            Some(Capture::Function)
        );
        assert_eq!(Capture::from_name("string.escape"), Some(Capture::String));
        assert_eq!(Capture::from_name("variable"), None);
        assert_eq!(Capture::from_name("punctuation.bracket"), None);
    }
}
