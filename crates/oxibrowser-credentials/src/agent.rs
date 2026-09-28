//! [`BrokerSource`] — the [`CredentialSource`] adapter over the credential
//! broker (M-D, upper design §5.3).
//!
//! The core agent-login engine stays dependency-clean behind the
//! `CredentialSource` port; this type is the credentials-side wiring: exact
//! allowlist check ([`OriginPolicy`], core M1) → [`PolicyEngine`]
//! authorization → [`CredentialProvider::resolve`] → TOTP code generation,
//! with the `credential_use` audit line pairing each fill to its
//! `credential_read` via the value fingerprint (§5.5).
//!
//! Values exist only in the return values; errors carry reasons only.

use std::sync::Arc;

use oxibrowser_core::account::{CredentialSource, ResolvedLogin, SourceError};
use oxibrowser_core::network::origin_policy::{Decision, Origin, OriginPolicy};
use oxibrowser_core::security::audit::{
    self, AuditDecision, AuditEvent, AuditEventKind, CredentialRef,
};
use zeroize::Zeroizing;

use crate::policy::{CredentialAction, PolicyEngine, UseRequest};
use crate::provider::{CredentialId, CredentialKind, CredentialMeta, CredentialProvider};
use crate::totp::TotpGenerator;

/// Broker-backed credential source: provider + policy engine.
pub struct BrokerSource {
    pub provider: Arc<dyn CredentialProvider>,
    pub engine: Arc<PolicyEngine>,
}

impl BrokerSource {
    pub fn new(provider: Arc<dyn CredentialProvider>, engine: Arc<PolicyEngine>) -> Self {
        BrokerSource { provider, engine }
    }

    /// The account's candidates for `kind`: handles parsed from the record,
    /// metadata known to the provider, matching kind and scope.
    fn candidates(
        &self,
        handles: &[String],
        scope: &str,
        kind: CredentialKind,
    ) -> Result<Vec<(CredentialId, CredentialMeta)>, SourceError> {
        let mut out = Vec::new();
        for handle in handles {
            let id = CredentialId(handle.clone());
            match self.provider.metadata(&id) {
                Ok(meta) if meta.kind == kind && meta.scope == scope => {
                    out.push((id, meta));
                }
                Ok(_) => {}
                Err(crate::error::CredError::NotFound(_)) => {}
                Err(e) => {
                    return Err(SourceError::Unavailable {
                        reason: e.to_string(),
                    });
                }
            }
        }
        // Accounts whose records predate their credentials (or were never
        // linked) resolve through the provider's scope listing instead —
        // the scope IS the linkage of record.
        if out.is_empty() {
            for meta in self
                .provider
                .list(None)
                .map_err(|e| SourceError::Unavailable {
                    reason: e.to_string(),
                })?
            {
                if meta.kind == kind && meta.scope == scope {
                    out.push((CredentialId(meta.id.0.clone()), meta));
                }
            }
        }
        Ok(out)
    }

    /// The exact-origin gate (core M1, fail-closed): the form origin must be
    /// in the credential's allowlist for both top page and hosting frame.
    fn allowlisted(meta: &CredentialMeta, origin: &Origin) -> Result<Vec<Origin>, SourceError> {
        let mut allowed = Vec::with_capacity(meta.allowed_origins.len());
        for stored in &meta.allowed_origins {
            allowed.push(
                Origin::parse(stored).map_err(|e| SourceError::OriginMismatch {
                    reason: format!(
                        "credential {} has unusable allowed origin {stored}: {e}",
                        meta.id
                    ),
                })?,
            );
        }
        if OriginPolicy::default().evaluate(&allowed, origin, Some(origin)) != Decision::Allow {
            return Err(SourceError::OriginMismatch {
                reason: format!(
                    "credential {} is not allowed at {}",
                    meta.id,
                    origin.as_str()
                ),
            });
        }
        Ok(allowed)
    }

    /// Policy authorization for one use. `RequireConfirmation` is denied for
    /// unattended flows — a card cannot be answered mid-login; the host
    /// sorts consent out before starting one (deny-biased composition).
    fn authorize(&self, request: &UseRequest) -> Result<(), SourceError> {
        match self.engine.authorize_use(request) {
            Decision::Allow => Ok(()),
            Decision::RequireConfirmation { reason } | Decision::Deny { reason } => {
                Err(SourceError::ConsentRequired { reason })
            }
        }
    }

    /// Pair the completed fill with its `credential_read` line (§5.5).
    fn audit_fill(&self, request: &UseRequest, fingerprint: &str) {
        let event = AuditEvent {
            origin: Some(request.top_level.as_str()),
            action: Some(request.action.as_str().to_string()),
            credential: Some(CredentialRef {
                id: request.credential.0.clone(),
                fingerprint: fingerprint.to_string(),
            }),
            ..audit::event(
                AuditEventKind::CredentialUse,
                AuditDecision::Allow,
                "confirmed".to_string(),
            )
        };
        if let Err(e) = self.engine.audit.record(event) {
            tracing::warn!(error = %e, "agent login fill audit write failed");
        }
    }
}

impl CredentialSource for BrokerSource {
    fn resolve_login(
        &self,
        handles: &[String],
        scope: &str,
        origin: &Origin,
    ) -> Result<ResolvedLogin, SourceError> {
        let candidates = self.candidates(handles, scope, CredentialKind::Password)?;
        let Some((id, meta)) = candidates.into_iter().next() else {
            return Err(SourceError::NotFound {
                reason: format!("no password credential for scope {scope}"),
            });
        };
        let allowed = Self::allowlisted(&meta, origin)?;

        let mut request = UseRequest::new(id.clone(), origin.clone(), CredentialAction::Login);
        request.frame = Some(origin.clone());
        self.authorize(&request)?;

        let (meta, secret) = self.provider.resolve(&id).map_err(|e| match &e {
            crate::error::CredError::NotFound(_) => SourceError::NotFound {
                reason: e.to_string(),
            },
            crate::error::CredError::KeyStoreUnavailable(_)
            | crate::error::CredError::Keyring(_)
            | crate::error::CredError::Io(_) => SourceError::Unavailable {
                reason: e.to_string(),
            },
            _ => SourceError::Unavailable {
                reason: e.to_string(),
            },
        })?;
        let value = secret
            .expose_str()
            .map_err(|e| SourceError::Unavailable {
                reason: e.to_string(),
            })?
            .to_string();
        let fingerprint = secret.fingerprint();
        drop(secret);
        self.audit_fill(&request, &fingerprint);

        Ok(ResolvedLogin {
            handle: id.0,
            username: meta.login_hint,
            password: Zeroizing::new(value),
            fingerprint,
            allowed_origins: allowed,
        })
    }

    fn resolve_totp(
        &self,
        handles: &[String],
        scope: &str,
        origin: &Origin,
    ) -> Result<String, SourceError> {
        let candidates = self.candidates(handles, scope, CredentialKind::Totp)?;
        let Some((id, meta)) = candidates.into_iter().next() else {
            return Err(SourceError::NotFound {
                reason: format!("no totp credential for scope {scope}"),
            });
        };
        if !meta.has_totp {
            return Err(SourceError::NotFound {
                reason: format!("totp credential {id} carries no otpauth value"),
            });
        }
        Self::allowlisted(&meta, origin)?;

        let mut request = UseRequest::new(id.clone(), origin.clone(), CredentialAction::Mfa);
        request.frame = Some(origin.clone());
        self.authorize(&request)?;

        let (_meta, secret) = self
            .provider
            .resolve(&id)
            .map_err(|e| SourceError::Unavailable {
                reason: e.to_string(),
            })?;
        let uri = secret
            .expose_str()
            .map_err(|e| SourceError::Unavailable {
                reason: e.to_string(),
            })?
            .to_string();
        // The code is generated in-broker and injected directly — never
        // returned to any surface (design §6.3).
        let (code, _validity) = TotpGenerator::from_otpauth(&uri)
            .map_err(|e| SourceError::Unavailable {
                reason: e.to_string(),
            })?
            .current()
            .map_err(|e| SourceError::Unavailable {
                reason: e.to_string(),
            })?;
        let fingerprint = oxibrowser_core::security::audit::secret_fingerprint(code.as_bytes());
        drop(secret);
        self.audit_fill(&request, &fingerprint);
        Ok(code)
    }

    /// `allowed_origins` of every password/api-key credential for `scope` —
    /// scheme + host + port exactly as pinned. Metadata only, no values.
    /// Discovers via the provider's scope listing rather than the account
    /// record's handle list, so discovery works before the two are linked.
    fn origin_hints(&self, _handles: &[String], scope: &str) -> Vec<String> {
        let list = match self.provider.list(None) {
            Ok(l) => l,
            Err(_) => return Vec::new(),
        };
        let mut hints: Vec<String> = Vec::new();
        for meta in list
            .iter()
            .filter(|m| m.scope == scope)
            .filter(|m| matches!(m.kind, CredentialKind::Password | CredentialKind::ApiKey))
        {
            for o in &meta.allowed_origins {
                if !hints.contains(o) {
                    hints.push(o.clone());
                }
            }
        }
        hints
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PolicyEngine;
    use crate::consent::{ConsentRecord, ConsentStore, ConsentSubject};
    use crate::provider::{InMemoryProvider, NewCredential};
    use crate::secret::SecretBox;
    use oxibrowser_core::security::audit::AuditLog;

    fn engine_with(consents: ConsentStore) -> Arc<PolicyEngine> {
        let dir = crate::unique_temp_dir("agent-broker");
        let audit = Arc::new(AuditLog::open(dir.join("audit.jsonl")).unwrap());
        Arc::new(PolicyEngine::new(Vec::new(), consents, audit))
    }

    fn seed(provider: &InMemoryProvider, origins: &[&str]) -> String {
        provider
            .put(NewCredential {
                agent_id: "main".into(),
                scope: "example.test".into(),
                kind: CredentialKind::Password,
                slug: "login".into(),
                allowed_origins: origins.iter().map(|o| o.to_string()).collect(),
                login_hint: Some("user@example.test".into()),
                password: Some(SecretBox::from_string("hunter2".into())),
                otpauth_uri: None,
            })
            .unwrap()
            .0
    }

    fn grant_credential(consents: &ConsentStore, credential: &str, origin: &str) {
        consents
            .grant(ConsentRecord::new(
                ConsentSubject::Credential {
                    credential: CredentialId(credential.to_string()),
                },
                origin,
                &["login"],
                chrono::Duration::hours(1),
                10,
            ))
            .unwrap();
    }

    fn origin(s: &str) -> Origin {
        Origin::parse(s).unwrap()
    }

    #[test]
    fn resolve_login_happy_path_carries_values_and_origins() {
        let provider = Arc::new(InMemoryProvider::new());
        let handle = seed(&provider, &["https://example.test"]);
        let consents =
            ConsentStore::open(crate::unique_temp_dir("agent-consents").join("c.jsonl")).unwrap();
        grant_credential(&consents, &handle, "https://example.test");
        let source = BrokerSource::new(provider, engine_with(consents));

        let resolved = source
            .resolve_login(
                std::slice::from_ref(&handle),
                "example.test",
                &origin("https://example.test"),
            )
            .unwrap();
        assert_eq!(resolved.handle, handle);
        assert_eq!(resolved.username.as_deref(), Some("user@example.test"));
        assert_eq!(resolved.password.as_str(), "hunter2");
        assert!(resolved.fingerprint.starts_with("sha256:"));
        assert_eq!(resolved.allowed_origins.len(), 1);
    }

    #[test]
    fn resolve_login_requires_consent_and_exact_origin() {
        let provider = Arc::new(InMemoryProvider::new());
        let handle = seed(&provider, &["https://example.test"]);
        let consents =
            ConsentStore::open(crate::unique_temp_dir("agent-consents").join("c.jsonl")).unwrap();
        let source = BrokerSource::new(
            Arc::clone(&provider) as Arc<dyn CredentialProvider>,
            engine_with(consents),
        );

        // No grant → ConsentRequired.
        let err = match source.resolve_login(
            std::slice::from_ref(&handle),
            "example.test",
            &origin("https://example.test"),
        ) {
            Err(e) => e,
            Ok(_) => panic!("no grant must deny"),
        };
        assert!(matches!(err, SourceError::ConsentRequired { .. }));

        // Credential whose exact allowlist excludes the form origin: M1
        // fires before consent is even consulted.
        let provider2 = Arc::new(InMemoryProvider::new());
        let foreign = seed(&provider2, &["https://other.test"]);
        let consents2 =
            ConsentStore::open(crate::unique_temp_dir("agent-consents").join("c.jsonl")).unwrap();
        let source2 = BrokerSource::new(
            Arc::clone(&provider2) as Arc<dyn CredentialProvider>,
            engine_with(consents2),
        );
        let err = match source2.resolve_login(
            &[foreign],
            "example.test",
            &origin("https://example.test"),
        ) {
            Err(e) => e,
            Ok(_) => panic!("allowlist miss must deny"),
        };
        assert!(matches!(err, SourceError::OriginMismatch { .. }), "{err:?}");
    }

    #[test]
    fn resolve_totp_missing_is_not_found() {
        let provider = Arc::new(InMemoryProvider::new());
        let consents =
            ConsentStore::open(crate::unique_temp_dir("agent-consents").join("c.jsonl")).unwrap();
        let source = BrokerSource::new(
            Arc::clone(&provider) as Arc<dyn CredentialProvider>,
            engine_with(consents),
        );
        let err = match source.resolve_totp(&[], "example.test", &origin("https://example.test")) {
            Err(e) => e,
            Ok(_) => panic!("no totp credential must be NotFound"),
        };
        assert!(matches!(err, SourceError::NotFound { .. }));
    }

    #[test]
    fn preflight_is_non_consuming() {
        let provider = Arc::new(InMemoryProvider::new());
        let handle = seed(&provider, &["https://example.test"]);
        let consents =
            ConsentStore::open(crate::unique_temp_dir("agent-consents").join("c.jsonl")).unwrap();
        grant_credential(&consents, &handle, "https://example.test");
        let engine = engine_with(consents);

        let mut request = UseRequest::new(
            CredentialId(handle.clone()),
            origin("https://example.test"),
            CredentialAction::Login,
        );
        request.frame = Some(origin("https://example.test"));
        assert!(engine.preflight(&request));
        assert!(engine.preflight(&request), "preflight never consumes");
        // The consuming path still works after any number of preflights.
        assert!(matches!(engine.authorize_use(&request), Decision::Allow));
        // Grant exhausted? No — max_uses 10. One more preflight still peeks.
        assert!(engine.preflight(&request));
    }
}
