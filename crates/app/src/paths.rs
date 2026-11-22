//! Comparing repo folders. Pure: no disk access.
//!
//! On Windows two spellings of one folder must open one tab: `C:\Repo`,
//! `c:\repo\` and `\\?\C:\Repo` are the same place. Paths from a file dialog
//! and from `discover` do not always agree on case, trailing separators or
//! the `\\?\` prefix.

use std::path::Path;

/// A key that is equal for two spellings of one folder.
pub fn comparable(path: &Path, case_insensitive: bool) -> String {
    let text = path.to_string_lossy();
    let text = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        text.into_owned()
    };
    let mut text = text.replace('\\', "/");
    // `//server/share` keeps its two leading slashes; everything else loses
    // trailing ones, except a bare root (`/`, `c:/`).
    while text.len() > 1 && text.ends_with('/') && !text.ends_with(":/") {
        text.pop();
    }
    if case_insensitive {
        text = text.to_lowercase();
    }
    text
}

/// Whether two paths name the same folder, by this platform's rules.
pub fn same_repo(a: &Path, b: &Path) -> bool {
    let insensitive = cfg!(any(windows, target_os = "macos"));
    comparable(a, insensitive) == comparable(b, insensitive)
}

/// The tab title: the folder's own name, or the whole path for a root.
pub fn title(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use proptest::prelude::*;

    use super::*;

    fn key(text: &str, insensitive: bool) -> String {
        comparable(&PathBuf::from(text), insensitive)
    }

    #[test]
    fn windows_spellings_of_one_folder_agree() {
        let plain = key(r"C:\Users\Acer\repo", true);
        assert_eq!(plain, key(r"c:\users\acer\repo\", true));
        assert_eq!(plain, key(r"\\?\C:\Users\Acer\repo", true));
        assert_eq!(plain, key("C:/Users/Acer/repo", true));
    }

    #[test]
    fn different_folders_stay_different() {
        assert_ne!(key(r"C:\a\repo", true), key(r"C:\a\repo2", true));
        assert_ne!(key(r"C:\a\repo", true), key(r"D:\a\repo", true));
    }

    #[test]
    fn case_matters_where_the_filesystem_cares() {
        assert_ne!(key("/home/a/Repo", false), key("/home/a/repo", false));
    }

    #[test]
    fn roots_keep_their_separator() {
        assert_eq!(key(r"C:\", true), "c:/");
        assert_eq!(key("/", false), "/");
    }

    #[test]
    fn unc_paths_keep_their_share() {
        assert_eq!(
            key(r"\\?\UNC\server\share\repo", true),
            key(r"\\server\share\repo\", true)
        );
    }

    #[test]
    fn title_is_the_folder_name() {
        assert_eq!(title(Path::new("/work/invoke-git")), "invoke-git");
    }

    proptest! {
        /// A key never changes when it is computed again from itself.
        #[test]
        fn the_key_is_stable(text in r"[A-Za-z0-9 ._\\/:-]{0,40}", insensitive: bool) {
            let once = key(&text, insensitive);
            prop_assert_eq!(key(&once, insensitive), once);
        }

        /// Trailing separators and the verbatim prefix never matter.
        #[test]
        fn decoration_does_not_matter(name in "[A-Za-z0-9._-]{1,12}", extra in 0usize..3) {
            let base = format!(r"C:\work\{name}");
            let decorated = format!(r"\\?\{base}{}", r"\".repeat(extra));
            prop_assert_eq!(key(&base, true), key(&decorated, true));
        }
    }
}
