//! The notes store as the screens use it: one database for the whole app,
//! opened the first time it is needed, and the repo key and the clock that
//! the store itself does not have. Every call does file IO: run it on a
//! background thread.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use invoke_core::UndoEntry;
use invoke_core::notes::{Anchor, Note, NoteId};
use shell::store::Store;

pub struct NotesDb {
    /// `None` when the app has no data folder (no notes then, with a reason).
    file: Option<PathBuf>,
    store: Mutex<Option<Store>>,
    /// The newest generation of undo points saved, by repo.
    saved: Mutex<std::collections::HashMap<String, u64>>,
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
            saved: Mutex::default(),
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

    /// The undo points kept for `repo` (oldest first), and how many rows were
    /// unreadable.
    pub fn load_undo(&self, repo: &str) -> Result<(Vec<UndoEntry>, usize), String> {
        self.run(|store| store.load_undo(repo))
    }

    /// Keeps `entries` as the undo points of `repo`. `generation` numbers the
    /// saves in the order they were asked for: two saves may run out of
    /// order on background threads, and an older one must not overwrite a
    /// newer one.
    pub fn save_undo(
        &self,
        repo: &str,
        generation: u64,
        entries: Vec<UndoEntry>,
    ) -> Result<(), String> {
        let mut latest = self
            .saved
            .lock()
            .map_err(|_| "the notes store was left in a bad state".to_owned())?;
        if latest.get(repo).is_some_and(|last| *last >= generation) {
            return Ok(());
        }
        self.run(|store| store.save_undo(repo, entries))?;
        latest.insert(repo.to_owned(), generation);
        Ok(())
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

    fn point(op: u64) -> UndoEntry {
        use invoke_core::{
            HeadTarget, IndexEffect, IndexId, Oid, OpId, Reversal, Reversible, UndoPoint,
        };
        UndoEntry {
            point: UndoPoint {
                op: OpId(op),
                label: format!("op {op}"),
                head: HeadTarget::Detached,
                refs: Vec::new(),
                index: IndexId::Absent,
                index_effect: IndexEffect::Untouched,
                reverse: Reversal::Refs,
            },
            tree: Oid::from_hex(&"a".repeat(40)).unwrap(),
            after_refs: Vec::new(),
            after_index: IndexId::Absent,
            after_head: HeadTarget::Detached,
            reversible: Reversible::Yes,
        }
    }

    #[test]
    fn an_older_save_never_overwrites_a_newer_one() {
        let dir = std::env::temp_dir().join(format!("invoke-undodb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = NotesDb::new(Some(dir.join("notes.sqlite")));
        db.save_undo("r", 2, vec![point(1), point(2)]).unwrap();
        // The save asked for first finishes last: it is dropped.
        db.save_undo("r", 1, vec![point(1)]).unwrap();
        db.save_undo("r", 2, vec![point(9)]).unwrap();
        assert_eq!(db.load_undo("r").unwrap().0.len(), 2);
        db.save_undo("r", 3, vec![point(7)]).unwrap();
        let (entries, skipped) = db.load_undo("r").unwrap();
        assert_eq!((entries.len(), skipped, entries[0].point.op.0), (1, 0, 7));
        // Another repo has its own count.
        db.save_undo("other", 1, vec![point(5)]).unwrap();
        assert_eq!(db.load_undo("other").unwrap().0.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
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
