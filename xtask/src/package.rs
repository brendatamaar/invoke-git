//! `cargo xtask package-windows`: downloads the pinned MinGit, checks its
//! SHA-256 and lays out the Windows install folder (app, askpass, rebase
//! editor, `mingit/`). Hashing, download and unzip use tools that ship with
//! Windows (PowerShell, curl.exe, tar.exe), so no extra crate is needed.
//!
//! Update procedure: see "Bundled MinGit" in `README.md`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The pinned MinGit release (git-for-windows). The checksum is the one in
/// the release notes; it was also recomputed from the downloaded file.
pub const MINGIT_VERSION: &str = "2.56.1";
pub const MINGIT_TAG: &str = "v2.56.1.windows.1";
pub const MINGIT_FILE: &str = "MinGit-2.56.1-64-bit.zip";
pub const MINGIT_SHA256: &str = "4a92850b6e917da4fd010669b614b55dccf5b0ec333a534cc4be4700dfefdafd";

/// Binaries that sit next to each other in the install folder; the app finds
/// `askpass` and the rebase editor through its own executable's folder.
const BINARIES: [&str; 3] = ["invoke-git.exe", "askpass.exe", "invoke-rebase-editor.exe"];

pub fn mingit_url() -> String {
    format!("https://github.com/git-for-windows/git/releases/download/{MINGIT_TAG}/{MINGIT_FILE}")
}

/// A SHA-256 as 64 lowercase hex digits, or `None`. Accepts surrounding
/// whitespace and either case, so tool output compares reliably.
pub fn normalize_sha256(text: &str) -> Option<String> {
    let text = text.trim();
    (text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| text.to_ascii_lowercase())
}

/// `Ok` when `actual` (tool output) equals the `expected` digest.
pub fn verify_sha256(expected: &str, actual: &str) -> Result<(), String> {
    let expected = normalize_sha256(expected).ok_or("the pinned SHA-256 is not 64 hex digits")?;
    let actual = normalize_sha256(actual)
        .ok_or_else(|| format!("could not read a SHA-256 from {:?}", actual.trim()))?;
    if expected == actual {
        Ok(())
    } else {
        Err(format!(
            "SHA-256 mismatch: expected {expected}, downloaded file is {actual}"
        ))
    }
}

pub fn package_windows() -> Result<(), String> {
    if !cfg!(windows) {
        return Err("package-windows runs on Windows only".into());
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let cache = root.join("target").join("mingit");
    let dist = root.join("dist").join("windows");
    fs::create_dir_all(&cache).map_err(|e| format!("create {} failed: {e}", cache.display()))?;

    let zip = cache.join(MINGIT_FILE);
    if !(zip.is_file() && sha256_of(&zip).is_ok_and(|h| verify_sha256(MINGIT_SHA256, &h).is_ok())) {
        eprintln!("xtask: downloading {}", mingit_url());
        run(
            "download MinGit",
            Command::new(system32("curl.exe"))
                .args(["--fail", "--silent", "--show-error", "--location", "-o"])
                .arg(&zip)
                .arg(mingit_url()),
        )?;
    }
    // Verify before anything is unpacked or copied; a bad file is deleted.
    let actual = sha256_of(&zip)?;
    if let Err(error) = verify_sha256(MINGIT_SHA256, &actual) {
        let _ = fs::remove_file(&zip);
        return Err(error);
    }
    eprintln!("xtask: MinGit {MINGIT_VERSION} SHA-256 verified");

    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    // An explicit target dir: `CARGO_TARGET_DIR` or `build.target-dir` would
    // otherwise send fresh binaries elsewhere and leave stale ones to copy.
    let target = root.join("target").join("package");
    let mut build = Command::new(cargo);
    build
        .current_dir(&root)
        .args(["build", "--release", "--target-dir"])
        .arg(&target);
    for package in ["app", "askpass", "shell"] {
        build.args(["-p", package]);
    }
    run("cargo build --release", &mut build)?;

    if dist.exists() {
        fs::remove_dir_all(&dist).map_err(|e| format!("clean {} failed: {e}", dist.display()))?;
    }
    let mingit = dist.join(shell_dir());
    fs::create_dir_all(&mingit).map_err(|e| format!("create {} failed: {e}", mingit.display()))?;
    for file in BINARIES {
        let from = target.join("release").join(file);
        fs::copy(&from, dist.join(file))
            .map_err(|e| format!("copy {} failed: {e}", from.display()))?;
    }
    run(
        "unpack MinGit",
        Command::new(system32("tar.exe"))
            .arg("-xf")
            .arg(&zip)
            .arg("-C")
            .arg(&mingit),
    )?;
    let git = mingit.join("cmd").join("git.exe");
    if !git.is_file() {
        return Err(format!("{} missing after unpacking", git.display()));
    }
    eprintln!("xtask: wrote {}", dist.display());
    Ok(())
}

/// Folder name next to the app; must match `shell::git_discovery::BUNDLED_DIR`.
fn shell_dir() -> &'static str {
    "mingit"
}

/// A tool from `%SystemRoot%System32`. `tar.exe` must be Windows' bsdtar (it
/// reads zips); Git for Windows puts a GNU tar earlier on PATH that does not.
fn system32(tool: &str) -> PathBuf {
    let root = env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    PathBuf::from(root).join("System32").join(tool)
}

fn sha256_of(path: &Path) -> Result<String, String> {
    // `-Command` joins every later argument into the script text, so the
    // path cannot follow as `$args[0]`. An environment variable carries it
    // as data: no quoting, and no path text is ever parsed as PowerShell.
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg("(Get-FileHash -Algorithm SHA256 -LiteralPath $env:INVOKE_HASH_PATH -ErrorAction Stop).Hash")
        .env("INVOKE_HASH_PATH", path)
        .output()
        .map_err(|e| format!("Get-FileHash failed to start: {e}"))?;
    if !output.status.success() {
        return Err(format!("Get-FileHash failed: {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run(step: &str, command: &mut Command) -> Result<(), String> {
    eprintln!("xtask: {step}");
    let status = command
        .status()
        .map_err(|e| format!("{step} failed: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{step} failed: {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pin_is_a_valid_digest_for_a_zip_of_the_pinned_version() {
        assert!(normalize_sha256(MINGIT_SHA256).is_some());
        assert!(MINGIT_FILE.contains(MINGIT_VERSION));
        assert!(MINGIT_TAG.starts_with(&format!("v{MINGIT_VERSION}")));
        assert!(mingit_url().ends_with(MINGIT_FILE));
    }

    #[test]
    fn matching_digests_pass_in_any_case_with_whitespace() {
        let upper = format!("  {}\r\n", MINGIT_SHA256.to_ascii_uppercase());
        assert!(verify_sha256(MINGIT_SHA256, &upper).is_ok());
    }

    #[test]
    fn a_different_digest_is_refused() {
        let other = "0".repeat(64);
        assert!(
            verify_sha256(MINGIT_SHA256, &other)
                .unwrap_err()
                .contains("mismatch")
        );
    }

    #[test]
    fn garbage_output_is_refused_not_trusted() {
        for bad in [
            "",
            "abc",
            "Get-FileHash : error",
            &"g".repeat(64),
            &"a".repeat(63),
        ] {
            assert!(verify_sha256(MINGIT_SHA256, bad).is_err(), "{bad:?}");
        }
        // An unusable pin is never treated as "matches anything".
        assert!(verify_sha256("", "").is_err());
    }
}
