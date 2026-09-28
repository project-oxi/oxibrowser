//! [`AccountRegistry`] — the on-disk account store (upper design §4.1, §6.1).
//!
//! Layout under `base_dir` (default `~/.oxibrowser/accounts`):
//!
//! ```text
//! accounts/                  0700
//! └── <account-id>/          0700
//!     ├── account.json       0600 — atomic replace, complete file always
//!     └── sessions/          0700 — per-account SessionStore root
//!         └── <scope>.session
//! ```
//!
//! Record writes go through a temp file + rename (atomic within one
//! filesystem), so a crashed writer can never leave a torn `account.json`.
//! The registry itself holds no secrets and performs no audit — lifecycle
//! events are audited by [`super::AccountManager`].

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::storage::session_store::SessionStore;

use super::record::{AccountRecord, AccountState, account_error, validate_account_id};

/// Store of account records rooted at `base_dir`.
#[derive(Debug, Clone)]
pub struct AccountRegistry {
    base_dir: PathBuf,
}

impl AccountRegistry {
    /// Default root: `$HOME/.oxibrowser/accounts` (§6.1). `None` when `HOME`
    /// is unset.
    pub fn default_dir() -> Option<PathBuf> {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(".oxibrowser").join("accounts"))
    }

    /// Registry rooted at `base_dir`; the directory tree (0700) is created
    /// eagerly so permission errors surface at construction.
    pub fn open(base_dir: impl Into<PathBuf>) -> Result<Self> {
        let base_dir = base_dir.into();
        ensure_dir_0700(&base_dir)?;
        Ok(AccountRegistry { base_dir })
    }

    /// Root directory of this registry.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Per-account directory.
    pub fn account_dir(&self, account_id: &str) -> PathBuf {
        self.base_dir.join(account_id)
    }

    /// Per-account `sessions/` root (§6.1).
    pub fn sessions_dir(&self, account_id: &str) -> PathBuf {
        self.account_dir(account_id).join("sessions")
    }

    /// Path of the account's `account.json`.
    pub fn record_path(&self, account_id: &str) -> PathBuf {
        self.account_dir(account_id).join("account.json")
    }

    /// True when `<account-id>/account.json` exists.
    pub fn exists(&self, account_id: &str) -> bool {
        validate_account_id(account_id).is_ok() && self.record_path(account_id).is_file()
    }

    /// Session store rooted at the account's `sessions/` directory. Does not
    /// create anything — `add` provisions the tree; the store creates its
    /// directory on first save.
    pub fn session_store(&self, account_id: &str) -> Result<SessionStore> {
        validate_account_id(account_id)?;
        Ok(SessionStore::new(self.sessions_dir(account_id)))
    }

    /// Create a new account record; the directory (0700) is created and the
    /// record written 0600. Fails when the account already exists.
    pub fn add(&self, record: AccountRecord) -> Result<AccountRecord> {
        validate_account_id(&record.account_id)?;
        let dir = self.account_dir(&record.account_id);
        if self.record_path(&record.account_id).exists() {
            return Err(account_error(format!(
                "account {:?} already exists",
                record.account_id
            )));
        }
        ensure_dir_0700(&dir)?;
        ensure_dir_0700(&self.sessions_dir(&record.account_id))?;
        self.write_record(&record)?;
        Ok(record)
    }

    /// Load one account record.
    pub fn get(&self, account_id: &str) -> Result<AccountRecord> {
        validate_account_id(account_id)?;
        let path = self.record_path(account_id);
        let bytes = fs::read(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                account_error(format!("account {account_id:?} not found"))
            }
            _ => account_error(format!("reading {}: {e}", path.display())),
        })?;
        serde_json::from_slice(&bytes)
            .map_err(|e| account_error(format!("parsing {}: {e}", path.display())))
    }

    /// All account records, sorted by id. Directories without a readable
    /// `account.json` are skipped with a warning (partial/corrupt state must
    /// not hide the healthy accounts).
    pub fn list(&self) -> Result<Vec<AccountRecord>> {
        let entries = match fs::read_dir(&self.base_dir) {
            Ok(entries) => entries,
            Err(e) => {
                return Err(account_error(format!(
                    "reading {}: {e}",
                    self.base_dir.display()
                )));
            }
        };
        let mut records = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| account_error(format!("readdir: {e}")))?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let record_path = path.join("account.json");
            if !record_path.is_file() {
                continue;
            }
            match fs::read(&record_path)
                .map_err(|e| e.to_string())
                .and_then(|bytes| {
                    serde_json::from_slice::<AccountRecord>(&bytes).map_err(|e| e.to_string())
                }) {
                Ok(record) => records.push(record),
                Err(e) => tracing::warn!(
                    path = %record_path.display(),
                    error = %e,
                    "skipping unreadable account record"
                ),
            }
        }
        records.sort_by(|a, b| a.account_id.cmp(&b.account_id));
        Ok(records)
    }

    /// Persist `record` (atomic 0600 replace). The record's id must match an
    /// existing account directory.
    pub fn save(&self, record: &AccountRecord) -> Result<()> {
        validate_account_id(&record.account_id)?;
        if !self.account_dir(&record.account_id).is_dir() {
            return Err(account_error(format!(
                "account {:?} not found",
                record.account_id
            )));
        }
        self.write_record(record)
    }

    /// Load → enforce the §4.2 state machine → persist. Returns the updated
    /// record.
    pub fn set_state(
        &self,
        account_id: &str,
        to: AccountState,
        detail: Option<String>,
    ) -> Result<AccountRecord> {
        let mut record = self.get(account_id)?;
        record.transition(to, detail)?;
        self.save(&record)?;
        Ok(record)
    }

    /// Remove the account directory — record, `sessions/` envelopes, and all
    /// (design §4.2 `revoke`). Returns `true` when the account existed.
    pub fn remove(&self, account_id: &str) -> Result<bool> {
        validate_account_id(account_id)?;
        let dir = self.account_dir(account_id);
        if !dir.exists() {
            return Ok(false);
        }
        fs::remove_dir_all(&dir)
            .map_err(|e| account_error(format!("removing {}: {e}", dir.display())))?;
        Ok(true)
    }

    fn write_record(&self, record: &AccountRecord) -> Result<()> {
        let json = serde_json::to_vec_pretty(record)
            .map_err(|e| account_error(format!("serializing account.json: {e}")))?;
        write_atomic_0600(&self.account_dir(&record.account_id), "account.json", &json)
    }
}

/// Create `dir` (and parents) with mode 0700 on unix; best-effort chmod when
/// it already exists with looser permissions.
fn ensure_dir_0700(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = fs::create_dir_all(dir)
            && e.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(account_error(format!("creating {}: {e}", dir.display())));
        }
        let meta =
            fs::metadata(dir).map_err(|e| account_error(format!("stat {}: {e}", dir.display())))?;
        if meta.is_dir() && (meta.permissions().mode() & 0o777) != 0o700 {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                .map_err(|e| account_error(format!("chmod {}: {e}", dir.display())))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
            .map_err(|e| account_error(format!("creating {}: {e}", dir.display())))?;
        Ok(())
    }
}

/// Write `bytes` to `dir/name` via a uniquely named temp file in `dir`
/// (rename is atomic within one filesystem), created 0600 on unix. A failure
/// anywhere removes the temp file; readers never observe a torn file.
pub(crate) fn write_atomic_0600(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let tmp = tempfile_name(dir, name);
    let write = |path: &Path| -> std::io::Result<()> {
        #[cfg(unix)]
        let file = {
            use std::os::unix::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)?
        };
        #[cfg(not(unix))]
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        (&file).write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(path, dir.join(name))
    };
    match write(&tmp) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(account_error(format!(
                "writing {} atomically: {e}",
                dir.join(name).display()
            )))
        }
    }
}

fn tempfile_name(dir: &Path, name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let stem = name.split('.').next().unwrap_or(name);
    dir.join(format!(".{stem}.tmp-{}-{n}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "oxi-account-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn sample(id: &str, scope: &str) -> AccountRecord {
        AccountRecord::new(id, scope).unwrap()
    }

    #[test]
    fn add_get_list_round_trip() {
        let dir = temp_dir("roundtrip");
        let reg = AccountRegistry::open(&dir).unwrap();
        let mut gh = sample("gh-work", "github.com");
        gh.identity.display_name = Some("Garden (work)".into());
        reg.add(gh.clone()).unwrap();
        reg.add(sample("gh-personal", "github.com")).unwrap();

        assert_eq!(reg.get("gh-work").unwrap(), gh);
        let list = reg.list().unwrap();
        let ids: Vec<_> = list.iter().map(|r| r.account_id.as_str()).collect();
        assert_eq!(ids, ["gh-personal", "gh-work"]);
        assert!(reg.exists("gh-work"));
        assert!(!reg.exists("nope"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn add_rejects_duplicate_and_bad_id() {
        let dir = temp_dir("dup");
        let reg = AccountRegistry::open(&dir).unwrap();
        reg.add(sample("gh", "github.com")).unwrap();
        assert!(reg.add(sample("gh", "github.com")).is_err());
        // registry re-validates the id even when handed a mutated record
        let mut bad = sample("placeholder", "example.com");
        bad.account_id = "Bad_ID".into();
        assert!(reg.add(bad).is_err());
        let mut upper = sample("placeholder", "example.com");
        upper.account_id = "GH".into();
        assert!(reg.add(upper).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn set_state_enforces_machine_and_persists() {
        let dir = temp_dir("state");
        let reg = AccountRegistry::open(&dir).unwrap();
        reg.add(sample("a", "example.com")).unwrap();
        let rec = reg
            .set_state("a", AccountState::Valid, Some("captured".into()))
            .unwrap();
        assert_eq!(rec.state, AccountState::Valid);
        // illegal: stale -> logging_in is legal, but needs_login -> stale is not
        reg.set_state("a", AccountState::Stale, Some("probe_401".into()))
            .unwrap();
        assert!(reg.set_state("a", AccountState::Revoked, None).is_err());
        // persisted?
        assert_eq!(reg.get("a").unwrap().state, AccountState::Stale);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writes_are_atomic_with_strict_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("perms");
        let reg = AccountRegistry::open(&dir).unwrap();
        reg.add(sample("p", "example.com")).unwrap();

        let meta = fs::metadata(reg.record_path("p")).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600, "record 0600");
        let base_mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(base_mode, 0o700, "registry dir 0700");
        let acct_mode = fs::metadata(reg.account_dir("p"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(acct_mode, 0o700, "account dir 0700");
        // no temp residue after successful writes
        let residue: Vec<_> = fs::read_dir(reg.account_dir("p"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(residue.is_empty(), "temp files leaked: {residue:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_rejects_unknown_account() {
        let dir = temp_dir("unknown");
        let reg = AccountRegistry::open(&dir).unwrap();
        let mut rec = sample("ghost", "example.com");
        rec.state = AccountState::Valid;
        assert!(reg.save(&rec).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn remove_deletes_record_and_sessions() {
        let dir = temp_dir("rm");
        let reg = AccountRegistry::open(&dir).unwrap();
        reg.add(sample("gone", "example.com")).unwrap();
        assert!(reg.account_dir("gone").is_dir());
        assert!(reg.remove("gone").unwrap());
        assert!(!reg.remove("gone").unwrap());
        assert!(!reg.account_dir("gone").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn list_skips_corrupt_entries() {
        let dir = temp_dir("corrupt");
        let reg = AccountRegistry::open(&dir).unwrap();
        reg.add(sample("good", "example.com")).unwrap();
        let bad = dir.join("bad");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("account.json"), "{not json").unwrap();
        let ids: Vec<_> = reg
            .list()
            .unwrap()
            .iter()
            .map(|r| r.account_id.clone())
            .collect();
        assert_eq!(ids, ["good"]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
