mod fixtures;
mod metadata;

use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};

const UI_PACKAGES: [&str; 4] = ["ui", "app", "spike-diff", "spike-graph"];

fn main() -> ExitCode {
    let args = env::args_os().skip(1).collect::<Vec<_>>();
    let result = match args.as_slice() {
        [command] if command == "check" => check(),
        [command] if command == "fixtures" => write_fixtures(),
        _ => {
            eprintln!("Usage: cargo xtask check | cargo xtask fixtures");
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("xtask: {error}");
            ExitCode::FAILURE
        }
    }
}

fn check() -> Result<(), String> {
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

    let headless_linux = cfg!(target_os = "linux")
        && ["DISPLAY", "WAYLAND_DISPLAY"]
            .iter()
            .all(|name| env::var_os(name).is_none_or(|value| value.is_empty()));
    let sccache = Command::new("sccache")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    let cargo = || {
        let mut command =
            Command::new(env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo")));
        command.current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join(".."));
        if headless_linux && env::var_os("CARGO_BUILD_JOBS").is_none() {
            command.env("CARGO_BUILD_JOBS", "2");
        }
        if sccache {
            command.env("RUSTC_WRAPPER", "sccache");
        } else if env::var_os("RUSTC_WRAPPER").is_some_and(|wrapper| wrapper == "sccache") {
            command.env_remove("RUSTC_WRAPPER");
        }
        command
    };

    let output = cargo()
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| format!("cargo metadata failed: {error}"))?;
    if !output.status.success() {
        return Err(format!("cargo metadata failed: {}", output.status));
    }
    let json = std::str::from_utf8(&output.stdout)
        .map_err(|error| format!("workspace package discovery failed: {error}"))?;
    let packages = metadata::package_names(json)
        .ok_or("workspace package discovery failed: invalid Cargo metadata")?;

    let excluded = |package: &str| headless_linux && UI_PACKAGES.contains(&package);
    let mut fmt = cargo();
    fmt.arg("fmt");
    let mut workspace = vec!["--workspace"];
    for package in &packages {
        if excluded(package) {
            workspace.extend(["--exclude", package]);
        } else {
            fmt.args(["-p", package]);
        }
    }
    run("cargo fmt", fmt.arg("--check"))?;
    run(
        "cargo clippy",
        cargo()
            .arg("clippy")
            .args(&workspace)
            .args(["--all-targets", "--", "-D", "warnings"]),
    )?;
    run("cargo test", cargo().arg("test").args(&workspace))?;

    if cfg!(windows) {
        let mut ui_check = cargo();
        ui_check.arg("check");
        let mut found = false;
        for package in ["ui", "app"] {
            if packages.contains(&package) {
                ui_check.args(["-p", package]);
                found = true;
            }
        }
        if found {
            run("cargo check (Windows UI)", &mut ui_check)?;
        } else {
            eprintln!("xtask: Windows UI check skipped (ui and app are not workspace members)");
        }
    }
    Ok(())
}

/// Writes the Phase 0 diff fixtures into the gitignored `fixtures/diff/`.
fn write_fixtures() -> Result<(), String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("fixtures")
        .join("diff");
    fs::create_dir_all(&dir)
        .map_err(|error| format!("create {} failed: {error}", dir.display()))?;
    let generated = fixtures::generate();
    for (name, lines) in [("old.rs", &generated.old), ("new.rs", &generated.new)] {
        let path = dir.join(name);
        // LF endings on every platform, so both files match byte for byte.
        let mut text = lines.join("\n");
        text.push('\n');
        fs::write(&path, text)
            .map_err(|error| format!("write {} failed: {error}", path.display()))?;
        eprintln!("xtask: wrote {} ({} lines)", path.display(), lines.len());
    }
    eprintln!(
        "xtask: {} lines modified, {} deleted, {} inserted ({} changed)",
        generated.modified,
        generated.deleted,
        generated.inserted,
        generated.modified + generated.deleted + generated.inserted
    );
    Ok(())
}

fn run(step: &str, command: &mut Command) -> Result<(), String> {
    eprintln!("xtask: {step}");
    let status = command
        .status()
        .map_err(|error| format!("{step} failed: {error}"))?;
    if !status.success() {
        return Err(format!("{step} failed: {status}"));
    }
    Ok(())
}
