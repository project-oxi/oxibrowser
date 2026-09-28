# 에이전트 무인 인증 — 구현 설계 (2026-09-27)

> 출처: `docs/research/00-SYNTHESIS.md` 및 심층 편 01/02/04/06. 연구는 방향성 수준이며, 본 문서는
> 모듈 경로·타입 시그니처·저장 포맷·CLI/REPL/CDP 표면·P0 함수 단위 계획으로 구체화한다.
>
> **상태: 부분 구현됨 (2026-09-27).** M0.1–M0.4(P0)와 M1(origin_policy)은 구현·검증 완료 —
> 상태 표는 §8 하단. M2 이상은 미구현. 코드 참조는 v0.22.0 기준 2026-09-27 재검증 결과이며,
> 연구 인용과의 드리프트는 §1과 부록 A에 정리했다.

---

## 목차

1. [연구 인용 재검증 — 드리프트 요약](#1-연구-인용-재검증--드리프트-요약)
2. [범위와 명시적 비목표](#2-범위와-명시적-비목표)
3. [아키텍처](#3-아키텍처)
4. [모듈·크레이트 경로와 타입 시그니처](#4-모듈크레이트-경로와-타입-시그니처)
5. [저장 포맷 스키마](#5-저장-포맷-스키마)
6. [표면: CLI · session REPL · CDP OXI](#6-표면-cli--session-repl--cdp-oxi)
7. [P0 구현 계획 (함수 단위 + 테스트)](#7-p0-구현-계획-함수-단위--테스트)
8. [P1/P2 마일스톤 — PR 분할](#8-p1p2-마일스톤--pr-분할)
9. [실패 모드](#9-실패-모드)
10. [의존성 (crates.io 검증)](#10-의존성-cratesio-검증)

---

## 1. 연구 인용 재검증 — 드리프트 요약

연구가 인용한 코드 위치를 현재 코드로 전수 재확인했다. 전체 표는 부록 A. 설계에 영향을 주는
드리프트만 요약한다.

| # | 연구 주장 | 재검증 결과 | 설계 영향 |
|---|---|---|---|
| D1 | `session.rs` L39-64 `RequestRecord`(평문 헤더/바디 보관) | **정확** — L39 구조체, `request_headers` L52, `post_body` L54, `response_headers` L60 | P0-1 그대로 유효 |
| D2 | HAR 쓰기 경로 = "session.rs network_log → main.rs write_har" | **정정** — 실제 직렬화는 `network/har.rs::to_har_json()` (L37). `write_har` (main.rs L576)은 파일 쓰기만 | 리랙션 삽입점은 har.rs 내부 (§7 P0-1) |
| D3 | — (연구 미언급) | **신규 사실** — HAR `response.content`는 size/mime만 기록, **바디 미수록** (har.rs L105-108). `queryString`은 항상 `[]` (L88) → URL 쿼리 민감값은 `url` 필드로만 유출 | 리랙션 범위: 헤더 + URL 쿼리 + `post_body`. 응답 바디는 범위 밖 (애초 미수록) |
| D4 | CDP `Network.requestWillBeSent` 헤더도 필터 필요 | **정정** — 현재 CDP 네트워크 이벤트는 헤더를 **빈 객체로 방출** (`"headers": {}`, network.rs L365). 단 `url`/`documentURL`은 원문 그대로 | CDP 쪽 실제 리랙션 대상은 URL 쿼리. 헤더 필터는 향후 변경 대비 불변식으로만 문서화 |
| D5 | `cookie_file` 디스크 지속 "기본 끔" 필요 | **이미 충족** — `config.rs` L222-224 기본 `None` (테스트 L431-434 명시). 단 `Browser::close`는 jar를 clear하지 않고 파일 있으면 저장만 함 (browser.rs L225-239) | P0-3 범위 축소: 남은 것은 teardown clear + credential 모드 가드 |
| D6 | `challenge.rs` — Cloudflare/DataDome/PerimeterX 분류 | **드리프트** — `AkamaiImperva` 벤더 추가 (L207-213, clearance `incap_ses`/`ak_bmsc`), `ChallengeKind`에 `JsCheck`/`Unknown` 존재 (L54-65) | 상승 연결 대상 벤더 4종. Interactive/Blocked 중단 조건은 여전 유효 (client.rs L593-601) |
| D7 | session REPL "22 명령" | **드리프트** — `Command` enum 25 variants = 기능 명령 23 + Help/Exit (parser.rs L21-112) | 새 명령 추가 시 25 기준 |
| D8 | `js/runtime.rs:2245-2248` 씨드 주석 | **드리프트** — `set_page_url_with_storage_seed` L1637 (파일 16,180줄). "기존 키 보존 + 씨드 키 덮어쓰기, 다음 문서 주입" 의미 불변 | 단일 flat 맵 제한은 여전 — 다중 오리진은 P1-M6 |
| D9 | `session.rs:2260` export 단일 오리진 한정 | **유효** — L2266-2280 주석에 한계 명시. `import_state`도 flat 맵 병합 (L2303+) | P1-M6 범위 그대로 |
| D10 | WebAuthn 전무 | **재확인** — `crates/` 전체 `webauthn|navigator\.credentials` 무일치 | P2-M7 신규 구현 |
| D11 | DomSnapshot password 마스킹 부재 | **재확인 + 유출 경로 특정** — `accessible_name`이 `value` 속성으로 fallback (dom_snapshot.rs L2013-2018), `serialize_node`가 value 속성을 그대로 직렬화 (L1628-1634) | P0-2 정확한 차단 지점 3곳 확정 (§7 P0-2) |
| D12 | `cookie.rs` L677 `CookieJar::clear()` 존재 | **정확** — L677-679 | P0-3에서 그대로 사용 |
| D13 | `mcp.rs`·`skills/`(install, webfetch) 존재, auth 스킬 없음 | **정확** — `skills/oxibrowser-install`, `skills/oxibrowser-webfetch` | P2-M9 스킬 매니페스트 연계 |
| D14 | — (AGENTS.md 자체) | CDP 도메인 파일 13개(mod 제외 12: oxi, network, emulation, page, dom, target, tracing, runtime, input, browser, fetch, log) — AGENTS.md의 "10 domain handlers"는 오래됨 | 본 설계와 무관하나 AGENTS.md 갱신 대상 |

연구의 보안 주장(비밀값 비노출 원칙, exact-origin 매칭, deny-우선 평가, `cf_clearance` 재생 불신뢰
등)은 코드 재검증과 무관한 외부 문서 근거이므로 본 설계가 그대로 채택하되, 실현 시 주의점은 §9
실패 모드로 분리해 문서화했다.

---

## 2. 범위와 명시적 비목표

### 2.1 범위

브라우저 안에서 자격증명을 **보관(브로커) → 정책(원본 고정·동의) → 주입(폼 필) → 기록(감사)** 하는
실행 계층을 OxiBrowser에 추가한다. API-퍼스트 경로(Cloudflare/Tailscale API 토큰 발급·호출)는
브라우저 밖 운영 절차이므로 본 설계 범위 밖이며, 브라우저는 "웹 세션으로만 되는 일"(연구 결정트리
Q2/Q3)을 담당한다.

### 2.2 명시적 비목표 (00-SYNTHESIS §6 비권고 그대로)

아래 7개는 연구 조사에서 근거가 확보된 **금지** 항목이며, 본 설계의 어느 마일스톤에서도 구현하지
않는다. 인용은 종합 문서 §6 표기의 근거 편이다.

1. `cf_clearance` 스냅샷 재생 의존 — 방문자·디바이스 결속으로 재사용 불신뢰 (02 §3.2)
2. 사용자 일상 Chrome 프로파일 attach — Chrome 136부터 원천 차단 + 최상위 위험 (02 §2.2)
3. SMS/이메일 2FA 자동화 — 구조가 피싱 릴레이와 동일 (04 §4.1, NIST restricted)
4. iCloud 기존 패스키 외부 사용 — 비밀키 비노출·연관도메인 필요로 불가 (04 §3)
5. `defaults delete MobileMeAccounts`식 iCloud 우회 — 불완전 상태 (03 §5.1)
6. LLM 프롬프트 기반 승인 요청 — 우회 가능, 실행 계층 게이트만 유효 (05 §2.3)
7. 브랜드·타이틀·유사도 기반 자격증명 매칭 — password-manager-resources가 반증 (06 §3.1)

### 2.3 설계 수준 추가 비목표

- **API 토큰 경로의 자동화 구현**(토큰 발급·API 호출 클라이언트) — 브라우저 제품 밖. 03의
  결정트리가 "브라우저 불필요"로 판정한 영역.
- **macOS GUI/TCC/iCloud 계정 조작 자동화** — 구조적으로 인간 게이트(연구 1 §1 표). 상승
  프리미티브(P1-M5 takeover)로만 접점을 둔다.
- **IndexedDB 범용 직렬화** — 02 §5 비권고. 인증 토큰이 IndexedDB인 사이트는 프로파일 재사용이
  정답이며 세션 저장소(P1-M6)에서 지원하지 않는다.
- **sessionStorage 스냅샷 (v1)** — Playwright 표준 포맷에 없음 (02 §4.2). 봉투 `version`을 올릴
  때 재평가.
- **세션 저장소의 원격 동기화/공유** — 단일 머신 로컬 전용.

---

## 3. 아키텍처

```
표면                        정책 계층                   보관 계층
──────────────             ─────────────────          ──────────────────
CLI (oxibrowser)     ─┐
session REPL/executor ─┼─▶ PolicyEngine           ─▶ CredentialProvider
CDP OXI 도메인        ─┘      │  ①deny 규칙              └─ KeyringProvider (macOS Keychain
                             │  ②OriginPolicy                / Linux Secret Service /
                             │    (exact origin,             Windows Credential Manager)
                             │     프레임 origin,        ─▶ SessionStore (AEAD 암호 파일,
                             │     리다이렉트 검사)          스코프별, 키는 키체인)
                             │  ③ConsentStore
                             │    (만료·횟수)
                             ▼
                        AuditLog (JSONL append-only, 값 대신 핸들+해시)
```

- **공통 원칙** (연구 00 §3, 06 §8): 비밀값은 브로커 내부에서만 존재하고 CDP 명령 인자·응답·로그·
  HAR로 절대 나오지 않는다. 로그에는 핸들 + SHA-256 앞 8자 핑거프린트만 남긴다.
- 새 비밀 관련 코드는 전부 `oxibrowser-credentials` 크레이트와 `oxibrowser-core`의 `security`
  모듈에 격리한다. 기존 크레이트는 표면 연결(executor/도메인/CLI 배선)만 추가한다.

---

## 4. 모듈·크레이트 경로와 타입 시그니처

### 4.1 `crates/oxibrowser-core/src/security/` (신규, P0)

리랙션과 감사는 JS/DOM·네트워크·CDP·CLI 어디에서도 필요한 말단 모듈이므로 core에 둔다. 외부
의존은 `serde_json`, `sha2`뿐이다.

```rust
// security/mod.rs
pub mod audit;
pub mod redact;
```

```rust
// security/redact.rs
/// 기본 리랙션 치환 문자열. 값 길이 정보도 숨긴다.
pub const REDACTED: &str = "__REDACTED__";

/// 리랙션 규칙 집합. 기본 프로필 + 조직 고유 헤더 추가분.
#[derive(Debug, Clone)]
pub struct RedactionProfile {
    /// 대소문자 무시 매칭되는 민감 헤더명.
    pub sensitive_headers: Vec<String>,   // 기본: §7 P0-1 목록
    /// URL 쿼리에서 값만 치환할 파라미터 키.
    pub sensitive_query_keys: Vec<String>,
    /// form-urlencoded 본문에서 값만 치환할 필드 키.
    pub sensitive_form_keys: Vec<String>,
}

impl RedactionProfile {
    pub fn default_har() -> Self;
    pub fn with_extra_headers(self, headers: impl IntoIterator<Item = String>) -> Self;
}

/// 단일 헤더가 민감 목록에 해당하는가 (ASCII 대소문자 무시).
pub fn is_sensitive_header(name: &str, profile: &RedactionProfile) -> bool;

/// 헤더 쌍 벡터를 리랙션한다. 민감 헤더는 (이름, REDACTED)로 치환 — 삭제가 아니라
/// 존재 표시를 남겨 디버그 가치를 보존한다.
pub fn redact_headers(
    headers: &[(String, String)],
    profile: &RedactionProfile,
) -> Vec<(String, String)>;

/// URL의 쿼리 문자열에서 민감 키의 값만 REDACTED로 치환한다.
/// 키 매칭은 퍼센트 디코딩 후 비교한다. 파서 실패 시 원문 반환(보수적).
pub fn redact_url_query(url: &str, profile: &RedactionProfile) -> String;

/// POST 본문 리랙션. `application/x-www-form-urlencoded`만 필드 단위 치환하고,
/// 그 외(포함 base64 인코딩 폼)는 전체 REDACTED 마커로 치환한다.
/// 반환 `None`은 "본문 없음"을 뜻한다(변경 불필요).
pub fn redact_post_body(
    body: &[u8],
    content_type: &str,
    profile: &RedactionProfile,
) -> Option<Vec<u8>>;
```

```rust
// security/audit.rs
/// 감사 이벤트 종류. P0에서 발생 지점이 연결되는 것만 포함한다.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEventKind {
    /// 자격증명 사용 판정(P1부터 실제 발생, 스키마는 P0에 확정).
    CredentialUse,
    /// 키체인/저장소 조회 자체(성공/실패).
    CredentialRead,
    /// 민감 행동(원본 export, raw HAR 쓰기 등).
    SensitiveAction,
    /// 세션 해체(쿠키 폐기 수 등).
    SessionTeardown,
    /// 정책 위반·차단(deny 매칭, origin 불일치, 캡처 차단).
    PolicyViolation,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditDecision { Allow, Deny, Prompt, Timeout }

/// 값은 절대 넣지 않는다. 비밀 참조는 CredentialRef(핸들 + 해시 8자).
#[derive(Debug, Clone, Serialize)]
pub struct AuditEvent {
    pub ts: String,            // RFC 3339 UTC, 밀리초
    pub seq: u64,              // 프로세스 내 단조 증가
    pub kind: AuditEventKind,
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

/// 자격증명의 로그 안전 참조.
#[derive(Debug, Clone, Serialize)]
pub struct CredentialRef {
    pub id: String,            // 핸들 (예: "kch:main/cloudflare.com/password/dashboard")
    pub fingerprint: String,   // "sha256:ab12cd34" — 원문 앞 8 hex
}

/// 단일 라이터 JSONL append-only 로그. 기본 경로 `~/.oxibrowser/audit.jsonl`.
/// `new`는 부모 디렉터리를 만들고 append+create로 연다. 매 이벤트 flush.
pub struct AuditLog { /* path, file, seq: AtomicU64 */ }

impl AuditLog {
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self>;
    pub fn open_default() -> std::io::Result<Self>;
    pub fn record(&self, event: AuditEvent) -> std::io::Result<()>;
}

/// 비밀 값의 로그 안전 핑거프린트: "sha256:" + hex 앞 8자.
pub fn secret_fingerprint(value: &[u8]) -> String;
```

### 4.2 `crates/oxibrowser-core/src/network/origin_policy.rs` (신규, P1-M1)

`ip_filter.rs`(SSRF 차단) 옆에 두는, 자격증명 주입 허용 오리진 강제 모듈. `psl` 크레이트(이미
워크스페이스 의존)로 등록 가능 도메인을 계산한다.

```rust
/// 스킴 + 호스트 + 실효 포트. 생성 시 소문자화·IDNA punycode·기본 포트 제거로 정규화한다.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Origin { /* scheme: Scheme, host: Host, port: u16 */ }

impl Origin {
    pub fn parse(input: &str) -> Result<Self, OriginError>;
    /// exact 비교 (정규화된 스킴+호스트+포트 일치).
    pub fn exact_eq(&self, other: &Origin) -> bool;
    /// DNS 라벨 경계 검사: self.host == domain 또는 self.host.ends_with("." + domain).
    /// 접미사 문자열 비교(notexample.com 문제)를 구조적으로 배제한다.
    pub fn host_within_registrable(&self, registrable: &str) -> bool;
}

/// 매칭 강도 계층 (06 §3.1). 자동 주입은 Exact에서만 허용된다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginMatch { Exact, RegistrableDomain, None }

/// 규칙 평가 결과. Deny가 항상 우선한다( deny → 동의 → 프롬프트 순서의deny 단계).
#[derive(Debug, Clone)]
pub enum Decision {
    Allow,
    RequireConfirmation { reason: String },
    Deny { reason: String },
}

#[derive(Debug, Clone)]
pub struct OriginRule {
    pub origin: Origin,
    pub mode: RuleMode,          // Allow | Deny
}

#[derive(Debug, Clone, Default)]
pub struct OriginPolicy { rules: Vec<OriginRule> }

impl OriginPolicy {
    pub fn from_rules(rules: Vec<OriginRule>) -> Self;
    /// 자격증명 사용 평가. `frame`은 폼이 실제로 속한 프레임의 오리진 —
    /// 최상위 페이지 오리진을 쓰면 iframe 사입 공격에 당한다(06 §3.2).
    /// 프레임 오리진을 알 수 없으면 Deny(fail-closed).
    pub fn evaluate(
        &self,
        allowed: &[Origin],     // 자격증명 레코드의 오리진 허용목록
        top_level: &Origin,
        frame: Option<&Origin>,
    ) -> Decision;
    /// 리다이렉트 판정. 자격증명 사용 중 동의 오리진 집합을 벗어나면
    /// 자격증명 세션 무효화 이벤트를 트리거한다.
    pub fn redirect_verdict(&self, allowed: &[Origin], next: &Origin) -> RedirectVerdict;
}

#[derive(Debug, Clone, Copy)]
pub enum RedirectVerdict {
    Continue,
    /// 자격증명 언로드 + 감사 `policy_violation` + 상승 재요청.
    InvalidateAndEscalate,
}
```

### 4.3 `crates/oxibrowser-credentials/` (신규 크레이트, P1-M2~M6)

의존 방향: `oxibrowser-credentials → oxibrowser-core` (단방향). `Cargo.toml` workspace members에
추가한다.

```
crates/oxibrowser-credentials/
├── src/
│   ├── lib.rs            // 공개 API 재노출
│   ├── secret.rs         // SecretBox (Zeroizing 래퍼, Debug/Serialize 미파생)
│   ├── provider.rs       // CredentialProvider 트레잇 + CredentialId/Meta/Record
│   ├── keyring.rs        // KeyringProvider (keyring 크레이트 백엔드)
│   ├── totp.rs           // TotpGenerator (otpauth:// 정규화 포함)
│   ├── consent.rs        // ConsentRecord + ConsentStore (JSONL)
│   ├── policy.rs         // PolicyEngine (deny → 동의 → 프롬프트)
│   ├── session_store.rs  // SessionEnvelope + AEAD 저장소
│   └── error.rs          // CredError
```

```rust
// secret.rs
/// Debug/Display/Serialize 미파생 — 컴파일 타임에 값 유출 경로를 차단한다.
/// Drop 시 zeroize.
pub struct SecretBox { inner: zeroize::Zeroizing<Vec<u8>> }

impl SecretBox {
    pub fn from_vec(v: Vec<u8>) -> Self;
    pub fn expose(&self) -> &[u8];          // 브로커 내부 주입 경로 전용
    pub fn fingerprint(&self) -> String;    // audit용, sha2 앞 8자
}

// provider.rs
/// 키체인 서비스 키 접두사. §5.1 컨벤션의 고정 부분.
pub const SERVICE_PREFIX: &str = "com.oxibrowser.agent";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CredentialId(pub String);   // "kch:<agent-id>/<scope>/<kind>/<slug>"

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialKind { Password, Totp, ApiKey, Note, Passkey }

/// 값 미포함 메타데이터 — 표면(CDP/REPL/CLI list)으로 나가도 안전하다.
#[derive(Debug, Clone, Serialize)]
pub struct CredentialMeta {
    pub id: CredentialId,
    pub kind: CredentialKind,
    pub agent_id: String,
    pub scope: String,                  // 등록 가능 도메인 (예: "cloudflare.com")
    pub slug: String,                   // 계정/용도 구분자
    pub allowed_origins: Vec<String>,   // 절대 오리진 목록 (§5.2)
    pub login_hint: Option<String>,     // 사용자명 힌트(비밀 아님)
    pub has_totp: bool,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

/// 키체인에 실제로 저장되는 JSON 레코드 (§5.2 스키마).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialRecord { /* CredentialMeta + 비밀 필드들 */ }

pub struct NewCredential {
    pub agent_id: String,
    pub scope: String,
    pub kind: CredentialKind,
    pub slug: String,
    pub allowed_origins: Vec<String>,
    pub login_hint: Option<String>,
    pub password: Option<SecretBox>,
    pub otpauth_uri: Option<SecretBox>,
}

/// 자격증명 저장소 백엔드. 구현: KeyringProvider (P1), 볼트 백엔드는 미래 확장.
pub trait CredentialProvider: Send + Sync {
    fn put(&self, cred: NewCredential) -> Result<CredentialId, CredError>;
    /// 값 반환이 필요한 유일한 메서드. 감사 `credential_read`를 기록한다.
    /// 정책 평가는 호출자(PolicyEngine)가 resolve 전에 끝내야 한다.
    fn resolve(&self, id: &CredentialId) -> Result<(CredentialMeta, SecretBox), CredError>;
    fn metadata(&self, id: &CredentialId) -> Result<CredentialMeta, CredError>;
    fn list(&self, agent_id: Option<&str>) -> Result<Vec<CredentialMeta>, CredError>;
    fn delete(&self, id: &CredentialId) -> Result<(), CredError>;
}

pub struct KeyringProvider { service_prefix: String }  // 기본 SERVICE_PREFIX
impl CredentialProvider for KeyringProvider { /* … */ }
```

```rust
// totp.rs
/// otpauth:// URI에서 생성기 구성을 복원한다 (issuer/algorithm/digits/period/skew).
pub struct TotpGenerator { /* … */ }

impl TotpGenerator {
    pub fn from_otpauth(uri: &str) -> Result<Self, CredError>;
    pub fn from_base32(secret: &str) -> Result<Self, CredError>;  // 기본 SHA1/6/30
    /// 현재 코드와 다음 창까지 남은 시간을 반환한다.
    /// 남은 시간 < 3초면 다음 창 코드를 반환한다(경계 만료 방지, 04 §2.2).
    pub fn current(&self) -> Result<(String, std::time::Duration), CredError>;
}

// consent.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsentRecord { /* §5.4 스키마 */ }

/// append-only JSONL. 마지막 레코드 우선(last-wins) + 폐기 톰스톤.
pub struct ConsentStore { path: PathBuf }

impl ConsentStore {
    pub fn open_default() -> std::io::Result<Self>;  // ~/.oxibrowser/consents.jsonl
    pub fn grant(&self, rec: ConsentRecord) -> std::io::Result<()>;
    pub fn revoke(&self, id: &CredentialId, origin: &str, action: &str) -> std::io::Result<()>;
    /// (credential_id, exact origin, action)에 유효한 동의를 조회한다.
    /// 만료·횟수 소진 레코드는 무시한다.
    pub fn active_for(
        &self, id: &CredentialId, origin: &str, action: &str, now: Timestamp,
    ) -> Option<ConsentRecord>;
}

// policy.rs
pub struct PolicyEngine {
    pub deny_rules: Vec<OriginRule>,
    pub consents: ConsentStore,
    pub audit: std::sync::Arc<oxibrowser_core::security::audit::AuditLog>,
}

impl PolicyEngine {
    /// deny 규칙 → 동의 캐시 → RequireConfirmation 순서 평가 (06 §2.1).
    /// 판정은 감사 로그에 `credential_use`로 기록된다.
    pub fn authorize_use(&self, req: UseRequest) -> Decision;
    /// 승인 후 실행 직전 재검증용 — 요약 재계산 값이 바뀌면 승인 무효.
    pub fn verify_confirmation(&self, token: &ConfirmationToken, req: &UseRequest) -> Decision;
}

pub struct UseRequest {
    pub credential: CredentialId,
    pub top_level: Origin,
    pub frame: Option<Origin>,
    pub action: CredentialAction,   // Login | Mfa | FillApiKey
}

pub struct ConfirmationToken { request_hash: [u8; 32], issued_at: Timestamp, expires_at: Timestamp }

// session_store.rs (P1-M6)
pub struct FingerprintMeta {
    pub user_agent: String,
    pub client_hints: ClientHintsMeta,   // platform 등 요약
    pub locale: String,
    pub timezone: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum EgressMeta { Fixed { note: Option<String> }, Unspecified }

pub struct SessionEnvelope {
    pub version: u32,                    // 1
    pub created_at: String,
    pub updated_at: String,
    pub scope: String,                   // 등록 가능 도메인
    pub fingerprint: FingerprintMeta,
    pub egress: EgressMeta,
    pub state: oxibrowser_core::storage_state::StorageState,  // Playwright 상호교환 유지
}

pub struct SessionStore { dir: PathBuf }   // 기본 ~/.oxibrowser/sessions/

impl SessionStore {
    /// AEAD 암호화 + tmp/rename 원자적 치환으로 `<scope>.session` 파일에 저장.
    pub fn save(&self, env: &SessionEnvelope) -> Result<PathBuf, CredError>;
    /// 복호화 + 지문 사전 점검. 불일치 시 `CredError::FingerprintMismatch` —
    /// 경고가 아니라 거부가 기본(fail-closed). 강제는 CLI `--fingerprint-override`.
    pub fn load(
        &self, scope: &str, current: &FingerprintMeta,
    ) -> Result<SessionEnvelope, CredError>;
    pub fn discard(&self, scope: &str) -> Result<bool, CredError>;
    pub fn list(&self) -> Result<Vec<ScopeSummary>, CredError>;
}
```

### 4.4 TOTP 의존성 세대 충돌 회피

워크스페이스는 RustCrypto **0.10 세대**(`sha1 0.10`, `md-5 0.10`)를 쓴다. DIY TOTP를
`hmac 0.13 + sha1`로 만들면 digest 0.11 세대가 새로 들어와 세대가 갈라진다. 그래서:

- **1순위**: `totp-rs 6.0.0` — 자체 의존 트리 안에서 해결, RFC 디테일(skew, window) 포함.
- **대안**: `hmac = "=0.12"` + 기존 `sha1 0.10` + `data-encoding 2.11.1`(base32) — 의존 최소
  원칙 우선 시. hmac 0.12는 digest 0.10과 짝이 되어 세대 충돌이 없다.

---

## 5. 저장 포맷 스키마

### 5.1 키체인 서비스 키 컨벤션

```
service = com.oxibrowser.agent/<agent-id>/<scope>     // scope: 등록 가능 도메인
account = <kind>/<slug>                               // kind: password|totp|api-key|note|passkey
```

- 예: `service="com.oxibrowser.agent/main/cloudflare.com"`, `account="password/dashboard"`.
- 오리진 제한은 키체인 키가 아니라 레코드의 `allowed_origins`(§5.2)가 담당한다. 키에 풀 오리진을
  넣지 않는 이유: 포트/스킴 변경 시 키 마이그레이션이 필요해지고 `:`·`/` 이스케이프 문제가 생긴다.
- `agent-id`가 다르면 아이템 공간이 분리된다 — 특정 에이전트 철회는
  `security delete-generic-password -s "com.oxibrowser.agent/<agent-id>/…"` 일괄 삭제로 가능.
- 세션 저장소 AEAD 키: `service="com.oxibrowser.agent/_session-keys/<scope>"`,
  `account="aead-key"` (32바이트 랜덤, `getrandom`로 생성 후 `set_secret`, create-once).
- macOS 참고(연구 01 §1): 비번들 CLI는 file-based login 키체인만 접근 가능. 아이템 작성자=소유자라
  재읽기는 무프롬프트(최초 1회 온보딩 패턴의 기반). 안정 서명 신원이 없으면 ACL이 재빌드마다
  무효화된다 — 온보딩 문서로 처리(§9 FM-7).

### 5.2 자격증명 레코드 (kSecValueData에 저장되는 JSON)

```json
{
  "version": 1,
  "id": "kch:main/cloudflare.com/password/dashboard",
  "kind": "password",
  "agent_id": "main",
  "scope": "cloudflare.com",
  "slug": "dashboard",
  "allowed_origins": [
    "https://dash.cloudflare.com",
    "https://login.cloudflare.com"
  ],
  "login_hint": "user@example.com",
  "password": "…",
  "otpauth": "otpauth://totp/Cloudflare:user@example.com?secret=JBSWY3DPEHPK3PXP&issuer=Cloudflare",
  "created_at": "2026-09-27T10:00:00Z",
  "last_used_at": "2026-09-27T12:00:00Z"
}
```

- `allowed_origins`는 **절대 오리진**(스킴+호스트+포트) 목록. OriginPolicy가 exact 매칭에 쓴다.
  eTLD+1 확장 매칭은 하지 않는다(제안 수준조차 v1에서 생략 — 06 §3.1 계층 3은 "제안만"인데
  브라우저 UI가 없는 무인 브라우저에서 제안 수신자가 없으므로).
- `kind: "passkey"`는 P2-M7 예약 필드. 개인키는 이 JSON이 아니라 별도 암호화 항목으로 저장
  (04 §1.4-3).
- `otpauth`는 Google Key URI Format. 존재 시 `has_totp=true`.

### 5.3 세션 저장소 봉투 (02 §4.2 채택 + 암호화 프레임)

평문 페이로드(암호화 전):

```json
{
  "version": 1,
  "created_at": "2026-09-27T10:00:00Z",
  "updated_at": "2026-09-27T12:00:00Z",
  "scope": "tailscale.com",
  "fingerprint": {
    "user_agent": "Mozilla/5.0 … Chrome/149 …",
    "client_hints": { "platform": "macOS" },
    "locale": "ko-KR",
    "timezone": "Asia/Seoul"
  },
  "egress": { "kind": "fixed", "note": "home-wan" },
  "state": {
    "cookies": [ /* StorageState.cookies 그대로 (partitioned/partition_key 포함) */ ],
    "origins": [
      { "origin": "https://login.tailscale.com", "localStorage": [ /* … */ ] },
      { "origin": "https://tailscale.com", "localStorage": [ /* … */ ] }
    ]
  }
}
```

디스크 파일 레이아웃(`~/.oxibrowser/sessions/<scope>.session`, 바이너리):

```
"OXSESS1\n"   8바이트 매직+버전
u32 LE        nonce 길이(고정 24, XChaCha20-Poly1305)
[24]          nonce
나머지        봉투 JSON 직렬화본의 AEAD 암호문 (associated data = 매직 8바이트)
```

- 키체인 부재 시(헤드리스 리눅스 Secret Service 없음) **평문 폴백 금지** —
  `CredError::KeyStoreUnavailable`로 중단 (02 §4.3).
- 쿠키 속성 완전 보존: `CookieEntry`가 sameSite/httpOnly/expiry/partitioned/partition_key를 이미
  직렬화함(cookie.rs L46-82). 세션 쿠키는 `expiry: null`. `partition_key`의
  `{top_level_site, has_cross_site_ancestor}` 구조 확장은 P1-M6b.
- `state`가 기존 `StorageState` JSON 그대로라 Playwright와 양방향 이동 가능.

### 5.4 동의 레코드 (`~/.oxibrowser/consents.jsonl`, 한 줄 = 한 레코드)

```json
{
  "version": 1,
  "consent_id": "c-7f3a9b",
  "credential_id": "kch:main/cloudflare.com/password/dashboard",
  "origin": "https://dash.cloudflare.com",
  "actions": ["login", "mfa"],
  "granted_at": "2026-09-27T10:00:00Z",
  "granted_by": "user:local",
  "expires_at": "2026-10-11T10:00:00Z",
  "max_uses": 50,
  "uses": 3
}
```

폐기 톰스톤:

```json
{
  "version": 1,
  "revoke": "c-7f3a9b",
  "revoked_at": "2026-09-28T08:00:00Z",
  "reason": "user_revoked"
}
```

- 무기한 동의 금지: `expires_at`은 필수이며 기본 14일, `max_uses` 기본 50 (연구 06 §2.1 스키마
  채택, `uses` 카운터와 `consent_id`만 추가).
- 동의 프롬프트의 origin 표시는 페이지 콘텐츠(타이틀·파비콘)가 아니라 브로커가 계산한 값만 사용.

### 5.5 감사 로그 (`~/.oxibrowser/audit.jsonl`)

이벤트 스키마는 §4.1 `AuditEvent`. 예시 라인:

```json
{"ts":"2026-09-27T12:00:00.123Z","seq":42,"kind":"credential_use","session_id":"ses-3","tab_id":"tab-1","origin":"https://dash.cloudflare.com","credential":{"id":"kch:main/cloudflare.com/password/dashboard","fingerprint":"sha256:ab12cd34"},"action":"login","decision":"allow","reason":"consent:c-7f3a9b"}
{"ts":"2026-09-27T12:00:05.456Z","seq":43,"kind":"credential_read","credential":{"id":"kch:main/cloudflare.com/password/dashboard","fingerprint":"sha256:ab12cd34"},"decision":"allow","reason":"broker_resolve"}
{"ts":"2026-09-27T12:10:00.000Z","seq":44,"kind":"policy_violation","origin":"https://evil-example.com","decision":"deny","reason":"origin_not_in_allowlist"}
{"ts":"2026-09-27T12:15:00.000Z","seq":45,"kind":"session_teardown","decision":"allow","reason":"browser_close","detail":{"cleared_cookies":37}}
{"ts":"2026-09-27T12:16:00.000Z","seq":46,"kind":"sensitive_action","decision":"allow","reason":"har_raw_export","detail":{"path":"/tmp/trace.har"}}
```

- 불변식: 어떤 이벤트에도 비밀 평문이 없다. `credential` 참조는 핸들+해시 8자뿐.
- append-only, 매 라인 flush. 로테이션은 P0에서 하지 않는다(크기 주의만 문서화, §9 FM-8).

---

## 6. 표면: CLI · session REPL · CDP OXI

### 6.1 CLI (`crates/oxibrowser/src/main.rs`)

| 명령 | 인자 | 비고 |
|---|---|---|
| `credential put` | `--agent <ID> --site <도메인> --kind <password\|totp\|api-key\|note> [--slug <S>] [--login <힌트>] --origin <오리진>… [--prompt \| --stdin]` | 값은 argv로 받지 않는다(셸 히스토리 방지). 감사 `credential_read` 아님 — `sensitive_action` |
| `credential list` | `[--agent <ID>]` | 메타데이터만 출력 |
| `credential get` | `--id <ID> --field <password\|otpauth>` | **--json과 조합 불가**(값의 JSON 유출 금지). 감사 `credential_read` |
| `credential totp` | `--id <ID>` | 현재 코드 출력(사람용). 남은 시간 < 3초면 다음 창 |
| `credential rm` | `--id <ID>` | |
| `credential onboard` | `--agent <ID> --site <도메인>` | ACL/partition 승인 안내 + 진단(`security dump-keychain` 점검) 출력. 구현은 안내 스크립트 생성까지(연구 01 §4.3) |
| `store save` | `[--scope <도메인>] [--dir <DIR>]` | 기본 scope=현재 탭의 등록 가능 도메인 |
| `store load` | `<scope> [--fingerprint-override]` | 지문 불일치 기본 거부 |
| `store list` / `store rm` | `[--dir <DIR>]` / `<scope>` | |
| `fetch` 플래그 추가 | `--har-raw`, `--no-audit`, `--audit <PATH>` | `--har-raw` 사용 시 stderr 경고 + 감사 `sensitive_action` |

### 6.2 session REPL (`crates/oxibrowser/src/session/parser.rs` + `executor.rs`)

`Command` enum 신규 variants (현재 25 variants 기준, §1 D7):

| 명령 | 인자 | 응답 |
|---|---|---|
| `credential_list` | `[agent]` | `{"credentials": [CredentialMeta…]}` — 메타데이터만 |
| `credential_authorize` | `<id> <origin> <action> [--ttl <SEC>] [--max-uses <N>]` | ConsentRecord 생성, `{"consent_id": "…"}` |
| `credential_forget` | `<id> <origin> [action]` | 동의 폐기 톰스톤 |
| `credential_use` | `<id> <origin>` | 브로커 로그인 실행(감지된 폼에 주입). 값은 응답에 없음: `{"filled": true, "fields": ["password"], "totp": true}` |
| `store_save` / `store_load` | `[scope]` | 세션 저장소 저장/복원. `store_load`는 브라우저에 credential 모드를 켠다 |
| `takeover` | `[timeout_sec]` | P1-M5. 입력 채널 사용자 위임 + 그 구간 캡처 중단 |

**executor 인터셉터 (P1-M4):** `execute()`(executor.rs L14)가 명령을 Tab 메서드로 넘기기 전에
`PolicyEngine`을 통과시킨다:

```
명령 → deny 규칙 → 동의 캐시 → (필요 시) confirmation 대기 → 실행 → 감사
```

credential 모드(session-store 로드 또는 브로커 세션 활성)에서만 활성화되며, 일반 세션의 기존
22→23개 명령 동작은 변경 없다. credential 모드에서 `Fill`/`Type`/`fillRef` 대상이
`input[type=password]`이면 리터럴 값을 거부하고 `credential_use` 경로로만 허용한다.

### 6.3 CDP OXI 도메인 (`crates/oxibrowser-cdp/src/domains/oxi.rs`)

기존 9 메서드(getMarkdown, getPageInfo, getStructuredPage, getAccessibilityTree,
getInteractiveElements, getBoxModelScreenshot, clickRef, fillRef, waitRef) 유지. 추가:

| 이름 | 종류 | 파라미터 | 응답/페이로드 |
|---|---|---|---|
| `OXI.credentialList` | command | `{agent?: string}` | `{credentials: CredentialMeta[]}` (메타데이터만) |
| `OXI.fillCredential` | command | `{ref: string, credentialId: string, fieldKind: "password"\|"totp"\|"apiKey"}` | `{filled: true, masked: true}`. 오류 코드: `consentRequired`, `originMismatch`, `credentialNotFound`, `refStale` |
| `OXI.confirmationRequired` | **event** | — | `{requestId: string, action: string, origin: string, summary: {values: string[], irreversible: boolean}, timeoutMs: number}` — 확인 카드 렌더 근거는 브로커 계산 값만 |
| `OXI.resolveConfirmation` | command | `{requestId: string, approved: boolean}` | `{resolved: true}`. 타임아웃/누락 필드 = 거부(암묵 승인 금지) |

- `fillCredential`이 값 흐름의 유일한 CDP 경로다. `fillRef`에 비밀 리터럴을 넘기는 것은
  credential 모드에서 거부된다(§6.2). TOTP 코드는 CDP 응답으로 절대 반환하지 않고 브로커가
  채운다.
- **CDP 게이팅 (P1-M4):** credential 모드에서 `Network.getAllCookies`/`Network.getCookies`는
  `deniedInCredentialMode` 오류. `Page.captureScreenshot`/`printToPDF`/screencast는 §7 P0-2의
  캡처 가드를 통과해야 한다.

---

## 7. P0 구현 계획 (함수 단위 + 테스트)

P0는 자격증명 기능과 무관한 **유출 면 축소**다. 네 개의 독립 PR로 쪼갠다(§8의 M0.x).

### P0-1. HAR/네트워크 로그 리랙션 기본화 (M0.1)

**변경 파일**
- 신규 `crates/oxibrowser-core/src/security/{mod.rs,redact.rs}`
- 수정 `crates/oxibrowser-core/src/network/har.rs` — `request_record_to_entry()`(L44 부근)
- 수정 `crates/oxibrowser-core/src/network/mod.rs` — `pub mod security`가 아니라 crate 루트
  `security` 모듈 등록 (`lib.rs`)
- 수정 `crates/oxibrowser/src/main.rs` — `fetch`Cmd에 `--har-raw`(L86-87 옆), `write_har()`(L576)
  경고+감사

**기본 민감 헤더 목록** (연구 06 §4.1 채택):

```
authorization, proxy-authorization, cookie, set-cookie,
x-api-key, x-auth-token, x-access-token, x-refresh-token,
x-csrf-token, x-xsrf-token
```

**함수 단위**

1. `har.rs` 상단에 `use crate::security::redact::{…}`. `to_har_json()`에 파라미터 추가는
   breaking이므로 **모듈 상수 `HAR_REDACTION: RedactionProfile`를 읽도록** 구현하고, raw 경로만
   `to_har_json_raw()`를 별도 함수로 노출한다.
   ```rust
   pub fn to_har_json(records: &[RequestRecord]) -> Value {          // 리랙션 기본
       to_har_json_impl(records, Redact::On, &RedactionProfile::default_har())
   }
   pub fn to_har_json_raw(records: &[RequestRecord]) -> Value;       // --har-raw 전용
   ```
2. `request_record_to_entry()` 내부(현재 L72-94):
   - `request_headers`/`response_headers` 생성부 → `redact::redact_headers()` 통과.
     `set-cookie`는 값 전체 REDACTED.
   - `"url"` 필드 → `redact::redact_url_query()`. `queryString`은 기존대로 `[]` 유지(D3).
   - `postData` 생성부 → `redact::redact_post_body()`:
     - `content-type: application/x-www-form-urlencoded` → 폼 파싱 후 민감 키 값만 치환.
       민감 폼 키 기본 목록: `password, passwd, pass, pwd, secret, token, otp, code, auth`.
     - base64 폼(har.rs L198-200 테스트 케이스 존재) → 디코드 시도→리랙션→재인코딩, 실패 시 전체
       REDACTED.
     - 그 외(JSON 로그인 페이로드 포함) → `{"text": "__REDACTED__", …}` 전체 치환. 선택적
       JSON 필드 마스킹은 하지 않는다(과설계 방지).
   - `post_body_truncated` 플래그는 치환 후에도 원래 잘림 사실을 보존.
3. `main.rs`: `--har-raw` 플래그 → `network_log_har_raw()` 호출 경로 + stderr 경고
   (`"HAR will contain unredacted credentials"`) + `AuditLog::record(sensitive_action)`(P0-4와
   같은 PR이 아니면 tracing warn만, 감사 연결은 M0.4).
4. CDP 이벤트(D4): `network.rs::emit_navigation_events/emit_response_events`,
   `page.rs`의 세 이벤트 방출부가 받는 `url`을 `redact_url_query()` 통과. 헤더는 현재 빈 객체이므로
   변경 없음 — 단 `//!` 문서에 "request 헤더를 추가할 때 반드시 redact_headers를 통과할 것"
   불변식 주석.

**테스트 계획**

- 단위 (`security/redact.rs`): 헤더 대소문자 무시(`COOKIE`/`cookie`), set-cookie 전체 치환, URL
  다중 파라미터·퍼센트 인코딩 키(`access%5Ftoken`), 파서 실패 시 원문 보존, form body 왕복
  (치환 후에도 나머지 필드 보존), base64 폼, JSON body 전체 치환, REDACTED에 값 길이 정보 없음.
- 단위 (`network/har.rs` 기존 테스트 확장): Authorization/Cookie 헤더가 REDACTED로 나오는지,
  `url`의 `token=` 값 치환, truncated 플래그 보존.
- 통수: `cargo run -- fetch <URL> --har x.har --json` 후 x.har에 `Authorization` 실값 부재 —
  로컬 httpbin류 픽스처로. `--har-raw`는 실값 존재 + 감사/경고 확인.

### P0-2. `type=password` 마스킹 + 캡처 가드 (M0.2)

**변경 파일**
- 수정 `crates/oxibrowser-core/src/js/dom_snapshot.rs`
- 수정 `crates/oxibrowser-core/src/session.rs` (`capture_screenshot_png` 내부 가드)
- 수정 `crates/oxibrowser-core/src/tab.rs` (`screenshot()` L845 — 에러 전파 확인)
- 수정 `crates/oxibrowser-core/src/js/form.rs` — 활성 요소 포커스 프로브 JS 스니펫 추가
- CDP `Page.captureScreenshot`/`printToPDF`/screencast는 전부 `Session::capture_screenshot_png`
  단일 관문을 지나므로(page.rs L557, L609, L741-742) 코어 가드 하나로 세 경로가 모두 막힌다.

**차단 지점 (D11에서 특정한 3곳)**

1. `dom_snapshot.rs` 빌드 경로 — 프레임→노드 변환 후 속성 후처리:
   ```rust
   /// input[type=password]의 value 속성을 REDACTED로 치환한다. 스냅샷은 읽기 전용
   /// 관찰이므로 실제 문서 값에는 영향이 없다.
   fn mask_password_inputs(nodes: &mut HashMap<u32, DomNode>) {
       for node in nodes.values_mut() {
           if node.tag.eq_ignore_ascii_case("input")
               && node.attributes.get("type").map(|t| t.trim()) == Some("password")
           {
               if let Some(v) = node.attributes.get_mut("value") { *v = REDACTED.to_string(); }
           }
       }
   }
   ```
   `from_frame`의 `compose_shadow_trees`/iframe 병합 완료 직후 1회 호출.
2. `accessible_name()` (L1933+) — 5번 fallback(`value`, L2013-2018)에서 password 입력이면 value를
   건너뛴다. 마스킹된 스냅샷이라면 이미 REDACTED지만, 미래에 value 소스가 늘어나는 것을 막는
   이중 안전장치. `input_type == password`일 때 `attr("value")` 분기 스킵.
3. 캡처 가드 — `Session::capture_screenshot_png` 진입 직전:
   ```rust
   /// 활성 요소가 password 입력이면 캡처를 거부한다(Operator 방식의 자동화 버전).
   async fn guard_sensitive_capture(&self) -> Result<()> {
       if self.eval_js(js::form::js_active_password_probe()).await?.as_bool() == Some(true) {
           audit.policy_violation("capture_blocked_password_focus");  // M0.4 연결
           return Err(Error::CaptureBlocked("password field focused"));
       }
       Ok(())
   }
   ```
   `js_active_password_probe()`는 `document.activeElement?.type === 'password'` 불리언 반환.
   우회 플래그는 P0에서 제공하지 않는다 — 비밀번호 필드 포커스 중 캡처 요구는 합법 사례가
   없다. 스크린샷 자동화가 password 필드 포커스 상태에서 막히면 명시적으로 다른 곳을 클릭해야
   한다(의도된 마찰).
4. OXI.getMarkdown — `p.to_markdown()`(oxi.rs L49)은 렌더 문서에서 만든다. form 컨트롤 값이
   markdown에 나오는지 검증 테스트를 넣고, 유출 시 렌더 마크다운 파이프라인에 value 속성 억제를
   추가한다. **검증 전까지 "마스킹됨"이라고 주장하지 않는다.**

**테스트 계획**

- 단위 (dom_snapshot): `input[type=password][value=secret]` 포함 페이지 → 스냅샷 attributes.value
  == REDACTED, `interactive_elements()`의 name이 secret 미포함(accessible_name fallback 차단),
  같은 페이지의 text input value는 보존. value 없는 password 입력(빈 값)에서 마스킹이 노이즈를
  안 만드는지. shadow DOM/iframe 내 password 입력도 마스킹(병합 후 호출 순서 보장).
- 단위 (form.rs): 프로브 JS가 포커스 password에서 true.
- 통합: 스냅샷→`OXI.getInteractiveElements` 와이어 포맷에 secret 미포함.
- 스크린샷 가드: 포커스 프로브 true 주입 시 `CaptureBlocked` 오류, CDP captureScreenshot이
  빈 PNG 폴백(page.rs L560의 `unwrap_or_else`)이 아닌 **오류 전파**가 되도록 page.rs도 수정 —
  빈 PNG 폴백은 가드를 무력화하므로 가드 오류는 폴백하지 않고 그대로 반환.

### P0-3. 쿠키 jar 수명 정리 (M0.3)

**사실 관계(§1 D5):** `cookie_file` 기본은 이미 `None`. 남은 갭: (a) `Browser::close`가 jar를
clear하지 않음, (b) 장수 프로세스(`serve`)에서 글로벌 jar가 세션 경계 없이 공유됨, (c) credential
모드에서 cookie_file 사용 통제 없음.

**변경 파일** — `crates/oxibrowser-core/src/browser.rs`, `config.rs`(신규 필드 1개)

**함수 단위**

1. `Browser::close()` (L225-239): 파일 저장 블록 **뒤에** clear 추가.
   ```rust
   // 인메모리 jar도 폐기한다. cookie_file 사용자는 위 블록에서 이미 저장되었다.
   let cleared = self.cookie_jar.write().clear_and_count();
   audit.session_teardown(cleared);  // M0.4 연결 지점, 없으면 tracing만
   ```
   `CookieJar::clear_and_count() -> usize` — 기존 `clear()`(L677)를 감싸 폐기 수를 반환(감사
   detail용). 기존 `clear()` 시그니처는 유지.
2. `BrowserConfig` 신규 필드: `pub clear_cookies_on_close: bool` (기본 `true`,
   `serde(default = "default_true")`). 기존 cookie_file 사용자의 영속 의도는 유지하면서 인메모리
   잔존만 막는다.
3. credential 모드 가드(P1-M6에서 실제 스위치가 생기지만 불변식은 지금 문서화):
   "세션 저장소가 로드된 브라우저는 `cookie_file` 구성을 거부한다" — `Browser::new`에서
   `session_store_active && config.cookie_file.is_some()` → 오류.

**테스트 계획**

- 단위 (cookie.rs): `clear_and_count` 반환값.
- 단위 (browser.rs): close 후 jar 비어 있음(`get_all().is_empty()`), `clear_cookies_on_close=false`
 면 보존, cookie_file 경로 설정 시 저장 파일 존재 + 이후 jar 비어 있음.
- 기존 `config.rs` 테스트(L417-435, L502-521)에 신규 필드 기본값 1줄 추가 — 기존 "builder ==
  default" 불변식 유지 확인.

### P0-4. 감사 로그 JSONL (M0.4)

**변경 파일** — 신규 `security/audit.rs`(§4.1), 수정 `main.rs`(CLI 플래그), `browser.rs`(teardown
발생), `tab.rs`/`session.rs`(capture_blocked 발생), `har.rs`는 M0.1이 이미 tracing으로 남기므로
감사는 raw export에서만.

**함수 단위**

1. `AuditLog`는 전역 하나(`OnceLock<Arc<AuditLog>>`, `oxibrowser` 바이너리와 `serve` 초기화 경로에서
   `open_default()`). 열기 실패 시: 크래시하지 않고 tracing error + 감사 비활성(스푸핑보다 가용성;
   credential 기능은 P1에서 감사 필수로 강화).
2. 발생 지점(P0 범위):
   - `sensitive_action` / `har_raw_export` — main.rs `--har-raw` 쓰기 직후
   - `session_teardown` — browser.rs close, `detail.cleared_cookies`
   - `sensitive_action` / `cookie_file_save` — browser.rs close의 저장 성공/실패
   - `policy_violation` / `capture_blocked_password_focus` — P0-2 가드
   - `credential_use`/`credential_read` — 스키마만 확정, 발생은 P1-M2/M4
3. CLI: `--audit <PATH>`(위치 변경), `--no-audit`(비활성). `serve`도 동일 플래그.
4. `secret_fingerprint()` — 이 시점부터 HAR/로그에 남는 비밀 참조 형식을 통일한다.

**테스트 계획**

- 단위: 연속 record → 파일의 각 줄이 파싱 가능한 JSON, `seq` 단조 증가, 필수 필드(ts/kind/decision)
  존재. 디렉터 자동 생성. `--no-audit` 시 파일 미생성.
- 부정 테스트: 알려진 비밀 문자열("hunter2")을 이벤트에 넣는 API가 없음을 타입으로 보장
  (AuditEvent에 자유 문자열 필드가 `reason`/`detail`뿐임을 문서로 명시하고, detail에 credential
  값 금지 규칙을 디버그 어서트로 — release에서는 검사 안 함).
- 통합: `fetch --har-raw` 1회 → audit.jsonl에 `sensitive_action` 1줄.

---

## 8. P1/P2 마일스톤 — PR 분할

P0 4개 PR에 이어 P1 6개, P2 3개. 각 PR은 독립 머지 가능, 뒤 PR이 앞 PR의 트레잇/스키마에 의존.

| PR | 제목 | 범위 | 의존 | 수용 기준 |
|---|---|---|---|---|
| M0.1 | `core: redact HAR and network logs by default` | P0-1 | — | 리랙션 단위/통합 테스트, `--har-raw` |
| M0.2 | `core: mask password inputs in snapshots, guard capture` | P0-2 | — | 마스킹 3지점 테스트, 캡처 가드 |
| M0.3 | `core: clear cookie jar on close, credential-mode guard` | P0-3 | — | teardown 테스트 |
| M0.4 | `core: JSONL audit log` | P0-4 | M0.1-3 순서 무관(연결만) | 감사 이벤트 테스트 |
| M1 | `core: origin policy for credential use` | `network/origin_policy.rs` + 리다이렉트 판정 훅(`Session::navigate` 결과에 verdict 노출) | — | DNS 라벨 경계/정규화/iframe fail-closed 테스트, `notexample.com` 반례 |
| M2 | `credentials: keyring-backed credential provider` | 신규 크레이트 골격, SecretBox, KeyringProvider, CLI `credential put/get/list/rm/totp/onboard`, 감사 `credential_read` | M0.4 | 키체인 왕복 테스트(macOS CI), ServicePrefix 컨벤션 테스트, Debug 미파생 컴파일 단정 |
| M3 | `credentials: TOTP generation` | `totp.rs`(totp-rs), `credential totp`, otpauth 정규화 | M2 | RFC 6238 벡터 테스트, 창 경계 로직 |
| M4 | `credentials: consent + policy engine + CDP surface` | ConsentStore, PolicyEngine, executor 인터셉터, `OXI.credentialList/fillCredential/confirmationRequired/resolveConfirmation`, CDP 쿠키 게이트 | M1, M2 | deny 우선순위 테스트, 동의 만료/횟수, confirmation 타임아웃=거부 |
| M5 | `session: takeover mode` | REPL/CDP 입력 채널 위임, 구간 캡처 중단 | M4 | takeover 중 `Page.captureScreenshot` 거부, 타임아웃 복귀 |
| M6 | `credentials: encrypted session store` | SessionEnvelope+AEAD+키체인 키, `store` CLI/REPL, **다중 오리진 export**(session.rs export_state 확장) | M2 | 암호화 프레임 왕복, 지문 불일치 거부, 원자적 치환, 다중 오리진 테스트 |
| M6b | `core: structured cookie partition key` | `CookieEntry.partition_key: Option<String>` → `Option<PartitionKey{top_level_site, has_cross_site_ancestor}>` (serde alias로 구문호환) | — | CHIPS 왕복 테스트, 구문호환 테스트 |
| M7 | `cdp: WebAuthn virtual authenticator` (P2) | `domains/webauthn.rs`, boa `navigator.credentials`, p256 ES256, attestation none | M4 | CDP 명령 시퀀스, 개인키 키체인 저장, RP ID 검증 테스트 |
| M8 | `core: two-layer profiles` (P2) | 영구 프로파일 + `persist:false` 읽기전용 + advisory lock | M6 | 동시 실행 잠금 테스트 |
| M9 | `skills: requires manifest` (P2) | 스킬 매니페스트 `requires: {credentials, irreversible_actions}`, auth 스킬 | M4 | 매니페스트→동의 매칭 테스트 |

PR 하나가 커지면 도메인 게이트부터 잘라낸다(M4는 게이트 없는 consent+engine과 게이트 추가로 2분할
가능).

### 구현 상태 (2026-09-27)

| PR | 상태 | 비고 |
|---|---|---|
| M0.1 | ✅ 구현 | `security/redact.rs` 신설, `har.rs` `to_har_json`(기본 리랙션)/`to_har_json_raw`, `--har-raw` + stderr 경고 + 감사. CDP 네트워크 이벤트 url 리랙션과 `--redact-header` 프로파일 파일은 잔여 |
| M0.2 | ✅ 구현 | 스냅샷 value 마스킹 + `accessible_name` password fallback 차단 + `capture_screenshot_png` 가드(감사 `capture_blocked_password_focus`), CDP `captureScreenshot`/`printToPDF` 오류 전파. screencast 펌프는 같은 관문이라 프레임 공백으로 동작 |
| M0.3 | ✅ 구현 | `CookieJar::clear_and_count`, `BrowserConfig::clear_cookies_on_close`(기본 true, builder `cookies_on_close`), `Browser::close` clear + 감사. credential 모드 가드는 M2/M6 도입 시 |
| M0.4 | ✅ 구현 | `security/audit.rs` 글로벌 JSONL 로거, 전역 `--audit/--no-audit`(모든 서브커맨드), 발생 지점 4종 연결 |
| M1 | ✅ 구현 | `network/origin_policy.rs` — Origin 정규화(punycode/포트), DNS 라벨 경계, 프레임 fail-closed, deny 우선, redirect verdict. 소비자(PolicyEngine)는 M4에서 연결 |
| M0.1 잔여 | ✅ 해소 | 전역 `--redact-header NAME`(fetch·serve 공통) — `redact::active_profile()`로 HAR·CDP 이벤트 URL 모두 확장 헤더 적용 |
| M2 이하 | ⬜ 미구현 | |

> **2026-09-28 정정**: M5(역할 인지 takeover로 확장)·M6(core 이동 + (account_id, scope) 키로 재키)·
> M8(M-A 위에 구축)의 범위와 신규 마일스톤 M-A~M-D는
> `2026-09-28-account-login-session-management.md`(계정 로그인 세션 관리 상위 설계)에서 정의한다.

---

## 9. 실패 모드

연구의 보안 주장 중 **실현 시 주의가 필요한 것**. 각 항목은 설계가 취하는 방어를 함께 적는다.

### FM-1. HAR 리랙션이 전부를 잡지 못한다 (연구 06 §4.1의 Chrome 면책과 동일)

- **조직 고유 인증 헤더**는 기본 목록에 없다. → `RedactionProfile::with_extra_headers()`와
  `--redact-header` CLI 옵션(P1에서 프로파일 파일 지원)으로 확장 가능하게 하되, **미등록 헤더의
  민감성은 자동 탐지할 수 없다**는 한계를 문서화한다.
- **값이 키 매칭을 피하는 인코딩**: 이중 퍼센트 인코딩, 폼이 아닌 JSON 본문의 토큰 등은 필드 단위
  치환을 회피할 수 있다. → JSON/알 수 없는 본문은 전체 REDACTED가 기본(P0-1), 키 매칭은 디코딩
  후 비교.
- **URL 쿼리의 비민감 이름 민감 값**(예: `/reset?xyz=토큰`)은 키 목록으로 못 잡는다. → 실패 모드로
  명시하고, 로그인 흐름 분석이 필요한 세션은 `--har` 자체를 쓰지 않는 운영 지침으로 대신한다.
- **`--har-raw`는 의도적 위험 스위치**다. 경고+감사로 추적 가능하게 하지만 막지는 않는다.
- **CDP 이벤트의 url**도 리랙션 대상이지만, 이벤트를 소비하는 클라이언트는 어차피 페이지와 동등한
  권한 있다(CDP=브라우저 침해, 연구 06 §8 전제). 리랙션은 "저장 아티팩트" 보호가 목적이지 CDP
  클라이언트 신뢰 경계가 아니다.

### FM-2. exact-origin 매칭의 예외 사례

- **OAuth/SSO 리다이렉트 체인**: 로그인 흐름은 IdP 오리진(login.microsoftonline.com 등)을 반드시
  거친다. 단일 오리진 동의로는 즉시 `InvalidateAndEscalate` 오타가 난다. → 자격증명 레코드의
  `allowed_origins`가 **오리진 집합**을 담도록 설계했고(§5.2), 동의도 (credential, origin, action)
  단위로 체인 각 오리진에 대해 받는다. 리다이렉트가 집합 밖으로 나가는 순간 자격증명 언로드 +
  감사 + 상승.
- **패스키는 스코프 단위가 다르다**: WebAuthn RP ID는 통상 eTLD+1이라(연구 06 §3.3) 비밀번호의
  exact-origin 규칙을 패스키에 적용하거나 그 반대를 하면 안 된다. → `CredentialKind::Passkey`의
  매칭 규칙은 M7에서 별도 정의하고, 그 전까지 passkey 종류는 정책 엔진이 Deny한다.
- **프레임 오리진 미확인**: 폼이 크로스오리진 iframe에 있고 프레임 오리진을 확정할 수 없으면
  fail-closed(Deny). 최상위 오리진으로 대체 판단하지 않는다 — `attacker.example`이
  `login.example` 폼을 사입하는 케이스(06 §3.2).
- **http 전용 셀프호스팅 콘솔**: 저장 오리진이 https면 http에 자동 주입하지 않는 원칙을 유지하되,
  http 오리진은 명시적 동의 레코드로만 허용한다(스킴 포함 exact 비교이므로 자연히 강제됨).
- **정규화 실패**: punycode/유니코드 혼동, 기본 포트(`:443`), 대문자 호스트. → `Origin::parse`가
  생성 시점에 정규화하고, 정규화 실패 URL은 매칭 시도 자체를 Deny한다.
- **오리진 고정의 근본 한계**: 오리진 검사는 URL 호스트만 본다. 해당 오리진이 침해되었거나
  CNAME/프론트로 악성 콘텐츠가 서빙되면 통과된다. origin pinning은 피싱 방어지 침해된 정상
  사이트 방어가 아님을 문서화한다.

### FM-3. 승인 피로 (연구 06 §2.2, CSA GhostApproval)

확인 프롬프트를 유일한 방어선으로 쓰지 않는다 — deny 규칙과 origin 고정이 결정론적으로 먼저
평가되고, 읽기/탐색은 동의 없이 허용해 프롬프트 빈도를 낮춘다. 확인 요청에는 항상 타임아웃
(기본 거부)이 있다.

### FM-4. 캡처 가드의 우회

`Page.captureScreenshot`의 기존 빈 PNG 폴백(page.rs L560)은 가드 오류를 삼켜버린다. 가드 오류는
폴백 없이 전파하도록 page.rs를 함께 고친다(P0-2). screencast 펌프도 같은 관문을 지나므로 비밀번호
포커스 중 프레임이 끊긴다 — 의도된 동작이며, 클라이언트에는 `screencastFrame` 공백으로만 나타난다.

### FM-5. 세션 지문·egress 메타는 사전 점검일 뿐이다

`FingerprintMeta`/`EgressMeta`는 주입 시점 비교 가능한 힌트다. 실제 이그레스 IP 변경을 감지하려면
외부 요청이 필요한데 그 자체가 신호를 노출한다. → v1은 UA/Client Hints/로케일/타임존 불일치만
거부하고, egress는 기록 전용으로 문서화한다. Cloudflare 챌린지 루프(연구 02 §3.1)는 이 메타로
예방되지 않을 수 있다.

### FM-6. AEAD 키 손실·키체인 부재

키체인의 키를 지우면 세션 파일은 복구 불가(설계대로). 키체인 접근 불가 환경에서 평문 폴백은 하지
않고 중단한다(02 §4.3). 백업 요구는 사용자 문서로.

### FM-7. 키체인 ACL/partition 운영 리스크 (연구 01 §1.2)

- ad-hoc 서명 재빌드 → ACL 무효 → 매 실행 프롬프트. 안정 서명 전까지 `credential` 기능은
  "프롬프트가 뜰 수 있음"을 온보딩 출력에 명시한다.
- `set-generic-password-partition-list`는 로그인 비밀번호를 요구해 무인 불가 → 1회성 사람 절차로
  분리, `credential onboard`가 진단+스크립트 생성만 한다.
- 헤드리스 리눅스에서 Secret Service 부재 → `CredError::KeyStoreUnavailable`로 TOTP/자격증명 기능
  전체 중단(부분 동작으로 오해시키지 않는다).

### FM-8. 감사 로그 자체의 한계

같은 사용자 권한 프로세스가 쓰는 로그라 로컬 침해자는 변조할 수 있다(append-only는 프로세스 수준
보장). 크기 무한 증가 — P0 로테이션 없음, 문서에 "수 MB 단위에서 수동 로테이션" 안내. 원격 전송/
서명은 비목표(§2.3).

---

## 10. 의존성 (crates.io 검증)

crates.io API로 2026-09-27 조회·검증한 버전과 라이선스:

| 크레이트 | 버전 | 라이선스 | 도입 | 용도 |
|---|---|---|---|---|
| `keyring` | 4.2.0 | MIT OR Apache-2.0 | P1-M2 | 키체인 어댑터. 4.x는 저장소 백엔드가 별도 크레이트(`apple-native-keyring-store` 1.0.2 등)로 분리된 구조 — 도입 시 feature 구성은 docs.rs 기준 재확인 [연구 01 §1.4 기일 동일, 재확인 권장] |
| `apple-native-keyring-store` | 1.0.2 | (crates.io 확인 요망 — 본 검증 미포함) | P1-M2 (keyring 4.x 경로 시) | macOS 네이티브 백엔드 |
| `security-framework` | 3.7.0 | MIT OR Apache-2.0 | (선택, P2 브로커 분리 시) | SecItem 직접 접근. keyring으로 충분하면 미도입 |
| `zeroize` | 1.9.0 | Apache-2.0 OR MIT | P1-M2 | SecretBox drop 시 메모리 클리어 |
| `sha2` | 0.11.0 | MIT OR Apache-2.0 | P0-4 | 비밀 핑거프린트(SHA-256 앞 8자). hmac 트레잇 미사용이라 워크스페이스 sha1 0.10과 세대 충돌 없음 |
| `chacha20poly1305` | 0.11.0 | Apache-2.0 OR MIT | P1-M6 | 세션 봉투 AEAD (XChaCha20-Poly1305) |
| `totp-rs` | 6.0.0 | MIT | P1-M3 | TOTP 생성(§4.4 세대 충돌 회피 1순위) |
| `hmac` 0.12 + `data-encoding` 2.11.1 | 0.12.x / 2.11.1 | MIT OR Apache-2.0 / MIT | (대안) | DIY TOTP 경로 — 워크스페이스 sha1 0.10 세대와 정합 |
| `p256` | 0.14.0 | Apache-2.0 OR MIT | P2-M7 (`ecdsa` feature) | WebAuthn ES256 서명 |
| `getrandom` | 0.3 (기존) | — | P0/M6 | AEAD 키·nonce 생성 (신규 의존 없음) |

P0가 새로 추가하는 의존은 `sha2` 하나뿐이다. `psl`(등록 가능 도메인)·`url`(오리진 정규화)·
`serde_json`은 기존 워크스페이스 의존을 쓴다.

---

## 부록 A. 연구 인용 → 현재 코드 전수 재검증표

| 연구 인용 | 문서 | 현재 코드 | 판정 |
|---|---|---|---|
| `session.rs` L39-64 `RequestRecord` 평문 보관 | 06 §8.3 | session.rs L39-64 (L52/54/60) | ✅ 정확 |
| `main.rs` `write_har` 경로 | 06 §8.3 | main.rs L576 — 단 직렬화는 `network/har.rs::to_har_json` L37 | ⚠️ 경로 정정 (D2) |
| `browser.rs` L50 글로벌 `cookie_jar`, L69-89 `cookie_file` | 06 §8.4 | L50, L69-90 | ✅ 정확 |
| `cookie.rs` L677 `clear()` | 06 §8.4 | L677-679 | ✅ 정확 |
| `storage_state.rs` Playwright 미러 | 00 §4 | L20-48, 문서 주석 명시 | ✅ 정확 |
| `challenge.rs` CF/DD/PX 분류 + clearance 쿠키명 | 00 §4, 02 §3.3 | L32-65 + **AkamaiImperva L207-213** | ⚠️ 드리프트 (D6) |
| client.rs Interactive/Blocked 중단 | 02 §3.3 | client.rs L593-601 | ✅ 유효 |
| `js/stealth.rs` | 00 §4 | ChromeProfile/StealthSurface/build/attach_to_navigator | ✅ 존재 |
| export 단일 오리진 한정 | 02 §1.1 | session.rs L2266-2280 주석 명시 | ✅ 유효 |
| `pending_seed` L262-266 / 주입 L1826-1830 | 02 §1.1 | L265-266 / L1828-1830 | ⚠️ 마이너 시프트 |
| `js/runtime.rs` 2245-2248 씨드 제약 | 02 §1.1 | L1631-1643 | ⚠️ 시프트, 의미 불변 |
| session REPL 22 명령 | 00 §4 | 25 variants (기능 23 + Help/Exit) | ⚠️ 드리프트 (D7) |
| `mcp.rs` 존재 | 00 §4 | crates/oxibrowser/src/mcp.rs | ✅ |
| skills install·webfetch, auth 슬롯 공백 | 00 §4 | skills/oxibrowser-{install,webfetch}/SKILL.md | ✅ |
| WebAuthn 전무 | 04 §1.4 | crates/ 전체 grep 무일치 | ✅ 재확인 |
| DomSnapshot password 마스킹 부재 | 06 §4.2 | 부재 확인 + 유출 경로 2곳 특정 (L2013-2018, L1628-1634) | ✅ + 상세화 (D11) |
| `cookie_file` 기본 끔 필요 | 06 P0.3 | 이미 기본 None (config.rs L222-224, 테스트 L431-434) | ⚠️ 이미 충족 (D5) |
| CDP 이벤트 헤더 노출 우려 | 06 §8.3 | 이벤트 헤더는 `{}` (network.rs L365); url은 원문 | ⚠️ 정정 (D4) |
| HAR `queryString`/응답바디 | (연구 미상세) | queryString 항상 `[]`(L88), 응답 바디 미수록(L105-108) | 신규 사실 (D3) |
| AGENTS.md "10 domain handlers" | (AGENTS.md) | 도메인 모듈 12개 | AGENTS.md 갱신 필요 (D14) |

재검증일: 2026-09-27 (연구 작성일과 같은 날, 코드 변경 없는 시점 기준)
