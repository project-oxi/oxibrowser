//! Browser configuration.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::PathBuf;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Serde helpers for Duration ↔ seconds
// ---------------------------------------------------------------------------

mod duration_secs {
    use super::{Deserialize, Deserializer, Duration, Serializer};

    pub fn serialize<S: Serializer>(dur: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(dur.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let secs = u64::deserialize(d)?;
        Ok(Duration::from_secs(secs))
    }
}

// ---------------------------------------------------------------------------
// Default functions for serde(default = "...")
// ---------------------------------------------------------------------------

fn default_user_agent() -> String {
    // Chrome 149 macOS — must match the wreq Emulation::Chrome149 profile
    // (sec-ch-ua v=149, sec-ch-ua-platform "macOS") so transport and JS
    // navigator.userAgent agree. See network/client.rs emulation().
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36".to_string()
}

fn default_timeout_secs() -> Duration {
    Duration::from_secs(30)
}

fn default_true() -> bool {
    true
}

fn default_max_sessions() -> usize {
    10
}

fn default_viewport_width() -> u32 {
    1280
}

fn default_viewport_height() -> u32 {
    720
}

fn default_connection_pool_size() -> usize {
    10
}

fn default_js_timeout_ms() -> u64 {
    5000
}

fn default_js_max_recursion() -> usize {
    100
}

fn default_js_max_loop_iterations() -> u64 {
    100_000
}

fn default_js_max_stack_size() -> usize {
    4_096
}

fn default_nav_script_timeout_ms() -> u64 {
    30_000
}

fn default_nav_script_max_recursion() -> usize {
    4_096
}

fn default_nav_script_max_loop_iterations() -> u64 {
    500_000_000
}

fn default_nav_script_max_stack_size() -> usize {
    65_536
}

fn default_navigation_timeout_ms() -> u64 {
    30_000
}

fn default_max_response_body() -> usize {
    10 * 1024 * 1024
}
// ---------------------------------------------------------------------------
// BrowserConfig
// ---------------------------------------------------------------------------

/// Configuration for a Browser instance.
///
/// Supports `Serialize`/`Deserialize` so Agent OS can embed it directly
/// in a TOML config file under `[browser.engine]`.
///
/// # TOML example
///
/// ```toml
/// [browser.engine]
/// user_agent = "MyBot/1.0"
/// obey_robots = false
/// js_timeout_ms = 10000
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserConfig {
    /// User-Agent string sent with requests.
    #[serde(default = "default_user_agent")]
    pub user_agent: String,

    /// Default page navigation timeout (in seconds for serialization).
    #[serde(default = "default_timeout_secs", with = "duration_secs")]
    pub default_timeout: Duration,

    /// Whether to obey robots.txt.
    #[serde(default = "default_true")]
    pub obey_robots: bool,

    /// Maximum number of concurrent sessions.
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,

    /// Viewport width for rendering (0 = no rendering).
    #[serde(default = "default_viewport_width")]
    pub viewport_width: u32,

    /// Viewport height for rendering (0 = no rendering).
    #[serde(default = "default_viewport_height")]
    pub viewport_height: u32,

    /// Enable offscreen rendering (requires servo feature).
    #[serde(default)]
    pub enable_rendering: bool,

    /// HTTP connection pool size.
    #[serde(default = "default_connection_pool_size")]
    pub connection_pool_size: usize,

    /// Accept invalid TLS certificates.
    #[serde(default)]
    pub accept_invalid_certs: bool,

    /// HTTP/HTTPS/SOCKS proxy URL (e.g. `http://host:port`, `socks5://host:port`,
    /// `socks5h://host:port` for remote DNS). `None` = direct connection.
    #[serde(default)]
    pub proxy: Option<String>,

    /// Credentials sent automatically on a `401` challenge with
    /// `WWW-Authenticate: Basic` or `Digest`. Applied to all requests when set
    /// (per-origin credential scoping is out of scope for headless automation).
    #[serde(default)]
    pub http_username: Option<String>,
    /// Password paired with [`BrowserConfig::http_username`].
    #[serde(default)]
    pub http_password: Option<String>,

    /// Enable SSRF protection (IP filter for private/internal IPs).
    /// Defaults to `true`. Set to `false` for testing or when CDP clients
    /// need to navigate to local services.
    #[serde(default = "default_true")]
    pub enable_ssrf_filter: bool,

    /// JS execution timeout in milliseconds.
    /// A single `evaluate()` call that runs longer than this will be aborted
    /// and the JS context will be reset.
    #[serde(default = "default_js_timeout_ms")]
    pub js_timeout_ms: u64,

    /// Maximum JS recursion depth (function call stack depth).
    /// Prevents infinite recursion like `function f() { f(); }`.
    #[serde(default = "default_js_max_recursion")]
    pub js_max_recursion: usize,

    /// Maximum JS loop iteration count.
    /// Prevents infinite loops like `while(true){}`.
    /// Set to `u64::MAX` for no limit.
    #[serde(default = "default_js_max_loop_iterations")]
    pub js_max_loop_iterations: u64,

    /// Maximum JS operand stack size.
    #[serde(default = "default_js_max_stack_size")]
    pub js_max_stack_size: usize,

    /// Timeout (ms) for the navigation script-execution phase — covers all
    /// page `<script>` tags plus the post-load settle pump cumulatively.
    /// Separate from `js_timeout_ms` (agent one-shot evals): real SPA bundles
    /// need far more wall time. Default 30 s.
    #[serde(default = "default_nav_script_timeout_ms")]
    pub nav_script_timeout_ms: u64,

    /// Max recursion depth for navigation script execution. Higher than
    /// `js_max_recursion` to allow framework-scale call depth without
    /// tripping. Default 4_096.
    #[serde(default = "default_nav_script_max_recursion")]
    pub nav_script_max_recursion: usize,

    /// Max loop iterations for navigation script execution. ~5_000x the
    /// `js_max_loop_iterations` cap so real SPA bundles (millions of init
    /// iterations, more under JIT-less boa) are not silently skipped.
    /// Default 500_000_000.
    #[serde(default = "default_nav_script_max_loop_iterations")]
    pub nav_script_max_loop_iterations: u64,

    /// Max operand stack size for navigation script execution. Default 16_384.
    #[serde(default = "default_nav_script_max_stack_size")]
    pub nav_script_max_stack_size: usize,

    /// Navigation timeout in milliseconds (time to wait for page load).
    #[serde(default = "default_navigation_timeout_ms")]
    pub navigation_timeout_ms: u64,

    /// Cookie jar persistence file path. `None` = in-memory only (default).
    #[serde(default)]
    pub cookie_file: Option<PathBuf>,

    /// Dispose of the in-memory cookie jar when the browser closes
    /// (default `true`). `cookie_file` persistence still happens before the
    /// clear — this only prevents in-memory session remnants from outliving
    /// the browser's lifetime in long-running hosts.
    #[serde(default = "default_true")]
    pub clear_cookies_on_close: bool,

    /// Maximum HTTP response body size in bytes (default 10 MiB).
    /// Oversized JS fetch responses fail instead of exposing corrupted bytes.
    /// Document/navigation text and convenience reads are truncated at the
    /// limit with a warning.
    #[serde(default = "default_max_response_body")]
    pub max_response_body_bytes: usize,

    /// Directory for saving downloads (`Content-Disposition: attachment`
    /// responses). `None` = the platform temp dir resolved at save time via
    /// [`BrowserConfig::download_dir_or_default`] (the directory itself is
    /// created lazily when the first download is saved).
    #[serde(default)]
    pub download_dir: Option<PathBuf>,

    /// 미지원/폴리필 Web API 접근을 카운트해 리포트한다. 활성화하면 JS
    /// 스레드의 `window` 객체가 카운팅 `Proxy`로 래핑되고, 대상 객체에
    /// 존재하지 않는 프로퍼티(≈ 미구현 Web API) 접근이 `OXI.getApiGaps`와
    /// fetch `--json` `meta.api_gaps`로 집계된다. 기본값 `false`.
    #[serde(default)]
    pub telemetry: bool,

    /// Keep timers, network callbacks and animation frames progressing between
    /// host commands. Opt-in for long-running interactive pages; default false.
    #[serde(default)]
    pub background_event_loop: bool,

    /// Allow WebGL2 context creation. Disable for applications that need a
    /// consistent WebGL1 fallback with the current partial WebGL2 surface.
    #[serde(default = "default_true")]
    pub webgl2_enabled: bool,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            user_agent: default_user_agent(),
            default_timeout: default_timeout_secs(),
            proxy: None,
            http_username: None,
            http_password: None,
            obey_robots: default_true(),
            max_sessions: default_max_sessions(),
            viewport_width: default_viewport_width(),
            viewport_height: default_viewport_height(),
            enable_rendering: false,
            connection_pool_size: default_connection_pool_size(),
            accept_invalid_certs: false,
            enable_ssrf_filter: default_true(),
            js_timeout_ms: default_js_timeout_ms(),
            js_max_recursion: default_js_max_recursion(),
            js_max_loop_iterations: default_js_max_loop_iterations(),
            js_max_stack_size: default_js_max_stack_size(),
            nav_script_timeout_ms: default_nav_script_timeout_ms(),
            nav_script_max_recursion: default_nav_script_max_recursion(),
            nav_script_max_loop_iterations: default_nav_script_max_loop_iterations(),
            nav_script_max_stack_size: default_nav_script_max_stack_size(),
            navigation_timeout_ms: default_navigation_timeout_ms(),
            cookie_file: None,
            clear_cookies_on_close: true,
            max_response_body_bytes: default_max_response_body(),
            download_dir: None,
            telemetry: false,
            background_event_loop: false,
            webgl2_enabled: true,
        }
    }
}

impl BrowserConfig {
    /// Resolve the download directory for `Content-Disposition: attachment`
    /// saves: the configured [`BrowserConfig::download_dir`], or the platform
    /// temp dir's `oxibrowser-downloads` folder. The directory itself is
    /// created lazily at save time (see `Session::handle_download`), not here.
    pub fn download_dir_or_default(&self) -> PathBuf {
        self.download_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("oxibrowser-downloads"))
    }

    /// Create a minimal config with no rendering.
    pub fn headless() -> Self {
        Self {
            enable_rendering: false,
            ..Self::default()
        }
    }

    /// Create a config optimized for automation.
    pub fn automation() -> Self {
        Self {
            obey_robots: false,
            default_timeout: Duration::from_secs(60),
            connection_pool_size: 20,
            js_timeout_ms: 10_000,
            ..Self::default()
        }
    }

    /// Return a fluent [`BrowserConfigBuilder`] seeded with
    /// `BrowserConfig::default()`.
    pub fn builder() -> BrowserConfigBuilder {
        BrowserConfigBuilder::new()
    }
}
// ---------------------------------------------------------------------------
// BrowserConfigBuilder
// ---------------------------------------------------------------------------

/// Fluent builder for [`BrowserConfig`].
///
/// Each method returns `Self` so calls can be chained; `build()` consumes
/// the builder and yields a fully-formed [`BrowserConfig`]. All fields
/// start at `BrowserConfig::default()` values.
///
/// # Example
///
/// ```
/// use oxibrowser_core::BrowserConfig;
/// let cfg = BrowserConfig::builder()
///     .user_agent("MyBot/1.0")
///     .viewport(1920, 1080)
///     .max_sessions(4)
///     .ssrf_filter(false)
///     .build();
/// ```
#[derive(Debug, Clone, Default)]
pub struct BrowserConfigBuilder {
    inner: BrowserConfig,
}

impl BrowserConfigBuilder {
    /// Create a new builder seeded with `BrowserConfig::default()`.
    pub fn new() -> Self {
        Self {
            inner: BrowserConfig::default(),
        }
    }

    /// Override the User-Agent string.
    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.inner.user_agent = ua.into();
        self
    }

    /// Override the viewport dimensions (width, height).
    pub fn viewport(mut self, width: u32, height: u32) -> Self {
        self.inner.viewport_width = width;
        self.inner.viewport_height = height;
        self
    }

    /// Override the default page-navigation timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.inner.default_timeout = timeout;
        self
    }

    /// Override the JavaScript execution timeout (milliseconds).
    pub fn js_timeout(mut self, ms: u64) -> Self {
        self.inner.js_timeout_ms = ms;
        self
    }

    /// Enable or disable the SSRF filter.
    pub fn ssrf_filter(mut self, enabled: bool) -> Self {
        self.inner.enable_ssrf_filter = enabled;
        self
    }

    /// Set HTTP authentication credentials (basic/digest) applied on 401 challenges.
    pub fn http_auth(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.inner.http_username = Some(username.into());
        self.inner.http_password = Some(password.into());
        self
    }

    /// Set an HTTP/HTTPS/SOCKS proxy URL.
    pub fn proxy(mut self, proxy: impl Into<String>) -> Self {
        self.inner.proxy = Some(proxy.into());
        self
    }

    /// Override the maximum number of concurrent sessions.
    pub fn max_sessions(mut self, max: usize) -> Self {
        self.inner.max_sessions = max;
        self
    }

    /// Enable or disable Web API gap telemetry ([`BrowserConfig::telemetry`]).
    pub fn telemetry(mut self, enabled: bool) -> Self {
        self.inner.telemetry = enabled;
        self
    }

    /// Dispose of the in-memory cookie jar on browser close
    /// ([`BrowserConfig::clear_cookies_on_close`]).
    pub fn cookies_on_close(mut self, enabled: bool) -> Self {
        self.inner.clear_cookies_on_close = enabled;
        self
    }

    /// Consume the builder and return the configured [`BrowserConfig`].
    pub fn build(self) -> BrowserConfig {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = BrowserConfig::default();
        assert!(config.user_agent.contains("Chrome/149"));
        assert_eq!(config.default_timeout, Duration::from_secs(30));
        assert!(config.obey_robots, "default should obey robots.txt");
        assert_eq!(config.max_sessions, 10);
        assert_eq!(config.viewport_width, 1280);
        assert_eq!(config.viewport_height, 720);
        assert!(!config.accept_invalid_certs);
        assert_eq!(config.js_timeout_ms, 5000);
        assert_eq!(config.js_max_recursion, 100);
        assert_eq!(config.js_max_loop_iterations, 100_000);
        assert_eq!(config.js_max_stack_size, 4_096);
        assert_eq!(config.navigation_timeout_ms, 30_000);
        assert!(
            config.cookie_file.is_none(),
            "default should have no cookie file"
        );
    }

    #[test]
    fn test_headless_config() {
        let config = BrowserConfig::headless();
        assert!(
            !config.enable_rendering,
            "headless should disable rendering"
        );
        assert_eq!(
            config.viewport_width, 1280,
            "headless should have default viewport width"
        );
        assert_eq!(
            config.viewport_height, 720,
            "headless should have default viewport height"
        );
    }

    #[test]
    fn test_automation_config() {
        let config = BrowserConfig::automation();
        assert!(!config.obey_robots, "automation should ignore robots.txt");
        assert_eq!(
            config.default_timeout,
            Duration::from_secs(60),
            "automation should have longer timeout"
        );
        assert_eq!(config.connection_pool_size, 20);
    }

    #[test]
    fn test_config_serde_roundtrip() {
        let config = BrowserConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let config2: BrowserConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config2.user_agent, config.user_agent);
        assert_eq!(config2.default_timeout, config.default_timeout);
        assert_eq!(config2.obey_robots, config.obey_robots);
        assert_eq!(config2.max_sessions, config.max_sessions);
        assert_eq!(config2.js_timeout_ms, config.js_timeout_ms);
        assert_eq!(config2.cookie_file, config.cookie_file);
    }

    #[test]
    fn test_config_partial_deserialize() {
        // Only override a few fields — the rest should be defaults
        let json = r#"{"obey_robots": false, "js_timeout_ms": 9999}"#;
        let config: BrowserConfig = serde_json::from_str(json).unwrap();
        assert!(!config.obey_robots);
        assert_eq!(config.js_timeout_ms, 9999);
        // Defaults preserved
        assert!(config.user_agent.contains("Chrome/149"));
        assert_eq!(config.max_sessions, 10);
    }

    #[test]
    fn test_config_empty_object_gives_defaults() {
        let json = "{}";
        let config: BrowserConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.user_agent, default_user_agent());
        assert_eq!(config.default_timeout, default_timeout_secs());
        assert!(config.obey_robots);
        assert_eq!(config.max_sessions, default_max_sessions());
    }

    #[test]
    fn test_builder_produces_default_when_empty() {
        // Builder with no calls must produce a config equivalent to
        // BrowserConfig::default().
        let from_builder = BrowserConfigBuilder::new().build();
        let from_default = BrowserConfig::default();
        assert_eq!(from_builder.user_agent, from_default.user_agent);
        assert_eq!(from_builder.default_timeout, from_default.default_timeout);
        assert_eq!(from_builder.max_sessions, from_default.max_sessions);
        assert_eq!(from_builder.viewport_width, from_default.viewport_width);
        assert_eq!(from_builder.viewport_height, from_default.viewport_height);
        assert_eq!(from_builder.js_timeout_ms, from_default.js_timeout_ms);
        assert_eq!(
            from_builder.enable_ssrf_filter,
            from_default.enable_ssrf_filter
        );
        assert_eq!(
            from_builder.cookie_file, from_default.cookie_file,
            "cookie_file should default to None"
        );
    }

    #[test]
    fn test_builder_method_on_browser_config() {
        // BrowserConfig::builder() must work too — equivalent to
        // BrowserConfigBuilder::new().
        let cfg = BrowserConfig::builder().build();
        assert_eq!(cfg.user_agent, BrowserConfig::default().user_agent);
    }

    #[test]
    fn test_builder_user_agent() {
        let cfg = BrowserConfig::builder().user_agent("MyBot/1.0").build();
        assert_eq!(cfg.user_agent, "MyBot/1.0");
    }

    #[test]
    fn test_builder_viewport() {
        let cfg = BrowserConfig::builder().viewport(1920, 1080).build();
        assert_eq!(cfg.viewport_width, 1920);
        assert_eq!(cfg.viewport_height, 1080);
    }

    #[test]
    fn test_builder_timeout() {
        let cfg = BrowserConfig::builder()
            .timeout(Duration::from_secs(45))
            .build();
        assert_eq!(cfg.default_timeout, Duration::from_secs(45));
    }

    #[test]
    fn test_builder_js_timeout() {
        let cfg = BrowserConfig::builder().js_timeout(12_345).build();
        assert_eq!(cfg.js_timeout_ms, 12_345);
    }

    #[test]
    fn test_builder_ssrf_filter() {
        let enabled = BrowserConfig::builder().ssrf_filter(true).build();
        let disabled = BrowserConfig::builder().ssrf_filter(false).build();
        assert!(enabled.enable_ssrf_filter);
        assert!(!disabled.enable_ssrf_filter);
    }

    #[test]
    fn test_builder_max_sessions() {
        let cfg = BrowserConfig::builder().max_sessions(3).build();
        assert_eq!(cfg.max_sessions, 3);
    }

    #[test]
    fn test_download_dir_default_resolution() {
        // Unset → platform temp dir default (directory created at save time).
        let cfg = BrowserConfig::default();
        assert!(cfg.download_dir.is_none());
        assert_eq!(
            cfg.download_dir_or_default(),
            std::env::temp_dir().join("oxibrowser-downloads")
        );

        // Explicit config wins.
        let custom = PathBuf::from("/tmp/oxi-custom-dl");
        let cfg = BrowserConfig {
            download_dir: Some(custom.clone()),
            ..BrowserConfig::default()
        };
        assert_eq!(cfg.download_dir_or_default(), custom);

        // Serializes/deserializes through the TOML/JSON config surface.
        let parsed: BrowserConfig =
            serde_json::from_str(r#"{"download_dir":"/tmp/oxi-from-json"}"#).unwrap();
        assert_eq!(
            parsed.download_dir,
            Some(PathBuf::from("/tmp/oxi-from-json"))
        );
        let parsed: BrowserConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.download_dir, None, "missing key defaults to None");
    }

    #[test]
    fn test_builder_chains_and_preserves_other_defaults() {
        // Multi-field chain — confirming non-overridden fields stay at defaults.
        let cfg = BrowserConfig::builder()
            .user_agent("X")
            .max_sessions(7)
            .ssrf_filter(false)
            .build();
        assert_eq!(cfg.user_agent, "X");
        assert_eq!(cfg.max_sessions, 7);
        assert!(!cfg.enable_ssrf_filter);
        // Defaults preserved on untouched fields.
        assert_eq!(cfg.viewport_width, default_viewport_width());
        assert_eq!(cfg.default_timeout, default_timeout_secs());
    }

    #[test]
    fn test_proxy_config() {
        // Default: no proxy.
        assert!(BrowserConfig::default().proxy.is_none());

        // Builder sets the proxy URL.
        let cfg = BrowserConfig::builder()
            .proxy("socks5h://127.0.0.1:1080")
            .build();
        assert_eq!(cfg.proxy.as_deref(), Some("socks5h://127.0.0.1:1080"));

        // Serde round-trip preserves proxy.
        let json = serde_json::to_string(&cfg).unwrap();
        let loaded: BrowserConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.proxy, cfg.proxy);
    }
}
