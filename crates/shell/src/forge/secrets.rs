//! Tokens and where they live.
//!
//! A token only ever exists as a [`Secret`] (which refuses to print itself)
//! and only ever rests in a [`TokenStore`]: the OS keychain in the app, an
//! in-memory map in tests. There is no file backend on purpose, so a token
//! cannot end up in a plain text config.

use std::collections::HashMap;
use std::sync::Mutex;

use super::error::ForgeError;

/// A token or other secret. `Debug` hides it; the only way to read it is
/// [`Secret::expose`], which the HTTP layer calls to build one header.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// `None` for an empty token, or one with whitespace or control
    /// characters in it (a pasted token with a line break is a mistake, and
    /// the stored form is line-based).
    pub fn new(text: &str) -> Option<Secret> {
        let text = text.trim();
        if text.is_empty() || text.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return None;
        }
        Some(Secret(text.to_owned()))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// What the keychain holds for one account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credential {
    pub access: Secret,
    /// OAuth only; GitLab tokens expire after two hours.
    pub refresh: Option<Secret>,
    /// Seconds since the Unix epoch.
    pub expires_at: Option<u64>,
}

impl Credential {
    pub fn token(access: Secret) -> Credential {
        Credential {
            access,
            refresh: None,
            expires_at: None,
        }
    }

    /// Whether the access token has run out (or will within `margin` seconds).
    pub fn expires_within(&self, now: u64, margin: u64) -> bool {
        self.expires_at
            .is_some_and(|expires| expires <= now.saturating_add(margin))
    }

    /// The single string kept in the keychain.
    pub fn encode(&self) -> String {
        format!(
            "v1\n{}\n{}\n{}",
            self.access.expose(),
            self.refresh.as_ref().map_or("", Secret::expose),
            self.expires_at.map(|t| t.to_string()).unwrap_or_default()
        )
    }

    pub fn decode(text: &str) -> Option<Credential> {
        let mut lines = text.split('\n');
        if lines.next()? != "v1" {
            return None;
        }
        let access = Secret::new(lines.next()?)?;
        let refresh = match lines.next()? {
            "" => None,
            text => Some(Secret::new(text)?),
        };
        let expires_at = match lines.next()? {
            "" => None,
            text => Some(text.parse().ok()?),
        };
        lines.next().is_none().then_some(Credential {
            access,
            refresh,
            expires_at,
        })
    }
}

/// Where credentials rest, keyed by `ForgeAccount::key`.
pub trait TokenStore: Send + Sync {
    fn save(&self, key: &str, credential: &Credential) -> Result<(), ForgeError>;
    fn load(&self, key: &str) -> Result<Option<Credential>, ForgeError>;
    /// Deleting what is not there is not an error.
    fn delete(&self, key: &str) -> Result<(), ForgeError>;
}

/// Credentials in memory: for tests and for CI, where no keychain exists.
#[derive(Default)]
pub struct MemoryStore {
    map: Mutex<HashMap<String, String>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many credentials are held (tests).
    pub fn len(&self) -> usize {
        self.map.lock().map(|map| map.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl TokenStore for MemoryStore {
    fn save(&self, key: &str, credential: &Credential) -> Result<(), ForgeError> {
        self.map
            .lock()
            .map_err(|_| ForgeError::Keychain("store poisoned".into()))?
            .insert(key.to_owned(), credential.encode());
        Ok(())
    }

    fn load(&self, key: &str) -> Result<Option<Credential>, ForgeError> {
        let map = self
            .map
            .lock()
            .map_err(|_| ForgeError::Keychain("store poisoned".into()))?;
        Ok(map.get(key).and_then(|text| Credential::decode(text)))
    }

    fn delete(&self, key: &str) -> Result<(), ForgeError> {
        self.map
            .lock()
            .map_err(|_| ForgeError::Keychain("store poisoned".into()))?
            .remove(key);
        Ok(())
    }
}

/// The OS keychain: Windows Credential Manager, macOS Keychain, the Secret
/// Service on Linux. One entry per account, named by the account key.
pub struct KeyringStore {
    service: String,
}

impl KeyringStore {
    pub const SERVICE: &'static str = "Invoke Git forge accounts";

    pub fn new() -> Self {
        Self {
            service: Self::SERVICE.to_owned(),
        }
    }

    fn entry(&self, key: &str) -> Result<keyring::Entry, ForgeError> {
        keyring::Entry::new(&self.service, key).map_err(keychain_error)
    }
}

impl Default for KeyringStore {
    fn default() -> Self {
        Self::new()
    }
}

/// A keychain error as a short, fixed phrase. The library's own text is not
/// passed on: it is not written to hold secrets, but the rule here is that
/// nothing from outside goes into a message.
fn keychain_error(error: keyring::Error) -> ForgeError {
    let why = match error {
        keyring::Error::NoStorageAccess(_) => "access to the keychain was refused",
        keyring::Error::NoDefaultStore | keyring::Error::PlatformFailure(_) => {
            "no keychain is available on this system"
        }
        keyring::Error::TooLong(..) => "the credential is too long for the keychain",
        _ => "the keychain reported an error",
    };
    ForgeError::Keychain(why.into())
}

impl TokenStore for KeyringStore {
    fn save(&self, key: &str, credential: &Credential) -> Result<(), ForgeError> {
        self.entry(key)?
            .set_password(&credential.encode())
            .map_err(keychain_error)
    }

    fn load(&self, key: &str) -> Result<Option<Credential>, ForgeError> {
        match self.entry(key)?.get_password() {
            Ok(text) => Ok(Credential::decode(&text)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(keychain_error(error)),
        }
    }

    fn delete(&self, key: &str) -> Result<(), ForgeError> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(keychain_error(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn secret(text: &str) -> Secret {
        Secret::new(text).unwrap()
    }

    #[test]
    fn a_secret_never_prints_and_rejects_what_the_store_cannot_hold() {
        let token = secret("ghp_abc123");
        assert!(!format!("{token:?}").contains("abc123"));
        let credential = Credential::token(token);
        assert!(!format!("{credential:?}").contains("abc123"));
        for bad in ["", "  ", "a b", "a\nb", "a\u{7}b"] {
            assert!(Secret::new(bad).is_none(), "{bad:?}");
        }
        assert_eq!(secret("  tok \n").expose(), "tok");
    }

    #[test]
    fn a_credential_survives_its_stored_form() {
        for credential in [
            Credential::token(secret("a")),
            Credential {
                access: secret("a"),
                refresh: Some(secret("r")),
                expires_at: Some(1_700_000_000),
            },
            Credential {
                access: secret("a"),
                refresh: None,
                expires_at: Some(5),
            },
        ] {
            assert_eq!(Credential::decode(&credential.encode()), Some(credential));
        }
        for bad in [
            "",
            "v2\na\n\n",
            "v1\n\n\n",
            "v1\na\n\nxyz",
            "v1\na\n\n\nextra",
            "v1\na",
        ] {
            assert_eq!(Credential::decode(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn expiry_is_judged_with_a_margin() {
        let mut credential = Credential::token(secret("a"));
        assert!(
            !credential.expires_within(1_000, 60),
            "no expiry never expires"
        );
        credential.expires_at = Some(1_100);
        assert!(!credential.expires_within(1_000, 60));
        assert!(credential.expires_within(1_000, 100));
        assert!(credential.expires_within(2_000, 0));
    }

    #[test]
    fn the_memory_store_saves_loads_and_deletes() {
        let store = MemoryStore::new();
        assert_eq!(store.load("k").unwrap(), None);
        let credential = Credential::token(secret("tok"));
        store.save("k", &credential).unwrap();
        assert_eq!(store.load("k").unwrap(), Some(credential));
        assert_eq!(store.len(), 1);
        store.delete("k").unwrap();
        store.delete("k").unwrap();
        assert!(store.is_empty());
    }

    proptest! {
        #[test]
        fn any_valid_credential_round_trips(
            access in "[!-~]{1,40}",
            refresh in proptest::option::of("[!-~]{1,40}"),
            expires in proptest::option::of(any::<u32>()),
        ) {
            let credential = Credential {
                access: secret(&access),
                refresh: refresh.map(|r| secret(&r)),
                expires_at: expires.map(u64::from),
            };
            prop_assert_eq!(Credential::decode(&credential.encode()), Some(credential));
        }
    }
}
