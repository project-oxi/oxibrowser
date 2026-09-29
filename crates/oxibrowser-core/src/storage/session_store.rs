//! OXSESS1 session envelope store (top-level design §6.1, sub-design §5.3).
//!
//! One [`SessionEnvelope`] per account scope (`<registrable domain>.session`),
//! sealed with XChaCha20-Poly1305 into the binary frame:
//!
//! ```text
//! "OXSESS1\n"   8-byte magic + format version (also the AEAD associated data)
//! u32 LE        nonce length (fixed 24, XChaCha20-Poly1305)
//! [24]          nonce
//! remainder     AEAD ciphertext of the envelope JSON (tag appended)
//! ```
//!
//! The 32-byte key comes from a [`KeyProvider`] (keychain-backed implementation
//! lives in the credentials crate); there is deliberately **no plaintext
//! fallback** — a missing key must abort the operation, never degrade (§5.3,
//! FM-6). Saves replace the target file atomically via temp-file + rename, so a
//! crashed writer can never leave a torn envelope behind.

use crate::error::{CoreError, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use zeroize::Zeroizing;

/// Magic + format version prefix of every OXSESS1 file; also the AEAD
/// associated data binding the header bytes to the ciphertext.
pub const SESSION_FILE_MAGIC: &[u8; 8] = b"OXSESS1\n";

/// XChaCha20-Poly1305 nonce size in bytes.
const NONCE_LEN: usize = 24;

/// Envelope format version produced (and accepted) by this module.
const ENVELOPE_VERSION: u32 = 1;

/// Browser fingerprint a session was captured under (sub-design §5.3).
///
/// Session cookies and anti-bot posture are tied to this, so [`SessionStore::load`]
/// rejects reuse under a different fingerprint unless the caller explicitly
/// overrides the check (`expected: None`, CLI `--fingerprint-override`).
/// All fields default generously so partial metadata still round-trips.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(default)]
pub struct FingerprintMeta {
    /// Full User-Agent string used during the session.
    pub user_agent: String,
    /// Client hints relevant to the fingerprint.
    pub client_hints: ClientHints,
    /// `navigator.language`-style locale, e.g. `ko-KR`.
    pub locale: String,
    /// IANA timezone, e.g. `Asia/Seoul`.
    pub timezone: String,
}

/// Client-hint subset of [`FingerprintMeta`].
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ClientHints {
    /// `Sec-CH-UA-Platform`, e.g. `macOS`. `None` when never negotiated.
    pub platform: Option<String>,
}

/// Egress posture of the session (sub-design §5.3).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[derive(Default)]
pub enum EgressMeta {
    /// Traffic observed through a known fixed egress, e.g. a home WAN.
    Fixed {
        /// Free-form label of the egress, `None` when unlabeled.
        #[serde(default)]
        note: Option<String>,
    },
    /// Egress not tracked / unspecified.
    #[default]
    Unspecified,
}

/// Plaintext payload of a session file (encrypted into an OXSESS1 frame).
///
/// `state` is the Playwright-interchangeable [`crate::storage_state::StorageState`]
/// captured via [`crate::session::Session::export_state_for_scope`], so envelopes
/// remain convertible with Playwright `storageState` JSON.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionEnvelope {
    /// Format version; this module writes 1 and rejects anything else on load.
    #[serde(default = "default_envelope_version")]
    pub version: u32,
    /// RFC 3339 creation timestamp; preserved across re-saves.
    pub created_at: String,
    /// RFC 3339 last-write timestamp; bumped with [`SessionEnvelope::touch`].
    pub updated_at: String,
    /// Registrable domain this session belongs to, e.g. `tailscale.com`.
    pub scope: String,
    /// Fingerprint the session was captured under.
    #[serde(default)]
    pub fingerprint: FingerprintMeta,
    /// Egress posture of the session.
    #[serde(default)]
    pub egress: EgressMeta,
    /// Cookies + per-origin localStorage (Playwright `storageState` shape).
    #[serde(default)]
    pub state: crate::storage_state::StorageState,
}

fn default_envelope_version() -> u32 {
    ENVELOPE_VERSION
}

impl SessionEnvelope {
    /// New envelope with `version = 1` and both timestamps set to now.
    pub fn new(
        scope: impl Into<String>,
        fingerprint: FingerprintMeta,
        egress: EgressMeta,
        state: crate::storage_state::StorageState,
    ) -> Self {
        let now = rfc3339_now();
        SessionEnvelope {
            version: ENVELOPE_VERSION,
            created_at: now.clone(),
            updated_at: now,
            scope: scope.into(),
            fingerprint,
            egress,
            state,
        }
    }

    /// Bump `updated_at` to now (call before re-saving a loaded envelope).
    pub fn touch(&mut self) {
        self.updated_at = rfc3339_now();
    }
}

fn rfc3339_now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Supplies the 32-byte AEAD key for one scope.
///
/// Implemented by the credentials crate's keychain provider; [`StaticKeyProvider`]
/// exists for tests and bootstrap tooling. Keys are wrapped in [`Zeroizing`] so
/// they are wiped on drop and never printed by `Debug`.
pub trait KeyProvider: Send + Sync {
    /// The session-sealing key for `scope`, or an error when the keystore is
    /// unavailable (callers must abort, never fall back to plaintext).
    fn session_key(&self, scope: &str) -> Result<Zeroizing<[u8; 32]>>;
}

/// In-memory fixed-key [`KeyProvider`] for tests and bootstrap tooling.
#[derive(Clone)]
pub struct StaticKeyProvider {
    key: Zeroizing<[u8; 32]>,
}

impl StaticKeyProvider {
    /// Provider over an exact 32-byte key.
    pub fn new(key: [u8; 32]) -> Self {
        StaticKeyProvider {
            key: Zeroizing::new(key),
        }
    }

    /// Provider over a byte slice; errors unless exactly 32 bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let key: [u8; 32] = bytes
            .try_into()
            .map_err(|_| invalid_key_length(bytes.len()))?;
        Ok(StaticKeyProvider::new(key))
    }
}

impl KeyProvider for StaticKeyProvider {
    fn session_key(&self, _scope: &str) -> Result<Zeroizing<[u8; 32]>> {
        Ok(self.key.clone())
    }
}

fn invalid_key_length(len: usize) -> CoreError {
    CoreError::SessionStoreAead(format!("session key must be 32 bytes, got {len}"))
}

/// One entry of [`SessionStore::list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeSummary {
    /// Account scope (registrable domain) derived from the file name.
    pub scope: String,
    /// Last modification time of the session file.
    pub mtime: SystemTime,
}

/// Scope-keyed AEAD session file store rooted at `dir`
/// (default deployment: `~/.oxibrowser/accounts/<id>/sessions/`, design §6.1).
pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    /// Store rooted at `dir`; the directory is created (mode 0700 on unix)
    /// on first save.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        SessionStore { dir: dir.into() }
    }

    /// Root directory of this store.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path of `scope`'s session file (without checking existence).
    pub fn path_for(&self, scope: &str) -> PathBuf {
        self.dir.join(format!("{scope}.session"))
    }

    /// Serialize `env`, seal it with the scope key, and atomically replace
    /// `<scope>.session` (temp file + rename, mode 0600 on unix). The file on
    /// disk is always a complete valid envelope — readers never observe a
    /// torn write.
    pub fn save(&self, env: &SessionEnvelope, keys: &dyn KeyProvider) -> Result<PathBuf> {
        let scope = validate_scope(&env.scope)?;
        let plaintext =
            serde_json::to_vec(env).map_err(|e| CoreError::SessionStoreFormat(e.to_string()))?;
        let key = keys.session_key(scope)?;
        let cipher = cipher_for(&key)?;
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(|e| CoreError::SessionStoreAead(e.to_string()))?;
        let ciphertext = cipher
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &plaintext,
                    aad: SESSION_FILE_MAGIC.as_slice(),
                },
            )
            .map_err(|e| CoreError::SessionStoreAead(e.to_string()))?;

        let mut blob =
            Vec::with_capacity(SESSION_FILE_MAGIC.len() + 4 + NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(SESSION_FILE_MAGIC);
        blob.extend_from_slice(&(NONCE_LEN as u32).to_le_bytes());
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&ciphertext);

        let final_path = self.path_for(scope);
        write_atomic(&self.dir, &final_path, &blob)?;
        Ok(final_path)
    }

    /// Read, authenticate, and decrypt `scope`'s envelope.
    ///
    /// When `expected` is `Some`, the stored [`FingerprintMeta`] is compared
    /// first and any difference fails closed with
    /// [`CoreError::SessionFingerprintMismatch`] — overriding requires passing
    /// `expected: None` (the `--fingerprint-override` path).
    pub fn load(
        &self,
        scope: &str,
        keys: &dyn KeyProvider,
        expected: Option<&FingerprintMeta>,
    ) -> Result<SessionEnvelope> {
        let scope = validate_scope(scope)?;
        let blob = fs::read(self.path_for(scope)).map_err(io_err)?;

        let header_len = SESSION_FILE_MAGIC.len() + 4;
        if blob.len() < header_len + NONCE_LEN + 16 {
            return Err(CoreError::SessionStoreFormat(format!(
                "session file truncated ({} bytes)",
                blob.len()
            )));
        }
        if &blob[..SESSION_FILE_MAGIC.len()] != SESSION_FILE_MAGIC {
            return Err(CoreError::SessionStoreFormat(
                "bad session file magic".to_string(),
            ));
        }
        let nonce_len = u32::from_le_bytes(blob[8..12].try_into().expect("4 bytes")) as usize;
        if nonce_len != NONCE_LEN {
            return Err(CoreError::SessionStoreFormat(format!(
                "unexpected nonce length {nonce_len} (expected {NONCE_LEN})"
            )));
        }
        let nonce =
            XNonce::try_from(&blob[12..12 + NONCE_LEN]).expect("nonce length validated above");
        let ciphertext = &blob[12 + NONCE_LEN..];

        let key = keys.session_key(scope)?;
        let cipher = cipher_for(&key)?;
        let plaintext = cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: ciphertext,
                    aad: SESSION_FILE_MAGIC.as_slice(),
                },
            )
            .map_err(|_| {
                CoreError::SessionStoreAead(
                    "session decryption failed (wrong key or corrupted/tampered file)".to_string(),
                )
            })?;

        let env: SessionEnvelope = serde_json::from_slice(&plaintext)
            .map_err(|e| CoreError::SessionStoreFormat(e.to_string()))?;
        if env.version != ENVELOPE_VERSION {
            return Err(CoreError::SessionStoreFormat(format!(
                "unsupported envelope version {} (expected {ENVELOPE_VERSION})",
                env.version
            )));
        }
        if env.scope != scope {
            return Err(CoreError::SessionStoreFormat(format!(
                "envelope scope {:?} does not match file scope {scope:?}",
                env.scope
            )));
        }
        if let Some(expected) = expected
            && let Some(diff) = fingerprint_diff(expected, &env.fingerprint)
        {
            return Err(CoreError::SessionFingerprintMismatch(diff));
        }
        Ok(env)
    }

    /// Delete `scope`'s session file; `Ok(true)` when it existed.
    pub fn discard(&self, scope: &str) -> Result<bool> {
        let scope = validate_scope(scope)?;
        match fs::remove_file(self.path_for(scope)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_err(e)),
        }
    }

    /// All stored scopes (missing directory → empty), sorted by scope.
    pub fn list(&self) -> Result<Vec<ScopeSummary>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err(e)),
        };
        let mut summaries = Vec::new();
        for entry in entries {
            let entry = entry.map_err(io_err)?;
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            let Some(scope) = name.strip_suffix(".session") else {
                continue;
            };
            if validate_scope(scope).is_err() {
                continue;
            }
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .map_err(io_err)?;
            summaries.push(ScopeSummary {
                scope: scope.to_string(),
                mtime,
            });
        }
        summaries.sort_by(|a, b| a.scope.cmp(&b.scope));
        Ok(summaries)
    }
}

fn cipher_for(key: &[u8; 32]) -> Result<XChaCha20Poly1305> {
    XChaCha20Poly1305::new_from_slice(key)
        .map_err(|e| CoreError::SessionStoreAead(format!("invalid session key: {e}")))
}

/// Reject scopes that could escape the store directory via the file name.
fn validate_scope(scope: &str) -> Result<&str> {
    if scope.is_empty()
        || scope.len() > 255
        || scope == "."
        || scope == ".."
        || scope.contains(['/', '\\', '\0'])
    {
        return Err(CoreError::SessionStoreFormat(format!(
            "invalid session scope {scope:?}"
        )));
    }
    Ok(scope)
}

/// Compare current vs stored fingerprint; `Some(description)` lists every
/// differing field (fingerprints carry no secrets, so details are loggable).
fn fingerprint_diff(expected: &FingerprintMeta, stored: &FingerprintMeta) -> Option<String> {
    let mut diffs = Vec::new();
    if expected.user_agent != stored.user_agent {
        diffs.push(format!(
            "user_agent (stored {:?}, current {:?})",
            stored.user_agent, expected.user_agent
        ));
    }
    if expected.client_hints.platform != stored.client_hints.platform {
        diffs.push(format!(
            "client_hints.platform (stored {:?}, current {:?})",
            stored.client_hints.platform, expected.client_hints.platform
        ));
    }
    if expected.locale != stored.locale {
        diffs.push(format!(
            "locale (stored {:?}, current {:?})",
            stored.locale, expected.locale
        ));
    }
    if expected.timezone != stored.timezone {
        diffs.push(format!(
            "timezone (stored {:?}, current {:?})",
            stored.timezone, expected.timezone
        ));
    }
    if diffs.is_empty() {
        None
    } else {
        Some(diffs.join("; "))
    }
}

fn io_err(e: std::io::Error) -> CoreError {
    CoreError::SessionStoreIo(e.to_string())
}

/// Write `blob` to `final_path` via a uniquely named temp file in the same
/// directory (rename is atomic within one filesystem). The temp file is
/// created 0600 on unix and removed on any failure.
fn write_atomic(dir: &Path, final_path: &Path, blob: &[u8]) -> Result<()> {
    fs::create_dir_all(dir).map_err(io_err)?;
    #[cfg(unix)]
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(io_err)?;

    let tmp = dir.join(format!(
        ".{}.{}.{}.tmp",
        final_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("session"),
        std::process::id(),
        uuid::Uuid::new_v4().as_simple()
    ));
    let write_result = (|| -> std::io::Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&tmp)?;
        file.write_all(blob)?;
        file.sync_all()
    })();
    match write_result {
        Ok(()) => {}
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            return Err(io_err(e));
        }
    }
    match fs::rename(&tmp, final_path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(io_err(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage_state::{LocalStorageEntry, OriginState};

    fn temp_store(tag: &str) -> (SessionStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "oxi-sess-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().as_simple()
        ));
        (SessionStore::new(dir.clone()), dir)
    }

    fn key() -> StaticKeyProvider {
        StaticKeyProvider::from_bytes(&[7u8; 32]).unwrap()
    }

    fn fingerprint(ua: &str) -> FingerprintMeta {
        FingerprintMeta {
            user_agent: ua.to_string(),
            client_hints: ClientHints {
                platform: Some("macOS".to_string()),
            },
            locale: "ko-KR".to_string(),
            timezone: "Asia/Seoul".to_string(),
        }
    }

    fn envelope(scope: &str, cookie_value: &str) -> SessionEnvelope {
        SessionEnvelope::new(
            scope,
            fingerprint("TestUA/1.0"),
            EgressMeta::Fixed {
                note: Some("home-wan".to_string()),
            },
            crate::storage_state::StorageState {
                cookies: vec![crate::network::cookie::CookieEntry {
                    name: "sid".to_string(),
                    value: cookie_value.to_string(),
                    domain: Some(scope.to_string()),
                    ..Default::default()
                }],
                origins: vec![OriginState {
                    origin: format!("https://{scope}"),
                    local_storage: vec![LocalStorageEntry {
                        name: "token".to_string(),
                        value: cookie_value.to_string(),
                    }],
                indexed_db: None,
            }],
            },
        )
    }

    /// Round-trip equality via the serialized form (`SessionEnvelope` holds
    /// foreign types without `PartialEq`).
    fn assert_envelopes_equal(a: &SessionEnvelope, b: &SessionEnvelope) {
        assert_eq!(
            serde_json::to_value(a).unwrap(),
            serde_json::to_value(b).unwrap()
        );
    }

    #[test]
    fn save_then_load_roundtrips_with_same_key() {
        let (store, dir) = temp_store("roundtrip");
        let env = envelope("github.com", "v1");

        let path = store.save(&env, &key()).unwrap();
        assert_eq!(path, store.path_for("github.com"));
        assert!(path.is_file());

        let loaded = store
            .load("github.com", &key(), Some(&env.fingerprint))
            .unwrap();
        assert_envelopes_equal(&env, &loaded);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn wrong_key_and_tampering_fail_authentication() {
        let (store, dir) = temp_store("tamper");
        store.save(&envelope("github.com", "v1"), &key()).unwrap();
        let path = store.path_for("github.com");

        // Different key → authentication failure, not a panic or plaintext.
        let other_key = StaticKeyProvider::from_bytes(&[9u8; 32]).unwrap();
        let err = store.load("github.com", &other_key, None).unwrap_err();
        assert!(matches!(err, CoreError::SessionStoreAead(_)), "{err:?}");

        // Flipped ciphertext byte → AEAD authentication failure.
        let mut blob = std::fs::read(&path).unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0xff;
        std::fs::write(&path, &blob).unwrap();
        let err = store.load("github.com", &key(), None).unwrap_err();
        assert!(matches!(err, CoreError::SessionStoreAead(_)), "{err:?}");

        // Bad magic and bad nonce length are format errors.
        let mut blob = std::fs::read(&path).unwrap();
        blob[0] = b'X';
        std::fs::write(&path, &blob).unwrap();
        let err = store.load("github.com", &key(), None).unwrap_err();
        assert!(matches!(err, CoreError::SessionStoreFormat(_)), "{err:?}");

        let mut blob = std::fs::read(&path).unwrap();
        blob[0] = b'O';
        blob[8..12].copy_from_slice(&23u32.to_le_bytes());
        std::fs::write(&path, &blob).unwrap();
        let err = store.load("github.com", &key(), None).unwrap_err();
        assert!(matches!(err, CoreError::SessionStoreFormat(_)), "{err:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resave_atomically_replaces_and_leaves_no_temp_files() {
        let (store, dir) = temp_store("atomic");
        let mut env = envelope("github.com", "0");
        store.save(&env, &key()).unwrap();

        // A concurrent reader must only ever observe complete envelopes while
        // the writer re-saves 50 times (rename is atomic on unix).
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader_stop = stop.clone();
        let reader_store_dir = dir.clone();
        let reader = std::thread::spawn(move || {
            let reader_store = SessionStore::new(reader_store_dir);
            let reader_key = key();
            let mut loads = 0usize;
            while !reader_stop.load(std::sync::atomic::Ordering::Relaxed) {
                let env = reader_store.load("github.com", &reader_key, None).unwrap();
                let value = env.state.cookies[0].value.clone();
                let version: u32 = value.parse().unwrap();
                assert!(version <= 49);
                loads += 1;
            }
            loads
        });

        for i in 0..50 {
            env = envelope("github.com", &i.to_string());
            env.touch();
            store.save(&env, &key()).unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let loads = reader.join().unwrap();
        assert!(loads > 0, "reader never ran");

        let final_env = store.load("github.com", &key(), None).unwrap();
        assert_eq!(final_env.state.cookies[0].value, "49");
        assert_eq!(store.list().unwrap().len(), 1);
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 1, "temp files leaked: {leftovers:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fingerprint_mismatch_is_rejected_unless_overridden() {
        let (store, dir) = temp_store("fingerprint");
        let env = envelope("github.com", "v1");
        store.save(&env, &key()).unwrap();

        let mut changed = fingerprint("TestUA/1.0");
        changed.locale = "en-US".to_string();
        let err = store
            .load("github.com", &key(), Some(&changed))
            .unwrap_err();
        match err {
            CoreError::SessionFingerprintMismatch(msg) => {
                assert!(msg.contains("locale"), "diff should name the field: {msg}");
                assert!(!msg.contains("user_agent"), "only differing fields: {msg}");
            }
            other => panic!("expected fingerprint mismatch, got {other:?}"),
        }

        // Override path: `expected: None` skips the check.
        let loaded = store.load("github.com", &key(), None).unwrap();
        assert_eq!(loaded.fingerprint.locale, "ko-KR");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn discard_list_and_scope_validation() {
        let (store, dir) = temp_store("list");
        assert!(store.list().unwrap().is_empty());

        assert!(!store.discard("github.com").unwrap());
        store.save(&envelope("github.com", "v1"), &key()).unwrap();
        store
            .save(&envelope("tailscale.com", "v1"), &key())
            .unwrap();

        let scopes: Vec<_> = store.list().unwrap().into_iter().map(|s| s.scope).collect();
        assert_eq!(scopes, vec!["github.com", "tailscale.com"]);
        assert!(store.discard("github.com").unwrap());
        assert_eq!(store.list().unwrap().len(), 1);

        // Path-escaping scopes never reach the filesystem.
        for bad in ["../evil", "", "a/b", ".."] {
            let err = store.save(&envelope(bad, "v"), &key()).unwrap_err();
            assert!(
                matches!(err, CoreError::SessionStoreFormat(_)),
                "{bad}: {err:?}"
            );
        }
        assert_eq!(store.list().unwrap().len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_session_file_is_an_io_error() {
        let (store, _dir) = temp_store("missing");
        let err = store.load("nosuch.com", &key(), None).unwrap_err();
        assert!(matches!(err, CoreError::SessionStoreIo(_)), "{err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn files_are_0600_and_dir_is_0700() {
        use std::os::unix::fs::PermissionsExt;
        let (store, dir) = temp_store("perms");
        store.save(&envelope("github.com", "v1"), &key()).unwrap();

        let file_mode = std::fs::metadata(store.path_for("github.com"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(file_mode & 0o777, 0o600);
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(dir_mode & 0o777, 0o700);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
