//! Lazy commit metadata for visible rows: subject, author and date.
//!
//! A background thread reads ahead of the visible rows; the UI thread reads
//! only visible rows the prefetch has not reached yet (after a jump).

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread;

use gix::ObjectId;
use gpui::SharedString;

pub struct Meta {
    pub subject: SharedString,
    pub author: SharedString,
    pub date: SharedString,
}

/// A batch of (row, commit) to read, newest request first in line.
pub type Request = Vec<(usize, ObjectId)>;

pub struct Prefetch {
    pub requests: Sender<Request>,
    pub results: Receiver<(usize, Meta)>,
}

pub fn spawn_prefetch(repo: PathBuf) -> Prefetch {
    let (requests, inbox) = channel::<Request>();
    let (outbox, results) = channel();
    thread::Builder::new()
        .name("spike-graph metadata".into())
        .spawn(move || {
            let Ok(repo) = gix::open(&repo) else {
                return;
            };
            let mut queue: Vec<Request> = Vec::new();
            loop {
                if queue.is_empty() {
                    match inbox.recv() {
                        Ok(batch) => queue.push(batch),
                        Err(_) => return,
                    }
                }
                loop {
                    match inbox.try_recv() {
                        Ok(batch) => queue.push(batch),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return,
                    }
                }
                // The newest batch is nearest the current scroll position.
                let Some(batch) = queue.pop() else {
                    continue;
                };
                for (row, id) in batch {
                    if let Ok(meta) = read(&repo, id)
                        && outbox.send((row, meta)).is_err()
                    {
                        return;
                    }
                }
            }
        })
        .expect("spawn the metadata thread");
    Prefetch { requests, results }
}

pub fn read(repo: &gix::Repository, id: ObjectId) -> Result<Meta, String> {
    let commit = repo.find_commit(id).map_err(|e| format!("{id}: {e}"))?;
    let message = commit.message_raw_sloppy();
    let subject = message
        .split(|&b| b == b'\n')
        .next()
        .map(|line| String::from_utf8_lossy(line).trim_end().to_string())
        .unwrap_or_default();
    let author = commit
        .author()
        .map(|a| String::from_utf8_lossy(a.name).into_owned())
        .unwrap_or_default();
    let date = commit
        .time()
        .map(|t| format_date(t.seconds))
        .unwrap_or_default();
    Ok(Meta {
        subject: subject.into(),
        author: author.into(),
        date: date.into(),
    })
}

/// `YYYY-MM-DD` in UTC from Unix seconds (Howard Hinnant's civil-from-days).
pub fn format_date(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::format_date;

    #[test]
    fn formats_known_dates() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(951_782_400), "2000-02-29");
        assert_eq!(format_date(1_795_123_524), "2026-11-19");
        assert_eq!(format_date(-86_400), "1969-12-31");
    }
}
