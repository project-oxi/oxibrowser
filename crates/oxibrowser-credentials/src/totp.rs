//! TOTP generation — [`TotpGenerator`] (lower design §4.3 `totp.rs`, §4.4
//! dependency choice).
//!
//! Generation rides on `totp-rs` 6 (its digest-0.11 tree is self-contained
//! and does not touch the workspace's `sha1 0.10`). Parsing accepts the
//! Google Key URI Format; configuration is normalized at construction so
//! accessors report canonical issuer/algorithm/digits/period values.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::CredError;

/// While fewer than this many seconds remain in the current window,
/// [`TotpGenerator::current`] returns the next window's code instead —
/// a code about to expire is useless for a form fill (design §2.2 guard).
pub const WINDOW_BUMP_SECS: u64 = 3;

/// RFC 6238 code generator over a normalized configuration.
pub struct TotpGenerator {
    inner: totp_rs::Totp,
}

impl TotpGenerator {
    /// Restore a generator from an `otpauth://` URI (Google Key URI Format).
    ///
    /// Label and query are normalized: issuer from the `issuer` parameter or
    /// the `Issuer:account` label, algorithm (default SHA-1), digits
    /// (default 6), period (default 30s). Digits must be 6 or 8 and the
    /// period at least 1s — RFC 6238/Google spec. Short secrets (e.g. the
    /// 10-byte ones many sites ship) are accepted; `totp-rs`'s strict
    /// 128-bit minimum would reject real-world URIs.
    pub fn from_otpauth(uri: &str) -> Result<Self, CredError> {
        let inner = totp_rs::Totp::from_url_unchecked(uri)
            .map_err(|e| CredError::Invalid(format!("otpauth URI rejected: {e}")))?;
        if !(6..=8).contains(&inner.digits()) {
            return Err(CredError::Invalid(format!(
                "otpauth digits must be 6 or 8, got {}",
                inner.digits()
            )));
        }
        if inner.step() == 0 {
            return Err(CredError::Invalid(
                "otpauth period must be at least 1s".to_string(),
            ));
        }
        Ok(Self { inner })
    }

    /// Build a generator from a bare base32 secret with RFC 6238 defaults:
    /// SHA-1, 6 digits, 30-second period.
    pub fn from_base32(secret: &str) -> Result<Self, CredError> {
        let normalized = secret.trim().to_ascii_uppercase();
        if normalized.is_empty() {
            return Err(CredError::Totp("empty base32 secret".to_string()));
        }
        let decoded = totp_rs::Secret::try_from_base32(&normalized)
            .map_err(|e| CredError::Totp(format!("base32 secret rejected: {e}")))?;
        let inner = totp_rs::Builder::new()
            .with_algorithm(totp_rs::Algorithm::SHA1)
            .with_digits(6)
            .with_skew(0)
            .with_step_duration(30)
            .with_account_name("oxibrowser")
            .with_secret(decoded)
            .build_noncompliant();
        Ok(Self { inner })
    }

    /// Current code plus the time left before it expires. Fewer than
    /// [`WINDOW_BUMP_SECS`] seconds left → the next window's code plus its
    /// full validity (boundary-expiry guard).
    pub fn current(&self) -> Result<(String, Duration), CredError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| CredError::Totp(format!("system clock before unix epoch: {e}")))?
            .as_secs();
        Ok(self.window_at(now))
    }

    /// Code for an explicit unix timestamp (deterministic path for tests and
    /// clock-free verification).
    pub fn generate_at(&self, unix_seconds: u64) -> String {
        self.inner.generate(unix_seconds).to_string()
    }

    /// Window logic over an explicit `now` — the pure core of [`Self::current`].
    pub fn window_at(&self, now: u64) -> (String, Duration) {
        let period = self.inner.step().max(1);
        let remaining = period - (now % period);
        if remaining < WINDOW_BUMP_SECS {
            (
                self.generate_at(now + remaining),
                Duration::from_secs(remaining + period),
            )
        } else {
            (self.generate_at(now), Duration::from_secs(remaining))
        }
    }

    /// Issuer as normalized from the URI (parameter or label).
    pub fn issuer(&self) -> Option<&str> {
        self.inner.issuer()
    }

    /// Digits per code.
    pub fn digits(&self) -> usize {
        self.inner.digits() as usize
    }

    /// Window period in seconds.
    pub fn period(&self) -> u64 {
        self.inner.step()
    }

    /// Hash algorithm name: `SHA1`, `SHA256`, or `SHA512`.
    pub fn algorithm_name(&self) -> &'static str {
        match self.inner.algorithm() {
            totp_rs::Algorithm::SHA1 => "SHA1",
            totp_rs::Algorithm::SHA256 => "SHA256",
            totp_rs::Algorithm::SHA512 => "SHA512",
            _ => "unknown",
        }
    }

    /// Base32 encoding of the underlying secret (record round-trips).
    pub fn secret_base32(&self) -> String {
        self.inner.secret().to_base32()
    }
}

impl std::fmt::Debug for TotpGenerator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The inner secret never appears in debug output.
        f.debug_struct("TotpGenerator")
            .field("algorithm", &self.algorithm_name())
            .field("digits", &self.digits())
            .field("period", &self.period())
            .field("issuer", &self.issuer())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use totp_rs::Algorithm;

    const RFC_SECRET_20: &[u8] = b"12345678901234567890";
    const RFC_SECRET_32: &[u8] = b"12345678901234567890123456789012";
    const RFC_SECRET_64: &[u8] =
        b"1234567890123456789012345678901234567890123456789012345678901234";

    fn make_gen(alg: Algorithm, digits: usize, secret: &[u8]) -> TotpGenerator {
        TotpGenerator {
            inner: totp_rs::Builder::new()
                .with_algorithm(alg)
                .with_digits(digits as u8)
                .with_skew(0)
                .with_step_duration(30)
                .with_account_name("rfc6238")
                .with_secret(secret.to_vec())
                .build()
                .unwrap(),
        }
    }

    #[test]
    fn rfc6238_sha1_vectors() {
        let g = make_gen(Algorithm::SHA1, 8, RFC_SECRET_20);
        let cases = [
            (59u64, "94287082"),
            (1111111109, "07081804"),
            (1111111111, "14050471"),
            (1234567890, "89005924"),
            (2000000000, "69279037"),
            (20000000000, "65353130"),
        ];
        for (t, code) in cases {
            assert_eq!(g.generate_at(t), code, "SHA1 T={t}");
        }
    }

    #[test]
    fn rfc6238_sha256_vectors() {
        let g = make_gen(Algorithm::SHA256, 8, RFC_SECRET_32);
        let cases = [
            (59u64, "46119246"),
            (1111111109, "68084774"),
            (1111111111, "67062674"),
            (1234567890, "91819424"),
            (2000000000, "90698825"),
            (20000000000, "77737706"),
        ];
        for (t, code) in cases {
            assert_eq!(g.generate_at(t), code, "SHA256 T={t}");
        }
    }

    #[test]
    fn rfc6238_sha512_vectors() {
        let g = make_gen(Algorithm::SHA512, 8, RFC_SECRET_64);
        let cases = [
            (59u64, "90693936"),
            (1111111109, "25091201"),
            (1111111111, "99943326"),
            (1234567890, "93441116"),
            (2000000000, "38618901"),
            (20000000000, "47863826"),
        ];
        for (t, code) in cases {
            assert_eq!(g.generate_at(t), code, "SHA512 T={t}");
        }
    }

    #[test]
    fn window_bumps_when_expiry_imminent() {
        let g = make_gen(Algorithm::SHA1, 6, RFC_SECRET_20);
        // 15s left in the [30, 60) window: current window's code.
        let (code, remaining) = g.window_at(45);
        assert_eq!(code, g.generate_at(45));
        assert_eq!(remaining, Duration::from_secs(15));

        // 2s left (< WINDOW_BUMP_SECS): next window's code, full validity.
        let (code, remaining) = g.window_at(58);
        assert_eq!(code, g.generate_at(60));
        assert_eq!(remaining, Duration::from_secs(32));

        // Exactly at the boundary: 3s left is still the current window.
        let (code, remaining) = g.window_at(57);
        assert_eq!(code, g.generate_at(57));
        assert_eq!(remaining, Duration::from_secs(3));
    }

    #[test]
    fn current_matches_window_at_real_time() {
        let g = TotpGenerator::from_base32("JBSWY3DPEHPK3PXP").unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (code, remaining) = g.current().unwrap();
        assert_eq!(code.len(), 6);
        let (expected, expected_remaining) = g.window_at(now);
        assert_eq!(code, expected);
        assert!(remaining <= expected_remaining + Duration::from_secs(1));
    }

    #[test]
    fn from_base32_defaults_sha1_6_30() {
        let g = TotpGenerator::from_base32("jbswy3dpehpk3pxp").unwrap();
        assert_eq!(g.algorithm_name(), "SHA1");
        assert_eq!(g.digits(), 6);
        assert_eq!(g.period(), 30);
        assert_eq!(g.issuer(), None);
        // Normalized to canonical base32.
        assert_eq!(g.secret_base32(), "JBSWY3DPEHPK3PXP");
    }

    #[test]
    fn from_base32_rejects_garbage() {
        assert!(TotpGenerator::from_base32("not!base32!").is_err());
        assert!(TotpGenerator::from_base32("").is_err());
        assert!(TotpGenerator::from_base32("   ").is_err());
    }

    #[test]
    fn otpauth_parses_and_normalizes() {
        let g = TotpGenerator::from_otpauth(
            "otpauth://totp/Cloudflare:user@example.com?secret=JBSWY3DPEHPK3PXP&issuer=Cloudflare",
        )
        .unwrap();
        assert_eq!(g.issuer(), Some("Cloudflare"));
        assert_eq!(g.algorithm_name(), "SHA1");
        assert_eq!(g.digits(), 6);
        assert_eq!(g.period(), 30);
    }

    #[test]
    fn otpauth_params_override_defaults() {
        let g = TotpGenerator::from_otpauth(
            "otpauth://totp/Github:constantoine%40github.com?secret=JBSWY3DPEHPK3PXP&issuer=Github&algorithm=SHA256&digits=8&period=60",
        )
        .unwrap();
        assert_eq!(g.issuer(), Some("Github"));
        assert_eq!(g.algorithm_name(), "SHA256");
        assert_eq!(g.digits(), 8);
        assert_eq!(g.period(), 60);
    }

    #[test]
    fn otpauth_issuer_from_label_when_param_missing() {
        let g = TotpGenerator::from_otpauth(
            "otpauth://totp/Acme:alice@example.com?secret=JBSWY3DPEHPK3PXP",
        )
        .unwrap();
        assert_eq!(g.issuer(), Some("Acme"));
    }

    #[test]
    fn otpauth_rejects_bad_uris() {
        // Missing secret.
        assert!(TotpGenerator::from_otpauth("otpauth://totp/Alice?issuer=Acme").is_err());
        // Wrong scheme.
        assert!(TotpGenerator::from_otpauth("https://totp/alice?secret=JBSWY3DPEHPK3PXP").is_err());
        // Invalid base32 secret.
        assert!(TotpGenerator::from_otpauth("otpauth://totp/alice?secret=!!!").is_err());
        // Empty input.
        assert!(TotpGenerator::from_otpauth("").is_err());
    }

    #[test]
    fn otpauth_uri_roundtrip_keeps_config() {
        let uri = "otpauth://totp/Acme:alice@example.com?secret=JBSWY3DPEHPK3PXP&issuer=Acme&algorithm=SHA256&digits=8&period=60";
        let g = TotpGenerator::from_otpauth(uri).unwrap();
        let regenerated = TotpGenerator::from_otpauth(&g.inner.to_url().unwrap()).unwrap();
        assert_eq!(regenerated.algorithm_name(), g.algorithm_name());
        assert_eq!(regenerated.digits(), g.digits());
        assert_eq!(regenerated.period(), g.period());
        assert_eq!(regenerated.issuer(), g.issuer());
        assert_eq!(regenerated.generate_at(59), g.generate_at(59));
    }

    #[test]
    fn debug_hides_secret() {
        let g = TotpGenerator::from_otpauth(
            "otpauth://totp/Acme:alice@example.com?secret=JBSWY3DPEHPK3PXP",
        )
        .unwrap();
        let dbg = format!("{g:?}");
        assert!(!dbg.contains("JBSWY3DPEHPK3PXP"), "{dbg}");
    }
}
