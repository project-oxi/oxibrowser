//! Append-only JSONL audit log for security-relevant events.
//!
//! Values never enter the log — credentials are referenced by handle plus a
//! SHA-256 fingerprint (first 8 hex chars). The log is process-global: the
//! binary initializes it at startup (default `~/.oxibrowser/audit.jsonl`,
//! `--audit <PATH>` to relocate, `--no-audit` to disable); library consumers
//! that never call [`init`] get a no-op logger so tests and embedders don't
//! write files implicitly.
//!
//! Trust boundary: the log is written by the same user's process, so it is
//! tamper-evident ordering, not tamper-proof storage (design §9 FM-8).

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// Audit event classification (design §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEventKind {
    /// Credential use decision (emitted from P1 policy engine).
    CredentialUse,
    /// A credential-store read itself (success or failure).
    CredentialRead,
    /// A sensitive side effect (raw HAR export, cookie-file save).
    SensitiveAction,
    /// Session teardown (cookie jar disposal counts).
    SessionTeardown,
    /// Policy violation or blocked action.
    PolicyViolation,
    /// Account lifecycle transition (upper design §6.4) — `action` carries
    /// `from->to`, `reason` the trigger detail.
    AccountState,
    /// Account context binding / grant consumption (agent_id in `reason`).
    AccountUse,
    /// Session envelope sealed for an account scope (`session_capture`).
    SessionCapture,
    /// Session envelope loaded into a live session (`session_restore`);
    /// fingerprint-gate rejections are `deny` decisions.
    SessionRestore,
    /// Session envelope disposed (logout) (`session_discard`).
    SessionDiscard,
}

/// Decision outcome recorded alongside the event kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditDecision {
    Allow,
    Deny,
    Prompt,
    Timeout,
}

/// Log-safe credential reference: handle plus value fingerprint only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialRef {
    pub id: String,
    /// `"sha256:ab12cd34"` — first 8 hex chars of the value's SHA-256.
    pub fingerprint: String,
}

/// Current audit line schema. Stamped on every event at `record` time;
/// per-event (not a file header) so partial mirrors and rotated files stay
/// self-describing. Consumers that join or mirror `audit.jsonl` must treat
/// lines with an unknown `schema_version` as opaque.
pub const AUDIT_SCHEMA_VERSION: u32 = 1;

/// One audit log line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Schema revision of this line (see [`AUDIT_SCHEMA_VERSION`]).
    #[serde(default)]
    pub schema_version: u32,
    /// RFC 3339 UTC with millisecond precision.
    pub ts: String,
    /// Process-local monotonic sequence number. **Not unique across
    /// processes** — concurrent writers interleave lines with colliding
    /// `seq` values. Correlation and deduplication must use `event_id`.
    pub seq: u64,
    /// Globally-unique event id (`evt-<instance>-<seq>`), stamped at
    /// `record` time. `<instance>` is per-`AuditLog`-open (random-ish
    /// nanos + pid), so pid reuse cannot collide ids either.
    #[serde(default)]
    pub event_id: String,
    pub kind: AuditEventKind,
    /// Caller-supplied correlation tag (`--ref`) — the join key between the
    /// audit ledger, consent records, and external task/run receipts.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub ref_tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<CredentialRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    pub decision: AuditDecision,
    pub reason: String,
}

/// Append-only JSONL writer. One event per line, flushed per event.
/// Interior-mutable so a shared `&AuditLog` (global) can record.
pub struct AuditLog {
    file: parking_lot::Mutex<std::fs::File>,
    seq: AtomicU64,
    /// Per-open uniqueness domain for `event_id`s (see [`AuditEvent`]).
    instance: String,
}

impl AuditLog {
    /// Open (creating parent directories and the file) at `path`.
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            file: parking_lot::Mutex::new(file),
            seq: AtomicU64::new(0),
            instance: instance_id(),
        })
    }

    /// Append one event and flush. Errors propagate — callers decide whether
    /// audit failure is fatal (it is not, in P0). Stamps `seq`,
    /// `schema_version`, and (when unset) `event_id`.
    pub fn record(&self, mut event: AuditEvent) -> std::io::Result<()> {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        event.seq = seq;
        event.schema_version = AUDIT_SCHEMA_VERSION;
        if event.event_id.is_empty() {
            event.event_id = format!("evt-{}-{seq}", self.instance);
        }
        let mut line = serde_json::to_string(&event)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        let mut file = self.file.lock();
        file.write_all(line.as_bytes())?;
        file.flush()
    }
}

/// Uniqueness-domain component for [`AuditEvent::event_id`]: hex nanos +
/// pid, fresh per [`AuditLog::open`]. No randomness dependency needed —
/// nanosecond wall clock plus pid is collision-free for practical purposes,
/// and `seq` disambiguates within the open.
fn instance_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}-{}", nanos, std::process::id())
}

static AUDIT: OnceLock<Option<AuditLog>> = OnceLock::new();

/// Default audit log location: `~/.oxibrowser/audit.jsonl`.
pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".oxibrowser").join("audit.jsonl"))
}

/// Initialize the global audit log.
///
/// - `Some(Some(path))` — write to `path` (`--audit <PATH>`).
/// - `Some(None)` — write to the default path.
/// - `None` — audit disabled (`--no-audit`).
///
/// Open failure degrades to disabled with a tracing error (design §7 P0-4:
/// availability over enforced audit in P0). A second call is a no-op — the
/// first initialization wins.
pub fn init(choice: Option<Option<PathBuf>>) {
    let resolved = match choice {
        None => None,
        Some(Some(path)) => AuditLog::open(&path)
            .map_err(|e| {
                tracing::error!(path = %path.display(), error = %e, "audit log open failed; audit disabled");
            })
            .ok(),
        Some(None) => match default_path() {
            Some(path) => AuditLog::open(&path)
                .map_err(|e| {
                    tracing::error!(path = %path.display(), error = %e, "audit log open failed; audit disabled");
                })
                .ok(),
            None => None,
        },
    };
    let _ = AUDIT.set(resolved);
}

/// True when a usable global audit log is installed.
pub fn enabled() -> bool {
    AUDIT.get().map(|a| a.is_some()).unwrap_or(false)
}

/// Record an event on the global log; no-op when audit is uninitialized or
/// disabled. Errors are logged, never propagated.
pub fn record(event: AuditEvent) {
    if let Some(Some(log)) = AUDIT.get()
        && let Err(e) = log.record(event)
    {
        tracing::warn!(error = %e, "audit log write failed");
    }
}

/// Convenience constructor for the common shape of P0 events.
pub fn event(
    kind: AuditEventKind,
    decision: AuditDecision,
    reason: impl Into<String>,
) -> AuditEvent {
    AuditEvent {
        schema_version: AUDIT_SCHEMA_VERSION,
        ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        seq: 0,
        event_id: String::new(),
        ref_tag: None,
        kind,
        session_id: None,
        tab_id: None,
        origin: None,
        credential: None,
        action: None,
        decision,
        reason: reason.into(),
    }
}

/// `"sha256:" + first 8 hex chars` of `value` — the log-safe fingerprint.
pub fn secret_fingerprint(value: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(value);
    format!("sha256:{}", hex_prefix8(&digest))
}

fn hex_prefix8(digest: &[u8]) -> String {
    digest.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonl_roundtrip_and_monotonic_seq() {
        let dir = std::env::temp_dir().join(format!("oxi-audit-test-{}", std::process::id()));
        let path = dir.join("audit.jsonl");
        let _ = std::fs::remove_file(&path);
        let log = AuditLog::open(&path).unwrap();
        log.record(event(
            AuditEventKind::SessionTeardown,
            AuditDecision::Allow,
            "t1",
        ))
        .unwrap();
        log.record(event(
            AuditEventKind::PolicyViolation,
            AuditDecision::Deny,
            "t2",
        ))
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: AuditEvent = serde_json::from_str(lines[0]).unwrap();
        let second: AuditEvent = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(first.seq, 0);
        assert_eq!(second.seq, 1);
        assert_eq!(first.kind, AuditEventKind::SessionTeardown);
        assert_eq!(second.decision, AuditDecision::Deny);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_secret_leaks_via_known_value() {
        // The type has no field that would carry a raw secret; assert the
        // serialized form of a fully-populated event contains nothing but the
        // fingerprint for the credential.
        let ev = AuditEvent {
            credential: Some(CredentialRef {
                id: "kch:main/x.com/password/a".into(),
                fingerprint: secret_fingerprint(b"hunter2"),
            }),
            ..event(
                AuditEventKind::CredentialRead,
                AuditDecision::Allow,
                "broker_resolve",
            )
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(!json.contains("hunter2"), "{json}");
        assert!(json.contains("sha256:"), "{json}");
        assert_eq!(secret_fingerprint(b"hunter2").len(), "sha256:".len() + 8);
    }

    #[test]
    fn uninitialized_global_is_noop() {
        // In the test binary `init` may have run via another test; record must
        // simply not panic either way.
        record(event(
            AuditEventKind::SensitiveAction,
            AuditDecision::Allow,
            "noop-check",
        ));
    }

    #[test]
    fn record_stamps_schema_version_event_id_and_ref() {
        let dir = std::env::temp_dir().join(format!("oxi-audit-schema-{}", std::process::id()));
        let path = dir.join("audit.jsonl");
        let _ = std::fs::remove_file(&path);
        let log = AuditLog::open(&path).unwrap();
        let mut with_ref = event(AuditEventKind::AccountUse, AuditDecision::Allow, "r1");
        with_ref.ref_tag = Some("run-42".into());
        log.record(with_ref).unwrap();
        log.record(event(
            AuditEventKind::SessionRestore,
            AuditDecision::Allow,
            "r2",
        ))
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        // Per-event schema stamp + `ref` join key under its contract name.
        assert!(lines[0].contains("\"schema_version\":1"), "{}", lines[0]);
        assert!(lines[0].contains("\"ref\":\"run-42\""), "{}", lines[0]);
        assert!(!lines[1].contains("\"ref\""), "{}", lines[1]);
        let first: AuditEvent = serde_json::from_str(lines[0]).unwrap();
        let second: AuditEvent = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(first.schema_version, AUDIT_SCHEMA_VERSION);
        assert_eq!(first.ref_tag.as_deref(), Some("run-42"));
        // event ids are stamped and unique within (and across) opens.
        assert!(!first.event_id.is_empty());
        assert_ne!(first.event_id, second.event_id);
        // Pre-schema lines (no schema_version/event_id/ref) still parse.
        let old = r#"{"ts":"2026-01-01T00:00:00.000Z","seq":3,"kind":"credential_use","decision":"allow","reason":"x"}"#;
        let parsed: AuditEvent = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.schema_version, 0);
        assert!(parsed.event_id.is_empty());
        assert!(parsed.ref_tag.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
