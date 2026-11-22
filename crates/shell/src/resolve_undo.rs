//! The index's resolve-undo record (task 23): the paths whose conflicts were
//! resolved by staging them, kept by git in the `REUC` extension so
//! `git checkout -m` could bring the conflict back.
//!
//! This is how "Resolved" survives on disk. Once a file is staged its
//! stages 1-3 are gone from the index entries, and nothing else says it was
//! ever conflicted. gix decodes the extension but does not expose it, and the
//! git CLI is for writes, so the extension is read here from the raw file.
//!
//! Index layout (`Documentation/gitformat-index.adoc`): a 12-byte header,
//! the entries, then extensions (4-byte signature, 4-byte big-endian size,
//! data), then the checksum. The entries have no length prefix, so the start
//! of the extensions is found the other way round: a `REUC` signature counts
//! only if the chain of extensions starting there ends exactly where the
//! checksum begins.
//!
//! The record is advisory. Anything that does not parse gives no paths, and
//! the files simply show as Unresolved or vanish from the list: git itself
//! never reads this from us.

/// Paths recorded as resolved. `hash_len` is the object id width in bytes.
pub fn resolved_paths(index: &[u8], hash_len: usize) -> Vec<String> {
    let Some(body_end) = index.len().checked_sub(hash_len) else {
        return Vec::new();
    };
    let mut start = 12;
    while start + 8 <= body_end {
        let Some(offset) = find_signature(&index[start..body_end]) else {
            break;
        };
        let at = start + offset;
        if chain_reaches(index, at, body_end)
            && let Some(size) = read_size(index, at + 4)
            && let Some(data) = index.get(at + 8..at + 8 + size)
        {
            return decode(data, hash_len).unwrap_or_default();
        }
        start = at + 1;
    }
    Vec::new()
}

fn find_signature(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"REUC")
}

fn read_size(bytes: &[u8], at: usize) -> Option<usize> {
    let raw: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
    usize::try_from(u32::from_be_bytes(raw)).ok()
}

/// Whether extensions run from `start` to exactly `end`.
fn chain_reaches(bytes: &[u8], start: usize, end: usize) -> bool {
    let mut at = start;
    while at < end {
        let Some(signature) = bytes.get(at..at + 4) else {
            return false;
        };
        if !signature.iter().all(u8::is_ascii_uppercase) {
            return false;
        }
        let Some(size) = read_size(bytes, at + 4) else {
            return false;
        };
        let Some(next) = at.checked_add(8).and_then(|n| n.checked_add(size)) else {
            return false;
        };
        at = next;
    }
    at == end
}

/// `path NUL mode1 NUL mode2 NUL mode3 NUL` then one object id for each mode
/// that is not 0, repeated. Modes are octal text.
fn decode(mut data: &[u8], hash_len: usize) -> Option<Vec<String>> {
    let mut paths = Vec::new();
    while !data.is_empty() {
        let (path, rest) = split_nul(data)?;
        data = rest;
        let mut ids = 0;
        for _ in 0..3 {
            let (mode, rest) = split_nul(data)?;
            data = rest;
            let mode = std::str::from_utf8(mode).ok()?;
            if u32::from_str_radix(mode, 8).ok()? != 0 {
                ids += 1;
            }
        }
        data = data.get(ids * hash_len..)?;
        paths.push(String::from_utf8_lossy(path).into_owned());
    }
    Some(paths)
}

fn split_nul(data: &[u8]) -> Option<(&[u8], &[u8])> {
    let end = data.iter().position(|byte| *byte == 0)?;
    Some((&data[..end], &data[end + 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const HASH: usize = 20;

    fn extension(signature: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = signature.to_vec();
        out.extend((data.len() as u32).to_be_bytes());
        out.extend(data);
        out
    }

    /// REUC data for `paths`: each recorded with stages 2 and 3 only.
    fn reuc(paths: &[String]) -> Vec<u8> {
        let mut out = Vec::new();
        for path in paths {
            out.extend(path.as_bytes());
            out.extend(b"\0");
            for mode in ["0", "100644", "100644"] {
                out.extend(mode.as_bytes());
                out.push(0);
            }
            out.extend([7u8; 2 * HASH]);
        }
        out
    }

    fn index(entries: &[u8], extensions: &[Vec<u8>]) -> Vec<u8> {
        let mut out = b"DIRC\0\0\0\x02\0\0\0\x01".to_vec();
        out.extend(entries);
        for ext in extensions {
            out.extend(ext);
        }
        out.extend([9u8; HASH]);
        out
    }

    #[test]
    fn finds_the_paths_after_other_extensions() {
        let paths = vec!["a.txt".to_owned(), "dir/b c.txt".to_owned()];
        let file = index(
            b"entry bytes",
            &[
                extension(b"TREE", b"1 0\0"),
                extension(b"REUC", &reuc(&paths)),
            ],
        );
        assert_eq!(resolved_paths(&file, HASH), paths);
    }

    #[test]
    fn no_extension_no_paths() {
        assert!(resolved_paths(&index(b"entries", &[extension(b"TREE", b"")]), HASH).is_empty());
        assert!(resolved_paths(&[], HASH).is_empty());
        assert!(resolved_paths(b"DIRC", HASH).is_empty());
    }

    #[test]
    fn a_signature_inside_an_entry_is_not_the_extension() {
        let paths = vec!["real.txt".to_owned()];
        // A file literally named after the signature, with a size that
        // points nowhere useful, ahead of the real extension.
        let file = index(
            b"\0\0REUC\0\0\0\x05REUC.txt\0",
            &[extension(b"REUC", &reuc(&paths))],
        );
        assert_eq!(resolved_paths(&file, HASH), paths);
    }

    #[test]
    fn a_truncated_record_gives_nothing() {
        let mut data = reuc(&["a.txt".to_owned()]);
        data.truncate(data.len() - 3);
        let file = index(b"e", &[extension(b"REUC", &data)]);
        assert!(resolved_paths(&file, HASH).is_empty());
    }

    proptest! {
        #[test]
        fn recorded_paths_round_trip(
            paths in proptest::collection::vec("[a-zA-Z0-9_./ -]{1,20}", 0..6),
            entries in proptest::collection::vec(any::<u8>(), 0..64),
        ) {
            let file = index(&entries, &[extension(b"REUC", &reuc(&paths))]);
            prop_assert_eq!(resolved_paths(&file, HASH), paths);
        }

        #[test]
        fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..300)) {
            let _ = resolved_paths(&bytes, HASH);
        }
    }
}
