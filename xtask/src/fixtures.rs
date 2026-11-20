// Phase 0 diff-spike fixtures: a 50k-line Rust-like file and a copy with
// about 2k changed lines spread through it. Generation is deterministic (a
// fixed seed), so every machine gets byte-identical files.

pub const LINES: usize = 50_000;
const HUNKS: usize = 200;
const HUNK_SPAN: usize = 10;
const SEED: u64 = 0x1_0bad_5eed;

pub struct Fixtures {
    pub old: Vec<String>,
    pub new: Vec<String>,
    pub modified: usize,
    pub deleted: usize,
    pub inserted: usize,
}

/// splitmix64: tiny, deterministic and good enough for fixture noise.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

const NOUNS: &[&str] = &[
    "commit", "branch", "index", "hunk", "lane", "patch", "blob", "tree", "reflog", "remote",
    "stash", "worktree", "object", "delta", "cursor", "buffer", "layout", "row",
];
const TYPES: &[&str] = &[
    "u32",
    "u64",
    "usize",
    "i64",
    "bool",
    "String",
    "Vec<u8>",
    "Option<usize>",
    "&str",
    "f32",
];
const WORDS: &[&str] = &[
    "fast", "stale", "pending", "loaded", "dirty", "merged", "detached", "visible", "cached",
];

fn camel(word: &str) -> String {
    let mut chars = word.chars();
    chars
        .next()
        .map(|first| first.to_ascii_uppercase().to_string() + chars.as_str())
        .unwrap_or_default()
}

fn statement(rng: &mut Rng, indent: &str, n: usize) -> String {
    let noun = rng.pick(NOUNS);
    match rng.below(8) {
        0 => format!("{indent}let {noun}_{n} = {};", rng.below(100_000)),
        1 => format!("{indent}let label = \"{} {noun} {n}\";", rng.pick(WORDS)),
        2 => format!(
            "{indent}// {} {noun} handling, step {n}",
            camel(rng.pick(WORDS))
        ),
        3 => format!(
            "{indent}total += self.{noun}.len() as u64 * {};",
            rng.below(64) + 1
        ),
        4 => format!(
            "{indent}if self.{noun}_count > {} {{ return None; }}",
            rng.below(4096)
        ),
        5 => format!("{indent}out.push({noun}_value(&self.{noun}, {n}));"),
        6 => format!(
            "{indent}let ratio: f32 = {}.{} / 3.0;",
            rng.below(100),
            rng.below(100)
        ),
        _ => format!("{indent}debug_assert!(self.{noun}.is_some(), \"{noun} must exist\");"),
    }
}

fn item(rng: &mut Rng, n: usize, out: &mut Vec<String>) {
    let noun = rng.pick(NOUNS);
    let name = format!("{}{n}", camel(noun));
    match rng.below(3) {
        0 => {
            out.push(format!(
                "/// The {} {noun} record number {n}.",
                rng.pick(WORDS)
            ));
            out.push("#[derive(Debug, Clone, PartialEq)]".to_string());
            out.push(format!("pub struct {name} {{"));
            for field in 0..rng.below(6) + 2 {
                out.push(format!(
                    "    pub {}_{field}: {},",
                    rng.pick(NOUNS),
                    rng.pick(TYPES)
                ));
            }
            out.push("}".to_string());
        }
        1 => {
            out.push(format!("impl {name} {{"));
            for method in 0..rng.below(3) + 1 {
                out.push(format!(
                    "    pub fn {}_{method}(&self, limit: usize) -> Option<{}> {{",
                    rng.pick(WORDS),
                    rng.pick(TYPES)
                ));
                out.push("        let mut total = 0u64;".to_string());
                for line in 0..rng.below(10) + 3 {
                    out.push(statement(rng, "        ", n * 10 + line));
                }
                out.push("        None".to_string());
                out.push("    }".to_string());
            }
            out.push("}".to_string());
        }
        _ => {
            out.push(format!(
                "fn {noun}_value(input: &[{}], seed: usize) -> usize {{",
                rng.pick(TYPES)
            ));
            out.push("    match seed % 3 {".to_string());
            out.push(format!("        0 => input.len() + {},", rng.below(1000)));
            out.push(format!(
                "        1 => seed.wrapping_mul({}),",
                rng.below(97) + 3
            ));
            out.push("        _ => 0,".to_string());
            out.push("    }".to_string());
            out.push("}".to_string());
        }
    }
    out.push(String::new());
}

fn generate_old(rng: &mut Rng) -> Vec<String> {
    let mut lines = vec![
        "//! Generated Phase 0 fixture. Rust-like, not meant to compile.".to_string(),
        String::new(),
        "use std::collections::HashMap;".to_string(),
        String::new(),
    ];
    let mut n = 0;
    while lines.len() < LINES {
        item(rng, n, &mut lines);
        n += 1;
    }
    lines.truncate(LINES);
    lines
}

/// Rewrites a line so it is guaranteed to differ, keeping it Rust-like.
fn modify(rng: &mut Rng, line: &str) -> String {
    let changed = match rng.below(3) {
        0 => line.replacen(|c: char| c.is_ascii_digit(), &rng.below(10).to_string(), 1),
        1 => {
            let from = rng.pick(NOUNS);
            line.replace(from, rng.pick(NOUNS))
        }
        _ => line.replace("pub ", ""),
    };
    if changed == line {
        format!("{line} // edited")
    } else {
        changed
    }
}

pub fn generate() -> Fixtures {
    let mut rng = Rng(SEED);
    let old = generate_old(&mut rng);
    let mut new = Vec::with_capacity(LINES + HUNK_SPAN);
    let (mut modified, mut deleted, mut inserted) = (0, 0, 0);

    // One hunk per equal stretch, at a random offset inside it, so changes
    // are spread from the first page to the last.
    let stretch = LINES / HUNKS;
    let starts: Vec<usize> = (0..HUNKS)
        .map(|h| h * stretch + rng.below(stretch - HUNK_SPAN))
        .collect();
    let mut next_hunk = starts.iter().peekable();
    let mut index = 0;
    while index < old.len() {
        if next_hunk.next_if(|&&start| start == index).is_some() {
            for line in &old[index..index + HUNK_SPAN] {
                match rng.below(20) {
                    0..=12 => {
                        new.push(modify(&mut rng, line));
                        modified += 1;
                    }
                    13..=15 => deleted += 1,
                    _ => {
                        new.push(line.clone());
                        new.push(statement(&mut rng, "        ", inserted));
                        inserted += 1;
                    }
                }
            }
            index += HUNK_SPAN;
        } else {
            new.push(old[index].clone());
            index += 1;
        }
    }
    Fixtures {
        old,
        new,
        modified,
        deleted,
        inserted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_file_has_exactly_fifty_thousand_lines() {
        assert_eq!(generate().old.len(), LINES);
    }

    #[test]
    fn about_two_thousand_lines_change_and_counts_add_up() {
        let f = generate();
        let changed = f.modified + f.deleted + f.inserted;
        assert!((1_800..=2_200).contains(&changed), "changed {changed}");
        assert_eq!(f.new.len(), LINES - f.deleted + f.inserted);
    }

    #[test]
    fn generation_is_deterministic() {
        let (a, b) = (generate(), generate());
        assert_eq!(a.old, b.old);
        assert_eq!(a.new, b.new);
    }

    #[test]
    fn modify_always_changes_the_line() {
        let mut rng = Rng(SEED);
        for line in generate().old.iter().take(5_000) {
            assert_ne!(&modify(&mut rng, line), line);
        }
    }
}
