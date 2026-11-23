//! The notes store as the screens use it: one database for the whole app,
//! opened the first time it is needed, and the repo key and the clock that
//! the store itself does not have. Every call does file IO: run it on a
//! background thread.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use invoke_core::notes::{Anchor, Note, NoteId};
use shell::store::Store;

pub struct NotesDb {
    /// `None` when the app has no data folder (no notes then, with a reason).
    file: Option<PathBuf>,
    store: Mutex<Option<Store>>,
}

/// The key a repo's notes are kept under: its folder, spelled the same way
/// whatever way it was opened (case and separators on Windows).
pub fn repo_key(workdir: &Path) -> String {
    crate::paths::comparable(workdir, cfg!(any(windows, target_os = "macos")))
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

impl NotesDb {
    pub fn new(file: Option<PathBuf>) -> Self {
        Self {
            file,
            store: Mutex::new(None),
        }
    }

    /// The database of this run of the app.
    pub fn global() -> Arc<NotesDb> {
        static DB: OnceLock<Arc<NotesDb>> = OnceLock::new();
        DB.get_or_init(|| Arc::new(NotesDb::new(crate::persist::notes_file())))
            .clone()
    }

    fn run<T>(&self, work: impl FnOnce(&Store) -> Result<T, String>) -> Result<T, String> {
        let mut slot = self.store.lock().map_err(|_| {
            "the notes store was left in a bad state; restart Invoke Git".to_owned()
        })?;
        if slot.is_none() {
            let file = self
                .file
                .as_ref()
                .ok_or_else(|| "there is no folder to keep notes in".to_owned())?;
            *slot = Some(Store::open(file)?);
        }
        match slot.as_ref() {
            Some(store) => work(store),
            None => Err("the notes store is not open".to_owned()),
        }
    }

    pub fn notes(&self, repo: &str) -> Result<Vec<Note>, String> {
        self.run(|store| store.notes(repo))
    }

    pub fn add(
        &self,
        repo: &str,
        branch: &str,
        anchor: &Anchor,
        body: &str,
        todo: bool,
    ) -> Result<Note, String> {
        self.run(|store| store.add_note(repo, branch, anchor, body, todo, now()))
    }

    pub fn set_resolved(&self, repo: &str, id: NoteId, resolved: bool) -> Result<bool, String> {
        self.run(|store| store.set_resolved(repo, id, resolved, now()))
    }

    pub fn delete(&self, repo: &str, id: NoteId) -> Result<bool, String> {
        self.run(|store| store.delete_note(repo, id))
    }

    pub fn reanchor(
        &self,
        repo: &str,
        id: NoteId,
        commit: &str,
        line: u32,
    ) -> Result<bool, String> {
        self.run(|store| store.reanchor(repo, id, commit, line))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_spellings_of_a_folder_are_one_repo() {
        let a = repo_key(Path::new(r"C:\Users\Me\Repo"));
        let b = repo_key(Path::new(r"c:\users\me\repo\"));
        if cfg!(windows) {
            assert_eq!(a, b);
        }
        assert_ne!(repo_key(Path::new("/a/one")), repo_key(Path::new("/a/two")));
    }

    #[test]
    fn without_a_data_folder_there_is_a_reason_not_a_panic() {
        let db = NotesDb::new(None);
        assert!(db.notes("r").unwrap_err().contains("no folder"));
    }

    #[test]
    fn the_store_opens_on_first_use_and_keeps_notes() {
        let dir = std::env::temp_dir().join(format!("invoke-notesdb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = NotesDb::new(Some(dir.join("notes.sqlite")));
        let anchor = Anchor {
            commit: "a".repeat(40),
            path: "x.rs".into(),
            line: 2,
            text: "let a = 1;".into(),
        };
        let note = db.add("r", "main", &anchor, "look", true).unwrap();
        assert!(note.created > 1_600_000_000);
        assert_eq!(db.notes("r").unwrap(), vec![note.clone()]);
        assert!(db.set_resolved("r", note.id, true).unwrap());
        assert!(db.notes("r").unwrap()[0].resolved);
        assert!(db.delete("r", note.id).unwrap());
        assert!(db.notes("r").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
