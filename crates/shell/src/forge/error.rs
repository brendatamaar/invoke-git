//! What can go wrong talking to a forge or the keychain.
//!
//! Every message here is built from fixed text, status codes and OAuth error
//! *codes*: never from a token, a request URL, a response body or a library
//! error's own text, because any of those can carry a secret.

use invoke_core::forge::{ForgeKind, OriginProblem};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForgeError {
    /// This build has no OAuth client id for the forge (see `config`).
    NotConfigured(ForgeKind),
    BadOrigin(OriginProblem),
    /// A URL that is not https (or loopback): refused before anything is sent.
    InsecureUrl,
    /// Could not connect, or the connection broke.
    Network,
    TimedOut,
    /// 401: the token is wrong, expired or revoked.
    Unauthorized,
    /// 403 without a rate-limit sign: the token lacks a scope, or SSO is needed.
    Forbidden,
    NotFound,
    RateLimited {
        retry_after_secs: Option<u64>,
    },
    Server(u16),
    Status(u16),
    /// The answer was not what the API documents.
    BadResponse(&'static str),
    /// The OAuth server refused; this is its error code (letters and
    /// underscores only).
    OAuth(String),
    /// The OS keychain refused or is unavailable.
    Keychain(String),
    /// The account list could not be read or written.
    Disk(String),
    UnknownAccount,
    /// The account exists but its token is gone from the keychain.
    NoCredential,
    /// The pasted token is empty or has spaces or line breaks in it.
    InvalidToken,
    /// Signing in is not possible right now (for example the loopback port
    /// is taken).
    Unavailable(String),
}

impl ForgeError {
    /// Whether signing in again (or a new token) would help.
    pub fn needs_reconnect(&self) -> bool {
        matches!(self, ForgeError::Unauthorized | ForgeError::NoCredential)
    }

    /// A sentence for the person using the app.
    pub fn user_message(&self) -> String {
        match self {
            ForgeError::NotConfigured(kind) => format!(
                "This build of Invoke Git has no {} sign-in app set up yet. Use an access token instead.",
                kind.label()
            ),
            ForgeError::BadOrigin(problem) => match problem {
                OriginProblem::Empty => "Enter the address of the server.".into(),
                OriginProblem::Scheme => "The address must start with https://.".into(),
                OriginProblem::Insecure => {
                    "Use https:// for this server. A token must not travel unencrypted.".into()
                }
                OriginProblem::UserInfo => {
                    "Leave the user name and password out of the address.".into()
                }
                OriginProblem::Path => {
                    "Enter only the server address (https://host), without a path.".into()
                }
                OriginProblem::Host => "That is not a valid server name.".into(),
                OriginProblem::Port => "That is not a valid port.".into(),
            },
            ForgeError::InsecureUrl => {
                "Refused to send a token over an unencrypted connection.".into()
            }
            ForgeError::Network => "Could not reach the server. Check the connection.".into(),
            ForgeError::TimedOut => "The server did not answer in time.".into(),
            ForgeError::Unauthorized => {
                "The server did not accept the token. It may be wrong, expired or revoked.".into()
            }
            ForgeError::Forbidden => {
                "The token is not allowed to do this. Check its scopes, or whether your organization requires SSO for it."
                    .into()
            }
            ForgeError::NotFound => "The server does not know that address.".into(),
            ForgeError::RateLimited {
                retry_after_secs: Some(secs),
            } => format!("Rate limited by the server. Try again in {secs} seconds."),
            ForgeError::RateLimited {
                retry_after_secs: None,
            } => "Rate limited by the server. Try again in a few minutes.".into(),
            ForgeError::Server(code) => format!("The server had a problem (HTTP {code})."),
            ForgeError::Status(code) => format!("The server answered with HTTP {code}."),
            ForgeError::BadResponse(what) => {
                format!("The server sent something unexpected ({what}).")
            }
            ForgeError::OAuth(code) => format!("The sign-in was refused ({code})."),
            ForgeError::Keychain(why) => {
                format!("The system keychain could not be used: {why}")
            }
            ForgeError::Disk(why) => format!("The account list could not be saved: {why}"),
            ForgeError::UnknownAccount => "That account is no longer connected.".into(),
            ForgeError::NoCredential => {
                "The saved token is missing from the keychain. Connect the account again.".into()
            }
            ForgeError::InvalidToken => {
                "That does not look like a token: it is empty or has spaces or line breaks.".into()
            }
            ForgeError::Unavailable(why) => why.clone(),
        }
    }
}

impl std::fmt::Display for ForgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.user_message())
    }
}

impl std::error::Error for ForgeError {}

/// Keeps an OAuth error code to letters, digits and underscores, so a
/// hostile server cannot put a sentence (or a token) in it.
pub fn clean_code(code: &str) -> String {
    let cleaned: String = code
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(48)
        .collect();
    if cleaned.is_empty() {
        "unknown_error".into()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hostile_error_code_is_flattened() {
        assert_eq!(clean_code("access_denied"), "access_denied");
        assert_eq!(clean_code("a b\nc=ghp_secret"), "abcghp_secret");
        assert_eq!(clean_code("\n\r "), "unknown_error");
        assert_eq!(clean_code(&"x".repeat(500)).len(), 48);
    }

    #[test]
    fn only_a_missing_or_refused_token_asks_to_reconnect() {
        assert!(ForgeError::Unauthorized.needs_reconnect());
        assert!(ForgeError::NoCredential.needs_reconnect());
        assert!(!ForgeError::Network.needs_reconnect());
        assert!(!ForgeError::Forbidden.needs_reconnect());
    }
}
