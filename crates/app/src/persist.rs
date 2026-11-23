//! The open tabs, remembered between runs in a small text file: the format
//! is pure (text in, text out); only [`load`] and [`save`] touch the disk.
//!
//! ```text
//! invoke-git tabs v1
//! active 1
//! C:\work\one
//! C:\work\two
//! ```

use std::path::{Path, PathBuf};

const HEADER: &str = "invoke-git tabs v1";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Saved {
    pub paths: Vec<PathBuf>,
    /// Index into `paths` of the selected tab; none for the welcome screen.
    pub active: Option<usize>,
}

pub fn encode(saved: &Saved) -> String {
    let mut text = format!("{HEADER}\n");
    if let Some(active) = saved.active {
        text.push_str(&format!("active {active}\n"));
    }
    for path in &saved.paths {
        // A path with a line break cannot be written on one line, and a
        // path that is not UTF-8 cannot be written as text: skip both.
        if let Some(path) = path.to_str().filter(|p| !p.contains(['\n', '\r'])) {
            text.push_str(path);
            text.push('\n');
        }
    }
    text
}

/// Reads what [`encode`] wrote. Anything unreadable gives an empty list:
/// a damaged file must never stop the app from starting.
pub fn decode(text: &str) -> Saved {
    let mut lines = text.lines();
    if lines.next() != Some(HEADER) {
        return Saved::default();
    }
    let mut active = None;
    let mut paths = Vec::new();
    for (position, line) in lines.enumerate() {
        match line.strip_prefix("active ") {
            Some(index) if position == 0 => active = index.trim().parse::<usize>().ok(),
            _ if line.is_empty() => {}
            _ => paths.push(PathBuf::from(line)),
        }
    }
    let active = active.filter(|index| *index < paths.len());
    Saved { paths, active }
}

/// `%APPDATA%\Invoke Git\tabs.txt` on Windows, `$XDG_CONFIG_HOME` or
/// `~/.config` elsewhere. `INVOKE_GIT_DATA_DIR` replaces the folder (for
/// tests, and for anyone who wants a throwaway profile).
pub fn file() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("tabs.txt"))
}

/// The app's data folder (see [`file`]): where the tabs, the notes and
/// whatever comes next are kept. Never inside a repository.
pub fn data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("INVOKE_GIT_DATA_DIR") {
        return Some(PathBuf::from(dir));
    }
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")))?;
    Some(base.join("Invoke Git"))
}

/// The notes database.
pub fn notes_file() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("notes.sqlite"))
}

pub fn load() -> Saved {
    file()
        .and_then(|file| std::fs::read_to_string(file).ok())
        .map(|text| decode(&text))
        .unwrap_or_default()
}

/// Best effort: a failure to remember the tabs is not worth an error.
pub fn save(saved: &Saved) {
    let Some(file) = file() else { return };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(file, encode(saved));
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn round_trips_a_typical_session() {
        let saved = Saved {
            paths: vec![PathBuf::from(r"C:\work\one"), PathBuf::from(r"C:\work\two")],
            active: Some(1),
        };
        assert_eq!(decode(&encode(&saved)), saved);
    }

    #[test]
    fn garbage_gives_an_empty_session() {
        assert_eq!(decode(""), Saved::default());
        assert_eq!(decode("not our file\nC:\\a\n"), Saved::default());
    }

    #[test]
    fn an_active_index_past_the_end_is_dropped() {
        let text = format!("{HEADER}\nactive 5\nC:\\a\n");
        assert_eq!(decode(&text).active, None);
        assert_eq!(decode(&text).paths.len(), 1);
    }

    proptest! {
        #[test]
        fn round_trips_any_single_line_paths(
            paths in proptest::collection::vec("[^\r\n]{1,30}", 0..6),
            active in proptest::option::of(0usize..8),
        ) {
            let paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
            let active = active.filter(|index| *index < paths.len());
            let saved = Saved { paths, active };
            prop_assert_eq!(decode(&encode(&saved)), saved);
        }

        #[test]
        fn decode_never_panics(text in ".{0,200}") {
            let _ = decode(&text);
        }
    }
}
