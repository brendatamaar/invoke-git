//! Forge accounts and repo lists (P8): GitHub, GitLab and self-hosted GitLab.
//!
//! Optional by design: nothing in the git paths of this crate refers to it,
//! so every git feature works with no account. Tokens are only ever held as
//! [`Secret`]s and rest in a [`TokenStore`] (the OS keychain in the app).
//!
//! The parts, from the bottom:
//! - `json`, `codec`: pure readers and encoders (JSON, base64url, SHA-256,
//!   PKCE, `Link` headers), checked against published vectors
//! - `secrets`, `error`: the token types, the keychain and memory stores, and
//!   errors that never carry a secret
//! - `http`: the one HTTP sender (https only, no redirects, bounded answers)
//! - `api`: the GitHub and GitLab calls
//! - `service`: accounts, tokens and repo listing together
//! - `signin`: carries out the sign-in machine's commands

pub mod api;
pub mod codec;
pub mod error;
pub mod http;
pub mod json;
pub mod random;
pub mod secrets;
pub mod service;
pub mod signin;

pub use api::{GitHubUrls, RepoPage};
pub use error::ForgeError;
pub use secrets::{Credential, KeyringStore, MemoryStore, Secret, TokenStore};
pub use service::{ClientIds, ForgeService, GITLAB_LOOPBACK_PORT, Listing, ServiceConfig};
pub use signin::SignInRunner;
