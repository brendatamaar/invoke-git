//! The words on the blocking setup screen. Pure: error in, text out.

use shell::git::{MIN_VERSION, SetupError};

pub fn headline() -> &'static str {
    "Git is required"
}

/// What went wrong and how to fix it. Plain words, no blame.
pub fn message(error: &SetupError) -> String {
    let need = format!(
        "Invoke Git needs git {}.{} or newer on your PATH.",
        MIN_VERSION.major, MIN_VERSION.minor
    );
    match error {
        SetupError::Missing(detail) => format!(
            "{need} It could not be started ({detail}). Install Git for Windows, then check again."
        ),
        SetupError::Unparseable(output) => format!(
            "{need} `git --version` printed something unexpected: {}. Install a current Git for Windows, then check again.",
            output.trim()
        ),
        SetupError::TooOld(version) => format!(
            "{need} Found {}.{}.{}. Update Git for Windows, then check again.",
            version.major, version.minor, version.patch
        ),
        SetupError::FallbackFailed { system, bundled } => format!(
            "{need} The git on your PATH is not usable ({}), and neither is the MinGit that ships with Invoke Git ({}). Install Git for Windows or reinstall Invoke Git, then check again.",
            reason(system),
            reason(bundled)
        ),
    }
}

/// One short clause for a nested error (no advice; the caller adds it).
fn reason(error: &SetupError) -> String {
    match error {
        SetupError::Missing(detail) => format!("could not be started: {detail}"),
        SetupError::Unparseable(output) => format!("unexpected version output: {}", output.trim()),
        SetupError::TooOld(v) => format!("version {}.{}.{} is too old", v.major, v.minor, v.patch),
        SetupError::FallbackFailed { system, .. } => reason(system),
    }
}

#[cfg(test)]
mod tests {
    use shell::git::GitVersion;

    use super::*;

    #[test]
    fn every_error_names_the_version_needed() {
        let errors = [
            SetupError::Missing("git: not found".into()),
            SetupError::Unparseable("hello".into()),
            SetupError::TooOld(GitVersion {
                major: 2,
                minor: 30,
                patch: 1,
            }),
            SetupError::FallbackFailed {
                system: Box::new(SetupError::Missing("git: not found".into())),
                bundled: Box::new(SetupError::Missing("mingit: not found".into())),
            },
        ];
        for error in errors {
            assert!(message(&error).contains("2.40"), "{error:?}");
        }
    }

    #[test]
    fn too_old_says_which_version_was_found() {
        let text = message(&SetupError::TooOld(GitVersion {
            major: 2,
            minor: 30,
            patch: 1,
        }));
        assert!(text.contains("2.30.1"));
    }
}
