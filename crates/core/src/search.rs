//! Parsed commit-search filters and pure matching.

/// The field selected by the caller for a search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchField {
    Message,
    Author,
    Hash,
    Path,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryError {
    Empty,
    InvalidHash,
    InvalidPath,
}

/// One literal filter. Text matches case-insensitively, and paths are
/// case-sensitive Git paths with slash separators.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchQuery {
    field: SearchField,
    value: String,
}

impl SearchQuery {
    pub fn parse(field: SearchField, input: &str) -> Result<Self, QueryError> {
        let value = if field == SearchField::Path {
            input
        } else {
            input.trim()
        };
        if value.is_empty() {
            return Err(QueryError::Empty);
        }
        let value = match field {
            SearchField::Message | SearchField::Author => value.to_lowercase(),
            SearchField::Hash => {
                if value.len() > 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(QueryError::InvalidHash);
                }
                value.to_ascii_lowercase()
            }
            SearchField::Path => {
                if value.contains('\0')
                    || value.split('/').any(|part| matches!(part, "" | "." | ".."))
                {
                    return Err(QueryError::InvalidPath);
                }
                value.to_owned()
            }
        };
        Ok(Self { field, value })
    }

    pub fn field(&self) -> SearchField {
        self.field
    }

    /// Match a hash filter against a canonical lowercase hex id.
    pub fn matches_hash(&self, hash: &str) -> bool {
        self.field == SearchField::Hash && hash.starts_with(&self.value)
    }

    pub fn matches_text(&self, message: &str, name: &str, email: &str) -> bool {
        match self.field {
            SearchField::Message => message.to_lowercase().contains(&self.value),
            SearchField::Author => {
                name.to_lowercase().contains(&self.value)
                    || email.to_lowercase().contains(&self.value)
            }
            SearchField::Hash | SearchField::Path => false,
        }
    }

    pub fn path(&self) -> Option<&str> {
        (self.field == SearchField::Path).then_some(self.value.as_str())
    }

    /// Compare the entries for the query's exact path. Callers supply an
    /// identity including both object id and mode, or `None` for absence.
    pub fn matches_path_change<T: Eq>(&self, before: Option<T>, after: Option<T>) -> bool {
        self.field == SearchField::Path && before != after
    }
}
