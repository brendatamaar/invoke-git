//! `cargo xtask bench-check` (backlog task 25): run the criterion benchmarks
//! and fail when a measured time is past its ceiling in
//! `docs/perf-budgets.toml`.
//!
//! Criterion does not fail on a regression by itself, so this reads the
//! median it writes to `<home>/<group>/<name>/new/estimates.json` and
//! compares it with an absolute ceiling. Ceilings come from the PRD budgets,
//! not from a recorded baseline, so a slow CI runner cannot move them. Every
//! benchmark needs a budget and every budget needs a benchmark.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

const BUDGETS_FILE: &str = "docs/perf-budgets.toml";
const SECTION: &str = "[budgets_ms]";

/// Ceilings in milliseconds, by benchmark id (`group/name`).
pub type Budgets = BTreeMap<String, f64>;

/// Hand parser for the tiny TOML subset the budgets file uses: comments,
/// one `[budgets_ms]` section, and `"group/name" = <milliseconds>` lines.
pub fn parse_budgets(text: &str) -> Result<Budgets, String> {
    let mut budgets = Budgets::new();
    let mut in_section = false;
    for (index, raw) in text.lines().enumerate() {
        let at = |message: &str| format!("{BUDGETS_FILE}:{}: {message}", index + 1);
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            if line != SECTION {
                return Err(at(&format!("unknown section {line}, expected {SECTION}")));
            }
            in_section = true;
            continue;
        }
        if !in_section {
            return Err(at(&format!("a budget before the {SECTION} section")));
        }
        let (name, value) = line
            .split_once('=')
            .ok_or_else(|| at("expected \"group/name\" = milliseconds"))?;
        let name = name
            .trim()
            .strip_prefix('"')
            .and_then(|name| name.strip_suffix('"'))
            .filter(|name| !name.is_empty() && !name.contains('"'))
            .ok_or_else(|| at("the benchmark name must be a quoted string"))?;
        let ms: f64 = value
            .trim()
            .parse()
            .map_err(|_| at("the budget must be a number of milliseconds"))?;
        if !ms.is_finite() || ms <= 0.0 {
            return Err(at("the budget must be a positive number"));
        }
        if budgets.insert(name.to_string(), ms).is_some() {
            return Err(at(&format!("{name} is listed twice")));
        }
    }
    if budgets.is_empty() {
        return Err(format!("{BUDGETS_FILE}: no budgets found"));
    }
    Ok(budgets)
}

/// The median time in nanoseconds from criterion's `estimates.json`, which
/// looks like `{"mean":{...},"median":{"confidence_interval":{...},
/// "point_estimate":123.4,"standard_error":1.2},...}`.
pub fn median_ns(json: &str) -> Option<f64> {
    let after = &json[json.find("\"median\"")?..];
    let after = &after[after.find("\"point_estimate\"")? + "\"point_estimate\"".len()..];
    let after = after.trim_start().strip_prefix(':')?.trim_start();
    let end = after
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E')))
        .unwrap_or(after.len());
    after[..end].parse().ok().filter(|ns: &f64| ns.is_finite())
}

/// The outcome of comparing measurements with budgets.
#[derive(Debug, PartialEq)]
pub struct Report {
    pub lines: Vec<String>,
    pub failures: usize,
}

fn show(ms: f64) -> String {
    if ms >= 100.0 {
        format!("{ms:.0} ms")
    } else if ms >= 1.0 {
        format!("{ms:.1} ms")
    } else {
        format!("{:.0} us", ms * 1000.0)
    }
}

/// Compares the measured medians (milliseconds, by id) with the budgets. A
/// budget with no measurement and a measurement with no budget both fail, so
/// a benchmark cannot be added, renamed or broken without the file noticing.
pub fn evaluate(budgets: &Budgets, measured: &BTreeMap<String, f64>) -> Report {
    let mut lines = Vec::new();
    let mut failures = 0;
    for (name, &budget) in budgets {
        match measured.get(name) {
            None => {
                failures += 1;
                lines.push(format!(
                    "FAIL {name}: no measurement (benchmark missing or renamed; \
                     fix it or remove its budget)"
                ));
            }
            Some(&ms) if ms > budget => {
                failures += 1;
                lines.push(format!(
                    "FAIL {name}: {} is over the {} budget (x{:.2} of the ceiling)",
                    show(ms),
                    show(budget),
                    ms / budget
                ));
            }
            Some(&ms) => lines.push(format!(
                "ok   {name}: {} within the {} budget (headroom x{:.1})",
                show(ms),
                show(budget),
                budget / ms
            )),
        }
    }
    for name in measured.keys().filter(|name| !budgets.contains_key(*name)) {
        failures += 1;
        lines.push(format!(
            "FAIL {name}: no budget (add it to {BUDGETS_FILE} with a ceiling from the PRD)"
        ));
    }
    Report { lines, failures }
}

/// Reads every `<home>/<group>/<name>/new/estimates.json` into milliseconds.
fn read_measurements(home: &Path) -> Result<BTreeMap<String, f64>, String> {
    let mut measured = BTreeMap::new();
    let read_dir = |path: &Path| -> Result<Vec<PathBuf>, String> {
        let entries = fs::read_dir(path)
            .map_err(|error| format!("read {} failed: {error}", path.display()))?;
        let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        paths.sort();
        Ok(paths)
    };
    for group in read_dir(home)?.into_iter().filter(|path| path.is_dir()) {
        let group_name = file_name(&group);
        for bench in read_dir(&group)?.into_iter().filter(|path| path.is_dir()) {
            let estimates = bench.join("new").join("estimates.json");
            // Criterion's own `report` directory has no estimates.
            let Ok(json) = fs::read_to_string(&estimates) else {
                continue;
            };
            let ns =
                median_ns(&json).ok_or_else(|| format!("no median in {}", estimates.display()))?;
            measured.insert(format!("{group_name}/{}", file_name(&bench)), ns / 1e6);
        }
    }
    Ok(measured)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn bench_check() -> Result<(), String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let text = fs::read_to_string(root.join(BUDGETS_FILE))
        .map_err(|error| format!("read {BUDGETS_FILE} failed: {error}"))?;
    let budgets = parse_budgets(&text)?;

    // Timings are only meaningful when no other build competes for the
    // cores, so share the lock `cargo xtask check` takes.
    let lock_path = env::temp_dir().join("invoke-git-check.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| format!("open check lock {} failed: {error}", lock_path.display()))?;
    eprintln!("xtask: acquiring check lock {}", lock_path.display());
    lock.lock()
        .map_err(|error| format!("acquire check lock failed: {error}"))?;

    // A fresh home, so a benchmark that stopped running cannot pass on a
    // stale result.
    let home = root.join("target").join("bench-check");
    if home.exists() {
        fs::remove_dir_all(&home)
            .map_err(|error| format!("clear {} failed: {error}", home.display()))?;
    }
    let mut command = Command::new(env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo")));
    command
        .current_dir(&root)
        .env("CRITERION_HOME", &home)
        .args(["bench", "-p", "diff", "-p", "graph", "--benches"]);
    if cfg!(target_os = "linux") && env::var_os("CARGO_BUILD_JOBS").is_none() {
        command.env("CARGO_BUILD_JOBS", "2");
    }
    eprintln!("xtask: cargo bench (diff, graph)");
    let status = command
        .status()
        .map_err(|error| format!("cargo bench failed: {error}"))?;
    if !status.success() {
        return Err(format!("cargo bench failed: {status}"));
    }

    let measured = read_measurements(&home)?;
    let report = evaluate(&budgets, &measured);
    eprintln!();
    eprintln!("xtask: benchmark medians against {BUDGETS_FILE}");
    for line in &report.lines {
        eprintln!("  {line}");
    }
    if report.failures > 0 {
        return Err(format!(
            "{} benchmark(s) past budget or without a budget",
            report.failures
        ));
    }
    eprintln!("xtask: all {} benchmarks within budget", budgets.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# comment
[budgets_ms]
"diff/a" = 50      # trailing comment
"graph/b" = 0.5
"#;

    fn measured(items: &[(&str, f64)]) -> BTreeMap<String, f64> {
        items.iter().map(|(n, v)| (n.to_string(), *v)).collect()
    }

    #[test]
    fn parses_budgets() {
        let budgets = parse_budgets(SAMPLE).unwrap();
        assert_eq!(budgets.len(), 2);
        assert_eq!(budgets["diff/a"], 50.0);
        assert_eq!(budgets["graph/b"], 0.5);
    }

    #[test]
    fn rejects_bad_budget_files() {
        for bad in [
            "",
            "[budgets_ms]\n",
            "\"a/b\" = 5\n",
            "[other]\n\"a/b\" = 5\n",
            "[budgets_ms]\na/b = 5\n",
            "[budgets_ms]\n\"a/b\" = fast\n",
            "[budgets_ms]\n\"a/b\" = 0\n",
            "[budgets_ms]\n\"a/b\" = -3\n",
            "[budgets_ms]\n\"a/b\" = 5\n\"a/b\" = 6\n",
            "[budgets_ms]\n\"a/b\"\n",
        ] {
            assert!(parse_budgets(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn reads_the_median_not_the_mean() {
        let json = r#"{"mean":{"confidence_interval":{"confidence_level":0.95,
            "lower_bound":1.0,"upper_bound":9.0},"point_estimate":111.0,
            "standard_error":0.5},"median":{"confidence_interval":{
            "confidence_level":0.95,"lower_bound":1.0,"upper_bound":9.0},
            "point_estimate":2.5e6,"standard_error":0.1},"slope":null}"#;
        assert_eq!(median_ns(json), Some(2.5e6));
        assert_eq!(median_ns("{}"), None);
        assert_eq!(median_ns(r#"{"median":{"point_estimate":"x"}}"#), None);
    }

    #[test]
    fn passes_within_budget_and_fails_past_it() {
        let budgets = parse_budgets(SAMPLE).unwrap();
        let ok = evaluate(&budgets, &measured(&[("diff/a", 49.9), ("graph/b", 0.1)]));
        assert_eq!(ok.failures, 0);
        let slow = evaluate(&budgets, &measured(&[("diff/a", 50.1), ("graph/b", 0.1)]));
        assert_eq!(slow.failures, 1);
        assert!(slow.lines[0].starts_with("FAIL diff/a"), "{:?}", slow.lines);
    }

    #[test]
    fn a_missing_or_unbudgeted_benchmark_fails() {
        let budgets = parse_budgets(SAMPLE).unwrap();
        let missing = evaluate(&budgets, &measured(&[("diff/a", 1.0)]));
        assert_eq!(missing.failures, 1);
        assert!(
            missing
                .lines
                .iter()
                .any(|l| l.contains("graph/b: no measurement"))
        );
        let extra = evaluate(
            &budgets,
            &measured(&[("diff/a", 1.0), ("graph/b", 0.1), ("diff/new", 1.0)]),
        );
        assert_eq!(extra.failures, 1);
        assert!(
            extra
                .lines
                .iter()
                .any(|l| l.contains("diff/new: no budget"))
        );
    }
}
