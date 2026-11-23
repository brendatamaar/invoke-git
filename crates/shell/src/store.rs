//! The app's SQLite file (task 22): the notes of the pre-push review. One
//! database in the app data folder, never inside `.git`; every row names the
//! repo it belongs to, so a request for another repo's note finds nothing.
//!
//! The schema carries its version in `PRAGMA user_version`. A file written by
//! a newer build is refused rather than guessed at. No clock here: the
//! caller gives the time.

use std::path::Path;

use invoke_core::notes::{Anchor, Note, NoteId, clean_body};
use rusqlite::{Connection, OptionalExtension, params};

/// The version of the schema this build writes.
const VERSION: i64 = 1;

pub struct Store {
    conn: Connection,
}

fn failed(what: &str, error: impl std::fmt::Display) -> String {
    format!("{what}: {error}")
}

impl Store {
    /// Opens (or creates) the database at `path`, creating its folder too.
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(folder) = path.parent() {
            std::fs::create_dir_all(folder)
                .map_err(|e| failed(&format!("create {}", folder.display()), e))?;
        }
        let conn =
            Connection::open(path).map_err(|e| failed(&format!("open {}", path.display()), e))?;
        Self::ready(conn)
    }

    /// A database that lives only as long as the value (tests).
    pub fn open_in_memory() -> Result<Self, String> {
        Self::ready(Connection::open_in_memory().map_err(|e| failed("open the store", e))?)
    }

    fn ready(conn: Connection) -> Result<Self, String> {
        // A second connection (another build, a backup tool) waits a moment
        // instead of failing at once.
        conn.busy_timeout(std::time::Duration::from_secs(2))
            .map_err(|e| failed("set up the store", e))?;
        let found: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|e| failed("read the store version", e))?;
        match found {
            0 => conn
                .execute_batch(&format!(
                    "BEGIN;
                     CREATE TABLE notes (
                         id        INTEGER PRIMARY KEY AUTOINCREMENT,
                         repo      TEXT    NOT NULL,
                         branch    TEXT    NOT NULL,
                         commit_id TEXT    NOT NULL,
                         path      TEXT    NOT NULL,
                         line      INTEGER NOT NULL,
                         line_text TEXT    NOT NULL,
                         body      TEXT    NOT NULL,
                         todo      INTEGER NOT NULL,
                         resolved  INTEGER NOT NULL,
                         created   INTEGER NOT NULL,
                         updated   INTEGER NOT NULL
                     );
                     CREATE INDEX notes_by_repo ON notes (repo);
                     PRAGMA user_version = {VERSION};
                     COMMIT;"
                ))
                .map_err(|e| failed("create the store", e))?,
            VERSION => {}
            newer => {
                return Err(format!(
                    "the store was written by a newer Invoke Git (version {newer}); this one understands {VERSION}"
                ));
            }
        }
        Ok(Self { conn })
    }

    /// Adds a note. The text is cleaned the same way for every caller
    /// ([`clean_body`]); an empty or over-long note is an error.
    pub fn add_note(
        &self,
        repo: &str,
        branch: &str,
        anchor: &Anchor,
        body: &str,
        todo: bool,
        now: i64,
    ) -> Result<Note, String> {
        let body = clean_body(body).map_err(|p| format!("{p:?}"))?;
        self.conn
            .execute(
                "INSERT INTO notes (repo, commit_id, path, line, line_text, body, todo, resolved, created, updated, branch)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?8, ?9)",
                params![repo, anchor.commit, anchor.path, anchor.line, anchor.text, body, todo, now, branch],
            )
            .map_err(|e| failed("save the note", e))?;
        Ok(Note {
            id: NoteId(self.conn.last_insert_rowid()),
            anchor: anchor.clone(),
            branch: branch.to_owned(),
            body,
            todo,
            resolved: false,
            created: now,
            updated: now,
        })
    }

    /// Every note of `repo`, by file, line and age.
    pub fn notes(&self, repo: &str) -> Result<Vec<Note>, String> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT id, commit_id, path, line, line_text, body, todo, resolved, created, updated, branch
                 FROM notes WHERE repo = ?1 ORDER BY path, line, id",
            )
            .map_err(|e| failed("read the notes", e))?;
        let rows = statement
            .query_map(params![repo], |row| {
                Ok(Note {
                    id: NoteId(row.get(0)?),
                    anchor: Anchor {
                        commit: row.get(1)?,
                        path: row.get(2)?,
                        line: row.get(3)?,
                        text: row.get(4)?,
                    },
                    branch: row.get(10)?,
                    body: row.get(5)?,
                    todo: row.get(6)?,
                    resolved: row.get(7)?,
                    created: row.get(8)?,
                    updated: row.get(9)?,
                })
            })
            .map_err(|e| failed("read the notes", e))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| failed("read the notes", e))
    }

    /// Marks a to-do resolved or open again. `false`: no such note in this
    /// repo.
    pub fn set_resolved(
        &self,
        repo: &str,
        id: NoteId,
        resolved: bool,
        now: i64,
    ) -> Result<bool, String> {
        self.changed(
            "UPDATE notes SET resolved = ?3, updated = ?4 WHERE repo = ?1 AND id = ?2",
            params![repo, id.0, resolved, now],
        )
    }

    /// Changes a note's text and whether it is a to-do. Making it a to-do
    /// again after it was resolved opens it.
    pub fn edit_note(
        &self,
        repo: &str,
        id: NoteId,
        body: &str,
        todo: bool,
        now: i64,
    ) -> Result<bool, String> {
        let body = clean_body(body).map_err(|p| format!("{p:?}"))?;
        self.changed(
            "UPDATE notes SET body = ?3, todo = ?4, resolved = CASE WHEN ?4 = 0 THEN 0 ELSE resolved END, updated = ?5
             WHERE repo = ?1 AND id = ?2",
            params![repo, id.0, body, todo, now],
        )
    }

    /// Moves a note to another commit and line (its file and text stay): what
    /// happens when the commit it was written on was rewritten and its line
    /// was found again. `updated` is not touched; the note itself did not
    /// change.
    pub fn reanchor(
        &self,
        repo: &str,
        id: NoteId,
        commit: &str,
        line: u32,
    ) -> Result<bool, String> {
        self.changed(
            "UPDATE notes SET commit_id = ?3, line = ?4 WHERE repo = ?1 AND id = ?2",
            params![repo, id.0, commit, line],
        )
    }

    pub fn delete_note(&self, repo: &str, id: NoteId) -> Result<bool, String> {
        self.changed(
            "DELETE FROM notes WHERE repo = ?1 AND id = ?2",
            params![repo, id.0],
        )
    }

    fn changed(&self, sql: &str, params: impl rusqlite::Params) -> Result<bool, String> {
        self.conn
            .execute(sql, params)
            .map(|rows| rows > 0)
            .map_err(|e| failed("change the note", e))
    }

    /// The schema version found on disk (for tests and diagnostics).
    pub fn version(&self) -> Result<i64, String> {
        self.conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .optional()
            .map_err(|e| failed("read the store version", e))
            .map(Option::unwrap_or_default)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn anchor(path: &str, line: u32, text: &str) -> Anchor {
        Anchor {
            commit: "a".repeat(40),
            path: path.into(),
            line,
            text: text.into(),
        }
    }

    #[test]
    fn a_note_is_kept_and_listed_by_file_and_line() {
        let store = Store::open_in_memory().unwrap();
        let b = store
            .add_note("r", "main", &anchor("b.rs", 3, "x"), " second \n", true, 10)
            .unwrap();
        let a2 = store
            .add_note("r", "main", &anchor("a.rs", 9, "y"), "later", false, 11)
            .unwrap();
        let a1 = store
            .add_note("r", "main", &anchor("a.rs", 2, "z"), "earlier", false, 12)
            .unwrap();
        assert_eq!(b.body, "second");
        assert!(b.todo && !b.resolved);
        let listed = store.notes("r").unwrap();
        assert_eq!(
            listed.iter().map(|n| n.id).collect::<Vec<_>>(),
            vec![a1.id, a2.id, b.id]
        );
        assert_eq!(listed[2], b);
        assert_eq!(store.version().unwrap(), VERSION);
    }

    #[test]
    fn a_note_belongs_to_its_repo_only() {
        let store = Store::open_in_memory().unwrap();
        let note = store
            .add_note("r1", "main", &anchor("a", 1, "t"), "mine", true, 1)
            .unwrap();
        assert!(store.notes("r2").unwrap().is_empty());
        assert!(!store.set_resolved("r2", note.id, true, 2).unwrap());
        assert!(!store.edit_note("r2", note.id, "x", false, 2).unwrap());
        assert!(!store.delete_note("r2", note.id).unwrap());
        assert_eq!(store.notes("r1").unwrap(), vec![note]);
    }

    #[test]
    fn resolving_editing_and_deleting_work_and_say_when_nothing_matched() {
        let store = Store::open_in_memory().unwrap();
        let note = store
            .add_note("r", "main", &anchor("a", 1, "t"), "check", true, 1)
            .unwrap();
        assert!(store.set_resolved("r", note.id, true, 5).unwrap());
        let now = &store.notes("r").unwrap()[0];
        assert!(now.resolved && now.updated == 5 && now.created == 1);
        assert!(store.set_resolved("r", note.id, false, 6).unwrap());
        // Turning the to-do off resolves nothing and clears "resolved".
        assert!(store.set_resolved("r", note.id, true, 7).unwrap());
        assert!(
            store
                .edit_note("r", note.id, "  new text ", false, 8)
                .unwrap()
        );
        let now = &store.notes("r").unwrap()[0];
        assert_eq!(
            (now.body.as_str(), now.todo, now.resolved),
            ("new text", false, false)
        );
        assert!(store.delete_note("r", note.id).unwrap());
        assert!(!store.delete_note("r", note.id).unwrap());
        assert!(!store.set_resolved("r", NoteId(999), true, 9).unwrap());
    }

    #[test]
    fn a_note_can_be_moved_to_another_commit_and_line() {
        let store = Store::open_in_memory().unwrap();
        let note = store
            .add_note("r", "topic", &anchor("a", 3, "t"), "check", true, 1)
            .unwrap();
        assert!(store.reanchor("r", note.id, &"b".repeat(40), 8).unwrap());
        let moved = &store.notes("r").unwrap()[0];
        assert_eq!(moved.anchor.commit, "b".repeat(40));
        assert_eq!((moved.anchor.line, moved.anchor.text.as_str()), (8, "t"));
        assert_eq!(
            (moved.branch.as_str(), moved.body.as_str(), moved.updated),
            ("topic", "check", 1)
        );
        assert!(!store.reanchor("other", note.id, "c", 1).unwrap());
    }

    #[test]
    fn an_empty_or_huge_note_is_refused_and_nothing_is_saved() {
        let store = Store::open_in_memory().unwrap();
        assert!(
            store
                .add_note("r", "main", &anchor("a", 1, "t"), "  \n ", false, 1)
                .is_err()
        );
        assert!(
            store
                .add_note(
                    "r",
                    "main",
                    &anchor("a", 1, "t"),
                    &"x".repeat(5000),
                    false,
                    1
                )
                .is_err()
        );
        let note = store
            .add_note("r", "main", &anchor("a", 1, "t"), "ok", false, 1)
            .unwrap();
        assert!(store.edit_note("r", note.id, "", false, 2).is_err());
        assert_eq!(store.notes("r").unwrap()[0].body, "ok");
    }

    #[test]
    fn notes_survive_closing_and_opening_the_file() {
        let dir = std::env::temp_dir().join(format!("invoke-store-{}", std::process::id()));
        let file = dir.join("deep").join("notes.sqlite");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let store = Store::open(&file).unwrap();
            store
                .add_note("r", "main", &anchor("a", 4, "four"), "keep me", true, 3)
                .unwrap();
        }
        let store = Store::open(&file).unwrap();
        let notes = store.notes("r").unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].anchor, anchor("a", 4, "four"));
        assert_eq!(notes[0].created, 3);
        // A file from a newer build is refused, not read as if it were ours.
        store
            .conn
            .execute_batch("PRAGMA user_version = 99")
            .unwrap();
        drop(store);
        let error = Store::open(&file).err().expect("refused");
        assert!(error.contains("newer"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    proptest! {
        /// Whatever the text, it comes back the way it went in (after the
        /// cleaning every note gets), and quotes or SQL in it do nothing.
        #[test]
        fn any_text_round_trips(
            body in "[^\\x00]{1,60}",
            line_text in "[^\\x00\\n\\r]{0,40}",
            path in "[a-z/'\" ;-]{1,20}",
            line in 1u32..100_000,
            todo: bool,
        ) {
            prop_assume!(!body.trim().is_empty());
            let store = Store::open_in_memory().unwrap();
            let anchor = Anchor { commit: "f".repeat(40), path, line, text: line_text };
            let note = store.add_note("repo'; DROP TABLE notes; --", "main", &anchor, &body, todo, 7).unwrap();
            let listed = store.notes("repo'; DROP TABLE notes; --").unwrap();
            prop_assert_eq!(listed.len(), 1);
            prop_assert_eq!(&listed[0], &note);
            prop_assert_eq!(&note.anchor, &anchor);
            let expected = body.replace("\r\n", "\n").replace('\r', "\n");
            prop_assert_eq!(note.body.as_str(), expected.trim());
        }
    }
}
