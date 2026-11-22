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
