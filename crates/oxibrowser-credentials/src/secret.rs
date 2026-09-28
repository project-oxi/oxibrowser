//! `SecretBox` — the only container a secret value may travel in
//! (lower design §4.3 `secret.rs`).

use oxibrowser_core::security::audit::secret_fingerprint;
use zeroize::Zeroizing;

/// A byte secret wiped on drop.
///
/// `Debug`, `Display`, and `Serialize` are deliberately **not** implemented —
/// the value cannot leak through formatting or serialization paths at compile
/// time. Reading the value requires an explicit [`SecretBox::expose`] call,
/// which exists only for the broker's injection path.
///
/// The value cannot leak through `Debug` — the impl does not exist:
///
/// ```compile_fail
/// use oxibrowser_credentials::SecretBox;
/// let s = SecretBox::from_string("hunter2".to_string());
/// fn assert_debug<T: std::fmt::Debug>(_: &T) {}
/// assert_debug(&s);
/// ```
///
/// Nor through `Display`:
///
/// ```compile_fail
/// use oxibrowser_credentials::SecretBox;
/// let s = SecretBox::from_string("hunter2".to_string());
/// fn assert_display<T: std::fmt::Display>(_: &T) {}
/// assert_display(&s);
/// ```
///
/// Nor through `serde_json`:
///
/// ```compile_fail
/// use oxibrowser_credentials::SecretBox;
/// let s = SecretBox::from_string("hunter2".to_string());
/// let _ = serde_json::to_string(&s).unwrap();
/// ```
pub struct SecretBox {
    inner: Zeroizing<Vec<u8>>,
}

impl SecretBox {
    /// Take ownership of a raw secret buffer.
    pub fn from_vec(v: Vec<u8>) -> Self {
        Self {
            inner: Zeroizing::new(v),
        }
    }

    /// Take ownership of a UTF-8 secret (passwords, `otpauth://` URIs).
    pub fn from_string(s: String) -> Self {
        Self::from_vec(s.into_bytes())
    }

    /// Broker-internal injection path — the only read accessor.
    pub fn expose(&self) -> &[u8] {
        &self.inner
    }

    /// UTF-8 view for record embedding; errors when the value is not UTF-8.
    pub fn expose_str(&self) -> Result<&str, crate::error::CredError> {
        std::str::from_utf8(&self.inner)
            .map_err(|e| crate::error::CredError::Invalid(format!("secret is not utf-8: {e}")))
    }

    /// Log-safe fingerprint: reuses the core audit helper so every surface
    /// formats a secret reference identically (`"sha256:"` + 8 hex chars).
    pub fn fingerprint(&self) -> String {
        secret_fingerprint(&self.inner)
    }
}

impl From<String> for SecretBox {
    fn from(s: String) -> Self {
        Self::from_string(s)
    }
}

impl From<Vec<u8>> for SecretBox {
    fn from(v: Vec<u8>) -> Self {
        Self::from_vec(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expose_and_fingerprint() {
        let s = SecretBox::from_string("hunter2".to_string());
        assert_eq!(s.expose(), b"hunter2");
        assert_eq!(s.expose_str().unwrap(), "hunter2");
        // SHA-256("hunter2") = f52fbd32… → first 8 hex chars.
        assert_eq!(s.fingerprint(), "sha256:f52fbd32");
    }

    #[test]
    fn fingerprint_is_fixed_shape() {
        let s = SecretBox::from_vec(vec![0u8; 32]);
        let fp = s.fingerprint();
        assert!(fp.starts_with("sha256:"), "{fp}");
        assert_eq!(fp["sha256:".len()..].len(), 8);
        assert!(
            fp["sha256:".len()..]
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
    }

    #[test]
    fn expose_str_rejects_non_utf8() {
        let s = SecretBox::from_vec(vec![0xff, 0xfe]);
        assert!(s.expose_str().is_err());
    }

    #[test]
    fn debug_display_serialize_are_not_implemented() {
        // The compile_fail doctests on SecretBox are the executable proof.
    }
}
