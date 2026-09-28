# 계정 로그인 세션 관리 — 설계 (2026-09-28)

> 목표: Codex Desktop 내장 브라우저처럼 — **사용자가 1회 로그인하면 계정·세션이 샌드박스에 보관**되고,
> 에이전트가 동의 범위 안에서 해당 계정으로 무인 작업(GitHub 로그인·사용 등)을 진행하는
> "슈퍼 에이전트 브라우저" 계층을 만든다.
>
> 관계: `2026-09-27-agent-auth-implementation.md`(이하 §하위 설계)의 **상위 제품 설계**. 하위 설계는
> 자격증명 평면(비밀 보관·주입)과 세션 파일 포맷을 다룬다. 본 문서는 이를 **Account**라는 사용자 친화
> 개념으로 엮고, 로그인 오케스트레이션·계정 샌드박스 런타임·상태 보드를 신규 정의한다. 하위 설계의
> M5/M6/M8 범위를 정정하고 신규 마일스톤 **M-A ~ M-D**를 추가한다(§9).
>
> 코드 사실 기준: 2026-09-28 스카우트 전수 확인(v0.22.x). 핵심 확인 사항 —
> `Browser`가 **글로벌 쿠키 jar** 1개를 모든 `Session`에 공유(browser.rs `new_session`),
> localStorage는 **오리진 구분 없는 단일 flat 맵**(session.rs `local_storage`), CDP
> `browserContextId`는 전부 하드코딩 `"default"`이고 `Target.createBrowserContext` 미구현,
> 모든 웹 저장소는 프로세스 종료 시 소멸(지속은 `--cookie-file` 전체 jar JSON 평문과
> `save-state`/`load-state`뿐).

---

## 목차

1. [개념 모델 — Codex Desktop 대응](#1-개념-모델--codex-desktop-대응)
2. [아키텍처](#2-아키텍처)
3. [BrowserContext — 계정 샌드박스 런타임 (M-A)](#3-browsercontext--계정-샌드박스-런타임-m-a)
4. [Account 레지스트리와 수명 주기 (M-B)](#4-account-레지스트리와-수명-주기-m-b)
5. [로그인 오케스트레이션 (M-C/M-D)](#5-로그인-오케스트레이션-m-cm-d)
6. [저장 포맷·디렉터리·보안 등급](#6-저장-포맷디렉터리보안-등급)
7. [표면: CLI · session REPL · CDP OXI · MCP · skill](#7-표면-cli--session-repl--cdp-oxi--mcp--skill)
8. [보안 모델 — 위협과 경계](#8-보안-모델--위협과-경계)
9. [마일스톤 정정·PR 분할](#9-마일스톤-정정pr-분할)
10. [실패 모드](#10-실패-모드)
11. [의존성](#11-의존성)

---

## 1. 개념 모델 — Codex Desktop 대응

| 개념 | 정의 | Codex Desktop 대응 |
|---|---|---|
| **Account** | 한 사이트 스코프의 사용자 신원 단위. 메타데이터(신원 카드) + 자격증명 핸들 + 세션 봉투 + 에이전트 그랜트의 묶음. 슬러그 식별(`gh-work`). | "계정 관리자"에 나열되는 계정 |
| **IdentityCard** | 계정의 비밀 아닌 표시 정보: `display_name`, `login_hint`, `avatar_url`, 사이트. 에이전트에게 노출 **허용**. | 연락처/계정 정보 카드 |
| **Session Envelope** | 로그인 상태 스냅샷(쿠키+localStorage+지문+egress). AEAD 암호화 파일. 스코프 단위, (account_id, scope) 키. | 내장 브라우저의 로그인 지속 |
| **BrowserContext** | 계정 1:1로 묶이는 실행 격리: 전용 쿠키 jar + 오리진별 localStorage + 고정 지문/egress. `Target.createBrowserContext`로 매핑. | (Chrome incognito/context 개념) |
| **Grant(동의)** | (agent_id × account_id × action 클래스 × 만료/횟수) 사용 허가. 하위 설계 M4 ConsentRecord의 확장. | "이 에이전트가 이 계정을 쓸 수 있음" |

핵심 원칙(하위 설계 §3에서 승계, 계정에 재적용):

1. **비밀값은 브로커 밖으로 나가지 않는다** — 세션 봉투(쿠키 포함)도 비밀 취급. 에이전트는 값을
   못 받고 "그 계정으로 실행된 컨텍스트"를 받는다.
2. **인증 상태는 2층** — 세션 봉투(재사용) → 만료 시 자격증명(재로그인) → 안 되면 인간 상승.
3. **게이트는 실행 계층에서 결정론적으로** — deny → 그랜트 → confirmation 순서. LLM에게 물어보게
   하지 않는다.

## 2. 아키텍처

```
표면                              오케스트레이션                     실행·보관
─────────────────────           ─────────────────────           ─────────────────────
CLI account login/status   ─┐                                   BrowserContext (core)
session REPL account_*      ─┼─▶ AccountManager ─▶ LoginOrchestrator ─▶ ├ CookieJar (전용)
CDP OXI (account*, login)   ─┘   (레지스트리·수명)   (4가지 부트스트랩)   ├ localStorage(오리진별)
MCP / skill                 ─┐                      ├ LoginDetector  ├ HttpClient(egress 고정)
host app (viewer role)      ─┘                      └ ValidationProbe┘ └ fingerprint 잠금
                                                          │
                                     PolicyEngine(M4) ◀────┤
                                     ConsentStore          ▼
                                     KeyringProvider   SessionStore (AEAD 파일, core)
                                     (credentials 크레이트)  ~/.oxibrowser/accounts/<id>/sessions/
                                                          │
                                     AuditLog (전면 감사, 값 대신 핸들+해시)
```

- 모듈 배치(의존 방향 충돌 회피, 상세 §3.5): **core**에 `context.rs`, `storage/session_store.rs`,
  `account/`(레지스트리·탐지·오케스트레이션)를 둔다. 하위 설계는 SessionStore를
  `oxibrowser-credentials`에 두었으나 **core로 이동 정정** — 오케스트레이터(core)가 저장소를
  호출해야 하는데 credentials → core 단방향 원칙이 순환을 만들기 때문. 키체인 접근은 core의
  `KeyProvider` 트레잇으로 추상화하고 credentials 크레이트가 구현한다(§3.5).
- 신규 비밀 관련 코드는 여전히 `security` 모듈 감사를 통과한다. 계정 기능의 모든 상태 전이는
  감사 이벤트를 남긴다(§6.4).

## 3. BrowserContext — 계정 샌드박스 런타임 (M-A)

### 3.1 동기 — 현재 구조의 격리 부재

- `Browser`가 jar 1개(`Arc<RwLock<CookieJar>>`)를 모든 `Session`에 공유 → A 사이트 로그인 쿠키가
  B 에이전트 세션에 그대로 노출. 계정 기능의 전제가 안 된다.
- localStorage가 오리진 무시 flat 맵 → 다중 오리진 세션(M6′)의 주입 대상이 없음.
- CDP `browserContextId` 하드코딩 → Playwright/Puppeteer 표준 경로로 컨텍스트를 못 만든다.

### 3.2 타입 (crates/oxibrowser-core/src/context.rs, 신규)

```rust
/// 브라우저 내 격리 컨텍스트. Chrome browser context / Playwright BrowserContext 대응.
/// 하나의 Browser 위에 여러 개가共存하며, 각자 jar·storage·지문·egress를 소유한다.
pub struct BrowserContext {
    pub context_id: ContextId,              // "ctx-<n>", CDP browserContextId로 노출
    pub label: Option<String>,              // 디버깅용
    /// 이 컨텍스트가 묶인 계정. None = 익명(기존 동작).
    pub account: Option<AccountHandle>,
    cookie_jar: Arc<RwLock<CookieJar>>,     // 전용 — Browser 글로벌 jar와 분리
    /// 오리진별 localStorage. 기존 Session의 flat 맵을 대체한다.
    local_storage: Arc<RwLock<HashMap<Origin, HashMap<String, String>>>>,
    http_client: HttpClient,                // egress(프록시)가 계정 설정과 함께 고정됨
    fingerprint: FingerprintMeta,           // UA/Client Hints/locale/timezone 잠금값
    network_policy: ContextNetworkPolicy,   // egress 고정, 세션 활성 중 변경 금지 규칙
}

impl Browser {
    /// 기존 new_session()은 "기본 익명 컨텍스트" 위의 설탕으로 재정의 — 공개 API 불변.
    pub fn new_session(&self) -> Result<Session>;
    pub fn new_context(&self, cfg: ContextConfig) -> Result<ContextId>;
    pub fn context(&self, id: &ContextId) -> Option<Arc<BrowserContext>>;
}

impl BrowserContext {
    pub fn new_session(&self) -> Result<Session>;   // Session은 컨텍스트의 jar/storage를 참조
}
```

불변식:

- **jar 소유권 이동**: `Session::new`가 받는 jar 참조의 출처를 Browser에서 BrowserContext로 변경.
  기본(익명) 컨텍스트가 기존 글로벌 jar 역할을 계승하므로 `fetch`/`serve` 기존 동작은 불변.
- **localStorage 오리진 키화**: JS 브리지(`LocalStorageMsg` 채널, `set_page_url_with_storage_seed`)
  의 키를 오리진별로 분리. 이것이 하위 설계 D8의 "씨드가 다음 문서에 몰아주기" 제약의 해소이자
  M6′ 다중 오리진 export의 전제다. 기존 `save-state` 파일(단일 오리진 폴딩)은 import 시 현재
  페이지 오리진으로 매핑되는 기존 동작 유지(호환).
- **지문 잠금**: 컨텍스트 생성 시 `FingerprintMeta`가 고정되고, `js/stealth.rs` 파생값·HTTP
  헤더·Emulation이 여기서 파생된다. 계정 세션이 로드된 컨텍스트에서 지문 변경 시도는 거부
  (fail-closed, §8 FM-L2).
- **egress 고정**: 계정 컨텍스트의 프록시/직접 연결은 계정 레코드의 `egress`에서 온다.
  세션 활성 중 egress 변경은 거부 + 감사. (구현 참고: `HttpClient`는 현재 Browser 1개 소유 —
  컨텍스트별 소유로 확장하되, 프록시 미지정 컨텍스트는 기존 클라이언트를 공유해 비용 최소화.)

### 3.3 CDP 매핑 (oxibrowser-cdp/src/domains/target.rs)

- `Target.createBrowserContext` **실구현**(현재 미구현, `browserContextId` 하드코딩).
  표준 파라미터 + OxiBrowser 확장 파라미터:

```jsonc
// 요청 (Playwright/Puppeteer 표준 호출에는 확장 키가 없음 — 익명 컨텍스트)
{ "method": "Target.createBrowserContext",
  "params": { "oxiAccount": "gh-work", "oxiAgentId": "omp" } }
// 응답
{ "browserContextId": "ctx-3" }
```

- `oxiAccount` 지정 시: AccountManager에 그랜트 확인(M4 PolicyEngine) → 승인이면 계정 세션을
  주입한 컨텍스트 반환, 거부 시 CDP 오류 `accountAccessDenied`{detail: consentRequired}.
  감사 `account_use`가 남는다. **에이전트가 Playwright 표준 API만으로 계정 샌드박스를 얻는
  경로**가 이것이다.
- `Target.createTarget`의 `browserContextId` 파라미터로 컨텍스트에 탭 생성(현재 "default"만
  인식하는 처리 수정).

### 3.4 익명 컨텍스트 호환

- 컨텍스트 미지정 기존 호출 경로(`fetch`, `run`, `session`, `serve` 기본)는 모두 기본 익명
  컨텍스트로 라우팅. 기존 테스트 전부 통과가 M-A 수용 기준.

### 3.5 크레이트·모듈 배치 정정 (하위 설계 §4.3 대비)

| 모듈 | 크레이트 | 비고 |
|---|---|---|
| `context.rs` | core | 신규(M-A) |
| `storage/session_store.rs` (구 M6 `credentials/session_store.rs`) | core | **이동 정정**. AEAD 파일 저장소. 키는 `KeyProvider` 트레잇 주입 |
| `KeyProvider` 트레잇 | core | `fn aead_key(&self, account_id, scope) -> Result<SecretBox>` |
| `KeyringKeyProvider` | credentials | 키체인 구현(하위 설계 §5.1 `_session-keys` 컨벤션 그대로) |
| `account/{mod,registry,detector,probe,orchestrator}.rs` | core | 신규(M-B/M-C). 레지스트리는 비밀 미보유 |
| `credentials` 크레이트(구 M2~M4) | credentials | 그대로: KeyringProvider, TOTP, ConsentStore, PolicyEngine |

의존 방향: `credentials → core`(불변), `cdp → core`(불변, M4 시점에 `+ credentials`),
`cli → {core, credentials}`. 순환 없음.

## 4. Account 레지스트리와 수명 주기 (M-B)

### 4.1 AccountRecord (`~/.oxibrowser/accounts/<account-id>/account.json`)

```jsonc
{
  "version": 1,
  "account_id": "gh-work",              // 슬러그: [a-z0-9-]{1,32}, 사용자 지정
  "scope": "github.com",                // psl 등록 가능 도메인 — 세션 스코프 키
  "display_name": "Garden (work)",      // IdentityCard — 비밀 아님
  "login_hint": "garden@corp.io",       // IdentityCard
  "avatar_url": "https://avatars.githubusercontent.com/u/…",
  "created_at": "2026-09-28T09:00:00Z",
  "state": "valid",                     // §4.2 상태 기계
  "state_detail": null,                 // 예: "probe_401", "challenge:cloudflare:interactive"
  "fingerprint": { /* FingerprintMeta — 이 계정의 가상 기기 */ },
  "egress": { "kind": "fixed", "note": "home-wan" },
  "credentials": ["kch:main/github.com/password/work",
                  "kch:main/github.com/totp/work"],   // 핸들만. 값은 키체인
  "session_summary": {                  // 값 없음: 개수·만료 지평선만
    "updated_at": "2026-09-28T12:00:00Z",
    "cookie_count": 14,
    "earliest_expiry": "2026-10-05T00:00:00Z",
    "origins": ["https://github.com"]
  },
  "probe": { "url": "https://github.com/settings/profile",
             "marker": "meta[name=user-login]" }      // 검증 프로브(선택, 자동 추론 폴백)
}
```

- **다계정 지원**: 같은 scope에 계정 여러 개(gh-work/gh-personal) 가능. 세션 파일 키가
  (account_id, scope)쌍이므로 충돌 없음 — 하위 설계 M6의 `<scope>.session` 단일 키에서 정정.
- `avatar_url`·`display_name`은 로그인 성공 시 DOM에서 1회 캡처(선택). **캡처 실패해도 계정
  기능은 동작** — 카드는 장식이고 보안 판단에 절대 쓰지 않는다(§8: 카드 내용은 페이지가 준 것이므로).
- `grants`는 별도 `consents.jsonl`(M4)이 권위원. account.json에는 미러링하지 않는다(드리프트 방지).

### 4.2 상태 기계

```
needs_login ── begin_login(mode) ──▶ logging_in ── detector 성공 ──▶ valid
needs_login ◀── 실패/타임아웃/abort ── logging_in
valid ── probe 실패(401/로그인 폼 재출현) ──▶ stale
valid ── challenge.rs 분류 Interactive/Blocked ──▶ challenge
stale ── 재검증 성공 또는 재로그인 ──▶ valid | needs_login
challenge ── 인간 상승 완료 ──▶ valid (재캡처 후)
any ── logout(서버측 세션 폐기 시도 + 봉투 삭제) ──▶ needs_login   [자격증명은 유지]
any ── revoke(계정 삭제) ──▶ (레코드·봉투·그랜트·키체인 자격증명 전부 폐기)
```

- 모든 전이는 감사 이벤트(`account_state` 계열)로 기록. `challenge` 진입 시
  `OXI.accountStateChanged` 이벤트 발행 → 에이전트는 재시도하지 않고 상승 요청(연구 02 §3.3
  원칙: 재시도는 리스크 점수만 악화).
- **크래시 세이프티**: 세션 봉투는 로그인 성공 직후·프로브 성공 직후 저장(원자적 치환).
  `Browser::close` 시점 저장에 의존하지 않는다(현재 cookie_file의 결함 — 스카우트 확인).

### 4.3 LoginDetector (`account/detector.rs`)

로그인 성공 판정. 신호(전부 "브로커가 계산한 값" — 페이지 텍스트를 그대로 믿지 않는다, 연구 06 §5.4):

| 신호 | 예 | 신뢰도 |
|---|---|---|
| 쿠키 | 스코프 도메인에 신규 HttpOnly+Secure 세션 쿠키(`user_session`류), `__Host-`/`logged_in`/`remember_*` 패턴 | 높음 |
| 내비게이션 | `/login|/signin|/session|/auth`에서 벗어난 리다이렉트 완료 | 중 |
| DOM | `meta[name=user-login]`(GitHub 실제 존재), 로그아웃 폼(`form[action*="logout"]`), 사용자 메뉴 아바타 | 중 |
| 저장소 | localStorage에 토큰성 키 신규 등장 | 중 |
| 명시 | user-mirror/위저드 흐름에서 사용자·호스트의 `done`/`OXI.reportLoginSuccess` | **최상** |

- 판정: 명시 신호 > 쿠키+DOM 조합 임계치. 미달이면 `logging_in` 유지 — 사용자가 위저드에서
  `done`으로 확정 가능(거짓 음성 폴백).
- **거짓 양성 방어**: detector 성공만으로 `valid` 확정하지 않고 저장 직전 ValidationProbe를 1회
  통과해야 한다(§4.4). 미통과 시 `stale`로 저장하고 상세 사유 기록.

### 4.4 ValidationProbe (`account/probe.rs`)

- `probe.url`+`marker` 지정 시: GET 후 마커 존재·인증 페이지 아님 확인.
- 미지정 시 폴백: 스코프 루트 GET → (a) 로그인 폼 부재 + (b)detector가 발견한 쿠키 신호 잔존
  확인. 챌린지 분류(challenge.rs) 결과도 반환.
- 비용 주의: 프로브는 `account status`·에이전트 작업 시작·주기(기본 꺼짐)에만. 자동 주기
  재검증은 v1 비목표(§8 FM-L6).

## 5. 로그인 오케스트레이션 (M-C/M-D)

### 5.1 네 가지 부트스트랩 경로

| 경로 | 사용자 | 흐름 | 마일스톤 |
|---|---|---|---|
| **import** | 이미 로그인된 상태 보유 | Playwright `storageState` JSON 또는 Netscape `cookies.txt` → 컨텍스트 주입 → 프로브 → 저장. 현재 지문을 baseline으로 채택(타 기기 캡처 상태의 위험은 문서화, §10 FM-L4) | M-C |
| **user-mirror** | 호스트 앱(Codex Desktop형 UI) | `oxibrowser account login <id> --json` → serve 기동 → stdout에 `{ws_url, viewer_token, login_id}` → 호스트가 **viewer 역할**로 접속해 페이지를 미러(screencast) → 사용자가 직접 로그인 → detector → 저장 | M-C |
| **wizard(REPL takeover)** | 터미널 사용자 | `oxibrowser account login <id>` → 내장 위저드: `goto/type/click/screenshot(파일)` 축소 REPL — 이 구간 에이전트 명령은 차단(M5′) → `done`/`abort` | M-C |
| **agent auto-login** | 무인 | 그랜트된 에이전트가 `OXI.loginWithAccount` 요청 → PolicyEngine 승인 → 브로커가 폼 주입(M2 비밀) + TOTP(M3) → detector → 저장. SMS/이메일 2FA·Interactive 챌린지는 상승으로 중단 | M-D |

### 5.2 viewer 역할 — 내장 브라우저 계약 (Codex Desktop 부분)

- CDP WebSocket 업그레이드 시 확장 헤더로 역할 주장:
  - `X-Oxi-Role: agent`(기본) — 전체 자동화. 단 로그인 창(takeover) 중 입력·캡처 거부.
  - `X-Oxi-Role: viewer` + `X-Oxi-Viewer-Token: <one-time>` — screencast/입력/navigate만 허용,
    쿠키·storage·평가·네트워크 로그 접근 거부. 토큰은 `OXI.beginLogin`(또는 CLI `account login`)
  이 발급하고 **out-of-band로 사용자·호스트에게만** 전달(CLI stdout JSON, 터미널 출력).
- 로그인 창(takeover window) 동안:
  - viewer: screencast·입력 허용(사용자 눈과 손).
  - agent: `Input.*` 거부(`input_denied_during_takeover`), `Page.captureScreenshot`/screencast
    거부(M0.2 가드와 동일 관문 — 하지만 viewer는 통과). 이 구분이 하위 설계 M5 "구간 캡처 중단"의
    정정점: **캡처는 역할별로** 차단된다.
  - wizard의 `screenshot` 명령은 예외: 사용자 본인 채널이므로 0600 파일로 저장 + 감사. 결과물이
    에이전트 컨텍스트로 흘러들어가지 않는다(파일 경로만 출력, REPL 응답에는 미포함).
- **경계의 정직한 정위**: 역할 게이팅은 정직한 에이전트(프롬프트 인젝션당한)에 대한 안전망이다.
  악성 로컬 프로세스는 두 번째 연결에서 viewer를 사칭할 수 없게 토큰을 요구하지만, CDP 클라이언트
  전제(연구 06 §8: CDP 접근 = 브라우저 침해)는 그대로 — 하드 경계는 loopback 기본 바인딩 +
  `--auth-token`(serve에 이미 존재) + AEAD + 키체인 + 감사다(§8).

### 5.3 agent 자동 로그인 (M-D)

```
OXI.loginWithAccount {accountId, agentId}
  → PolicyEngine.authorize_use(action=login)        (deny → 그랜트 → confirmation)
  → 컨텍스트에 스코프jar 적재(세션 있으면 그것부터 — stale이면 재로그인)
  → 폼 탐지(DomSnapshot) → OriginPolicy.evaluate(프레임 오리진 포함, M1)
  → KeyringProvider.resolve (감사 credential_read) → 주입 (값은 응답·로그 불가)
  → TOTP 필요 시 TotpGenerator (M3) → 재감사 credential_use
  → LoginDetector → 저장 → OXI.loginStateChanged {state:"valid"}
  실패 분기: challenge Interactive/Blocked → 계정 상태 challenge + 상승 이벤트
             SMS/이메일 2FA 감지 → 즉시 중단 + 상승 (금지 목록 유지)
             리다이렉트가 allowed_origins 이탈 → 자격증명 언로드 (M1 RedirectVerdict)
```

- HTTP Basic 401(`Fetch.authRequired`) 대응: v1 범위 밖. 하위 과제로 기록만(§9 M-D stretch).

### 5.4 GitHub 종단간 예시

```
# 1회 사용자 로그인 (호스트 앱 없이 터미널):
$ oxibrowser account add --site github.com --id gh-work --login garden@corp.io
$ oxibrowser account login gh-work
  wizard> goto https://github.com/login
  wizard> type #login_field garden@corp.io        # 값은 위저드가 직접 받음(에이전트 경유 아님)
  wizard> type #password …
  wizard> totp                                     # 키체인에 otpauth 있으면 브로커가 채움
  [detector] meta[name=user-login] + user_session(HttpOnly) → 저장
  wizard> save-credential                          # (선택) 비밀번호+otpauth를 키체인에 — 향후 무인
  state: valid

# 이후 에이전트 (Playwright 표준 경로):
browserContext = cdp.send("Target.createBrowserContext",
                          {oxiAccount: "gh-work", oxiAgentId: "omp"})   # 그랜트 확인·감사
page = newTarget(browserContext) → github.com 접속 → 로그인 유지
cdp.send("Network.getCookies")  →  deniedInAccountMode (오류)

# 세션 만료:
OXI.accountStateChanged {state:"stale"} →
  그랜트된 에이전트: OXI.loginWithAccount("gh-work") → 브로커+TOTP 무인 재로그인
```

## 6. 저장 포맷·디렉터리·보안 등급

### 6.1 디렉터리 레이아웃

```
~/.oxibrowser/                       0700
├── audit.jsonl                      (M0.4, 기존)
├── consents.jsonl                   (M4 — account 그랜트 포함으로 확장)
└── accounts/                        0700
    └── gh-work/                     0700
        ├── account.json             0600 — 메타데이터(비밀 아님, 존재 자체가 민감)
        └── sessions/                0700
            └── github.com.session   0600 — OXSESS1 AEAD 봉투(하위 설계 §5.3 그대로)
```

- 봉투 바이너리 포맷·AEAD(XChaCha20-Poly1305)·원자적 치환·키체인 키(`_session-keys`)는 하위
  설계 M6 규격 그대로. **정정 2건**: (a) 저장 키가 `sessions/<scope>.session` →
  `accounts/<account-id>/sessions/<scope>.session`, (b) 모듈 위치 credentials → core(§3.5).
- 키체인 부재 환경: 평문 폴백 금지, `CredError::KeyStoreUnavailable`(FM-6 승계).

### 6.2 데이터 보안 등급표

| 데이터 | 위치 | 보호 | 에이전트 노출 |
|---|---|---|---|
| 비밀번호·TOTP 시크릿·API 키 | OS 키체인 | 키체인 ACL | **절대 불가** |
| 세션 봉투(쿠키+localStorage 원문) | AEAD 파일 | 키체인 키 | **절대 불가** — 주입만 |
| AEAD 키 | OS 키체인 | 키체인 | **절대 불가** |
| account.json(계정 목록·힌트·카드) | 로컬 파일 | 0600 | **허용**(accountList 메타데이터) |
| 그랜트 기록 | consents.jsonl | 로컬 | 요약만(본인 agent_id 분) |
| 감사 로그 | audit.jsonl | 로컬 append | 핸들+해시만(값 없음, 기존 원칙) |
| 인증된 페이지 콘텐츠 | 메모리 | — | 그랜트의 `read` 범위 내 허용 |

### 6.3 CDP 게이팅 (account 컨텍스트에서)

- `Network.getAllCookies`/`getCookies`/`setCookie`/`deleteCookies`, `OXI.exportStorageState` →
  `deniedInAccountMode`(하위 설계 §6.3 "credential 모드"의 계정 확장). `--json` 덤프·HAR도
  동일(리랙션은 유지되지만 원천 차단이 원칙).
- 예외 경로: 명시적 CLI `account export-state <id>`(호출자=로컬 사용자) — 감사
  `sensitive_action` + 경고. CDP로는 노출하지 않는다.

### 6.4 감사 이벤트 확장

`AuditEventKind` 추가(스키마만 신규, 하위 설계 §4.1에 병합):

- `AccountState { from, to, detail }` — 수명 전이 전체
- `AccountUse` — 컨텍스트 바인딩·그랜트 소비(agent_id 포함)
- `SessionCapture` / `SessionRestore` / `SessionDiscard` — 봉투 저장·주입·폐기(값 없음)

## 7. 표면: CLI · session REPL · CDP OXI · MCP · skill

### 7.1 CLI (`crates/oxibrowser/src/main.rs`)

| 명령 | 인자 | 비고 |
|---|---|---|
| `account add` | `--site <도메인> [--id <슬러그>] [--login <힌트>] [--display <이름>]` | 레코드 생성, state=needs_login. `--id` 생략 시 scope에서 유도(충돌 시 `-2`) |
| `account list` | `[--json]` | 상태 보드: id/scope/state/login_hint/updated_at. Codex "계정 관리자"의 CLI판 |
| `account login` | `<id> [--mode user\|agent\|import] [--storage-state F] [--cookies F] [--json] [--timeout <SEC>]` | §5.1. `--json`이면 호스트 계약(stdout에 ws_url+viewer_token, 완료까지 블록) |
| `account status` | `<id> [--probe]` | `--probe` 시 ValidationProbe 실행 후 갱신 |
| `account logout` | `<id> [--remote]` | 봉투 삭제(+그랜트 무효화). `--remote`는 서버측 로그아웃 페이지 시도(best-effort) |
| `account grant` | `<id> --agent <A> --actions <목록> [--ttl <SEC>] [--max-uses <N>]` | M4 ConsentStore에 account 그랜트 기록 |
| `account revoke` | `<id> --agent <A>` | 그랜트 폐기 톰스톤 |
| `account export-state` | `<id> [--out F]` | Playwright JSON으로 내보내기 — 감사+경고(§6.3) |
| `account rm` | `<id>` | 전면 폐기: 봉투·그랜트·키체인 자격증명(`service prefix` 매칭 일괄) |
| `fetch`/`serve` 플래그 | `--account <id>[,…]` | 해당 계정 컨텍스트로 바인딩. serve는 로그인 창·viewer도 함께 서빙 가능 |

### 7.2 session REPL (`session/parser.rs` — 현재 25 variants에 추가)

| 명령 | 응답 |
|---|---|
| `account_list` | `{accounts: [AccountSummary…]}` |
| `account_status <id> [--probe]` | `{state, detail, session_summary}` |
| `account_login <id> [--mode …]` | M-C. REPL 세션 위에서 위저드/에이전트 로그인 개시 |
| `account_logout <id>` | `{state: "needs_login"}` |
| `takeover [timeout_sec]` | M5′ — 역할 인지 버전(§5.2) |
| `store_save`/`store_load` | 기존 유지(저수준) — account 흐름의 내부 구성요소 |

### 7.3 CDP OXI (`domains/oxi.rs`)

하위 설계 §6.3 표(credentialList/fillCredential/confirmationRequired/resolveConfirmation) 그대로
+ 추가:

| 이름 | 종류 | 파라미터 | 응답/페이로드 |
|---|---|---|---|
| `OXI.accountList` | command | `{agent?: string}` | `{accounts: [AccountSummary]}` — 메타·상태만 |
| `OXI.beginLogin` | command | `{accountId, mode: "user"\|"agent"}` | `{loginId, viewerToken?, timeoutMs}` — user 모드만 viewerToken |
| `OXI.endLogin` | command | `{loginId, outcome: "done"\|"abort"}` | 사용자/호스트 명시 종료 |
| `OXI.reportLoginSuccess` | command | `{loginId}` | 명시 신호(§4.3 최상 신뢰) |
| `OXI.loginWithAccount` | command | `{accountId, agentId}` | M-D 자동 로그인 개시(비동기, 결과는 이벤트) |
| `OXI.accountStateChanged` | **event** | — | `{accountId, state, detail}` — stale/challenge 시 에이전트가 재시작 아닌 상승 판단 |
| `OXI.loginStateChanged` | **event** | — | `{loginId, accountId, state}` |

- `Target.createBrowserContext` 확장 파라미터 `oxiAccount`/`oxiAgentId`(§3.3)가 Playwright
  사용자의 주 경로.

### 7.4 MCP · skill

- `mcp.rs` 도구 추가: `account_list`, `account_status`, `login_request`(상승 이벤트를 MCP
  알림으로) — M9와 함께.
- `skills/oxibrowser-account/SKILL.md`: "로그인이 필요하면 account_list → loginWithAccount →
  accountStateChanged 대기, challenge면 사용자 상승" 절차 프롬프트 + `requires:
  {credentials: true}` 매니페스트(M9).

## 8. 보안 모델 — 위협과 경계

### 8.1 위협-대응표 (연구 06 위협 모델 승계 + 계정 신규)

| 위협 | 대응 | 근거/비고 |
|---|---|---|
| 프롬프트 인젝션: 인증된 페이지가 에이전트에게 유출 지시 | 쿠키·봉투는 CDP로 꺼낼 수 없(§6.3). 데이터 유출 자체는 그랜트의 `read` 범위 내 허용 — 인증 사이트를 읽는 것이 기능이므로. 되돌릴 수 없는 행위는 별도 확인 | 연구 06 §2; §8.3 |
| 인젝션이 로그인 폼에 악성 오리진 주입 | OriginPolicy 프레임 오리진 fail-closed(M1 구현됨) + allowed_origins exact | §하위 설계 FM-2 |
| 악성 페이지가 사용자 미러 사칭(가짜 "로그인됨") | detector는 브로커 계산 신호만; 카드·확인 표시도 브로커 값만(IdentityCard 내용은 판단에 불사용) | 연구 06 §5.4 |
| 역할 사칭(viewer 탈취) | one-time viewerToken out-of-band. 정직한 에이전트 안전망 + 하드 경계는 loopback/auth-token | §5.2 |
| 세션 파일 탈취 | AEAD + 키체인 키, 평문 폴백 금지 | §6.1, FM-6 |
| 계정 간 쿠키 블리딩 | 컨텍스트별 전용 jar(§3) — 구조적 차단 | 스카우트 격차 해소 |
| 같은 계정, 병렬 에이전트 상태 충돌 | 봉투 저장은 원자적 치환(last-wins) + 감사. 동시 실행 잠금은 M8b(advisory lock) | §10 FM-L7 |
| 승인 피로 | deny → 그랜트 → confirmation 순서, 타임아웃=거부, 읽기는 무프롬프트 | FM-3 승계 |

### 8.2 action 클래스(그랜트 단위)

`navigate`(이동·읽기) / `interact`(폼·클릭 등 일반 상호작용) / `irreversible`(결제·삭제·발송·
권한 변경). 기본 그랜트 = scope 내 `navigate`+`interact`. `irreversible`은 별도 그랜트 또는
confirmation 카드(패턴: 결제 폼·`delete`·`settings` 경로 등 — **완전할 수 없음을 문서화**,
FM-L8). 로그인 자체는 `login` action(M4 체계 승계).

### 8.3 유지 금지 목록 (하위 설계 §2.2의 7건 그대로)

cf_clearance 재생 의존, 일상 Chrome 프로파일 attach, SMS/이메일 2FA 자동화, iCloud 패스키 외부
사용, iCloud 우회, LLM 프롬프트 승인, 유사도 기반 자격증명 매칭 — 계정 계층에서도 전부 금지.
단 clearance 쿠키은 세션 봉투에 **포함되되** 30분 통로 취급(만료 임박 시 `challenge` 상태).

### 8.4 샌드박스 약속 요약 (에이전트 관점)

- **받는 것**: 계정 목록·상태, IdentityCard(표시 정보), 그랜트된 계정으로 실행되는 격리
  컨텍스트(로그인 유지), 상태 변경 이벤트, 상승 요청 채널.
- **절대 못 받는 것**(게이트로 차단): CDP/CLI 경유 쿠키·스토리지 덤프(credential 모드 게이트), localStorage 원문 일괄 반출, 비밀번호, TOTP 시크릿·코드, AEAD 키,
  봉투 파일 경로·평문, 로그인 창의 화면. **브라우저 의미론상 예외(2026-09-29 정정)**: 페이지 JS가
  원래 도달 가능한 자산(non-`HttpOnly` 쿠키, 자기 오리진 localStorage)은 에이전트가 페이지를
  구동하는 한 `document.cookie` 등으로 읽을 수 있다 — 이는 실제 브라우저와 동일한 한계로,
  `read` 그랜트의 범위에 귀속시켜 문서화한다. `HttpOnly` 세션 쿠키는 JS 비가시 상태로 유지되고
  일괄 반출 경로는 전부 게이트된다.
- **요구되는 것**: agentId 자발 선언, 그랜트 내 행동, irreversible 확인 응답, challenge 시
  재시도 금지.

## 9. 마일스톤 정정·PR 분할

하위 설계 §8 표에서 **정정되는 행**: M5(takeover — 역할 인지로 확장), M6(세션 저장소 — core
이동 + (account_id, scope) 키), M8(2층 프로파일 — M-A 위에 구축). **신규**: M-A~M-D. 나머지
(M2/M3/M4/M6b/M7/M9)는 그대로.

| PR | 제목 | 범위 | 의존 | 수용 기준 |
|---|---|---|---|---|
| **M-A** | `core: browser contexts — per-context jar, origin-keyed storage, CDP createBrowserContext` | `context.rs`, Session jar/storage 출처 변경, `Target.createBrowserContext` 실구현(익명), `browserContextId` 실값, localStorage 오리진 키화 | — | 기존 워크스페이스 테스트 전부 통과(기본 경로 불변), 컨텍스트 2개 쿠키 격리 테스트, 오리진별 localStorage 왕복, Playwright `browser.newContext()` 동작 확인 |
| M2 | (불변) keyring credential provider | — | M0.4 | 하위 설계 그대로 |
| M3 | (불변) TOTP | — | M2 | 〃 |
| M4 | (불변) consent + policy engine + CDP 게이트 | ConsentRecord에 account 그랜트 주체 추가(`subject: account`) | M1, M2 | 〃 + account 그랜트 만료·횟수 테스트 |
| **M6′** | `core: encrypted session store (account-keyed)` | `storage/session_store.rs`(이동+재키), 키체인 키, 다중 오리진 export(`Session::export_state` 확장), 원자적 치환, 크래시 시점 저장 | M-A | 암호화 왕복, 지문 불일치 거부, (account, scope) 다계정共存, 다중 오리진 주입(pending_seed 오리진 매칭) |
| **M-B** | `core: account registry + lifecycle + status` | `account/{registry,detector,probe}.rs`, account.json, 상태 기계, 감사 확장, CLI `account add/list/status/rm`, `OXI.accountList` | M6′ | 상태 전이 테스트, detector 단위(신호 조합·거짓 양성 프로브 방어), 프로브 폴백 |
| **M-C** | `login orchestration v1 — import + user-mirror + wizard` | `account/orchestrator.rs`, viewer 역할/토큰(cdp server.rs), takeover 역할 인지(M5′ 흡수), CLI `account login/logout/export-state`, `OXI.beginLogin/endLogin/reportLoginSuccess` + 이벤트, `--account` 플래그 | M-B | GitHub 실사이트 수동 시나리오(위저드로 로그인→저장→재사용→stale 전이), viewer 토큰 없는 사칭 연결 거부, takeover 중 agent 입력·캡처 거부·viewer 허용, import 경로 storageState 왕복 |
| **M-D** | `agent auto-login` | `OXI.loginWithAccount`, 브로커 폼 주입 연결, TOTP, 리다이렉트 무효화 연결, challenge→상승 이벤트, `Target.createBrowserContext {oxiAccount}` 그랜트 게이트 | M-C, M2, M3, M4 | 무인 로그인 성공 시나리오(로컬 테스트 사이트), deniedInAccountMode 게이트, 그랜트 없는 createBrowserContext 거부 |
| M6b/M7/M9 | (불변; M9는 §7.4 스킬 내용으로) | — | — | — |
| **M8b** | `profiles: two-layer + advisory lock` (구 M8) | 영구 프로파일 + persist:false 오버레이 + 잠금 | M-A | 하위 설계 그대로 |

권장 순서: **M-A 먼저**(순수 core, 의존 없음, 이후 전부의 지형 변형) → M2 → M3 → M4 → M6′ →
M-B → M-C → M-D. M-C의 호스트 계약(`--json` stdout)은 Codex Desktop형 앱이 붙는 계약 지점이므로
변경 시 문서 갱신 필수.

### 구현 상태 (2026-09-28)

| PR | 상태 | 비고 |
|---|---|---|
| M-A | ✅ 구현+리뷰 완료 | `core/src/context.rs` 신설(ContextId/ContextConfig/BrowserContext — 전용 jar·오리진 키 localStorage·http_client), `browser.rs` default_context+레지스트리+new_context/dispose_context/new_session_in/new_tab_in, `session.rs` Session::new(Arc<BrowserContext>)·오리진 버킷 get/set/import·close 시 공유 스토리지 보존, `js/runtime.rs` 매 내비게이션 버킷 재시드. CDP `Target.createBrowserContext/disposeBrowserContext` 실구현 + 전 이벤트 실제 ctx id + `createTarget(browserContextId)`·`proxyServer` 매핑·`oxiAccount` 정직 거부(-32000). 리뷰 4건 수정: 컨텍스트 HttpClient를 항상 자기 jar에 결합(HTTP 경로 블리딩 제거), LocalStorageMsg 오리진 스탬프+내비게이션 전 Drain 배리어(경쟁 제거), dispose_context jar 클리어+감사, 무효 proxyServer 거부. 검증: 워크스페이스 775 테스트 통과, 실제 serve+raw WS 스모크 PASS(실HTTP Set-Cookie 컨텍스트 격리·레이스 생존·재사용 차단). 잔여: named context는 휘발성(M-B+ 저장소 대기). opaque origin "null" 버킷 공유는 v0.24.1에서 URL 키잍으로 해소, 복원 세션 누수·키체인 논블로킹화 동시 수정 |
| M-B | ✅ 구현 완료 (2026-09-28) | `core/src/account/{mod,record,registry,manager,detector,probe}.rs` 신설 — AccountRecord(§4.1 스키마, 슬러그·스코프 검증, flat identity 카드), AccountRegistry(계정당 0700 디렉터리·0600 원자적 account.json·sessions/에 SessionStore 재사용·§4.2 전이 기계 강제), AccountManager(capture/restore/mark_stale/logout/verify_with_probe — 지문 불일치 기본 거부, 전 전이 감사 `session_capture`/`session_restore`/`session_discard`/`account_state`), LoginDetector(§4.3 신호 — 신규 HttpOnly+Secure 세션 쿠키·auth 경로 이탈·DOM 마커·신규 토큰성 localStorage·명시 신호, high+점수 ≥6 조합 판정, baseline 스냅샷), ValidationProbe(§4.4 — 마커 CSS-ish 셀렉터/리터럴 매칭, 로그인 폼 부재, 챌린지 분류, 도달 불가 시 상태 불변), 감사 `AuditEventKind` 5종 확장(account_state/account_use/session_capture/session_restore/session_discard). CLI `account add/list/status(--probe)/rm` + `credential put(stdin·prompt 전용)/get(--field, --json 불가)/list/rm/totp/onboard` — KeyringProvider·PolicyEngine·ConsentStore 실연결(로컬 사용자 자가 확인), 세션 AEAD 키는 키체인 `_session-keys/<scope>` create-once(M-C에서 credentials 크레이트 `KeyringKeyProvider`로 흡수 완료 §3.5). 테스트: 레지스트리 왕복·권한(0600/0700)·원자성, 상태 전이·감사 JSONL, detector 신호 조합, capture/restore StaticKeyProvider 실왕복, 프로브 wiremock HTTP, CLI 통합(격리 HOME). |
| M2/M3/M4 | ✅ 구현 완료 (2026-09-29) | 신규 크레이트 `oxibrowser-credentials`: SecretBox(compile-fail 증명), KeyringProvider(§5.1 서비스 키), InMemoryProvider, TotpGenerator(RFC 6238 벡터·창 경계), ConsentStore(last-wins·톰스톤·만료/횟수·credential+account 이중 평면), PolicyEngine(deny→동의→confirmation, 해시 바인딩 토큰). CDP 표면: OXI.credentialList/fillCredential/confirmationRequired/resolveConfirmation + credential_mode 게이트(쿠키 4종·exportStorageState). 정정: account 그랜트는 **agent-스코프**(ConsentSubject::Account{account, agent}) — §1 정의 준수. |
| M6′ | ✅ 구현 완료 (2026-09-29) | `core/storage/session_store.rs` — OXSESS1 XChaCha20-Poly1305 봉투, 원자적 0600 치환, 지문 fail-closed, KeyProvider 트레이트(+KeyringKeyProvider), 다중 오리진 `export_state_for_scope`. |
| M-C | ✅ 구현 완료 (2026-09-29) | LoginOrchestrator(begin/complete/end/abort/timeout·원타임 viewer 토큰·이벤트), import 경로(storageState·Netscape cookies.txt→프로브→캡처), CLI `account login/logout/export-state` + `--account`(fetch/serve)·위저드·호스트 계약(--json stdout), viewer 역할(X-Oxi-Role/토큰·takeover 게이트), OXI account/login 명령·이벤트, REPL account_*·takeover. |
| M-D | ✅ 구현 완료 (2026-09-29) | `core/account/agent_login.rs`(엔진+CredentialSource 포트, 후보 탐색이 자격증명 pinned origin 우선), OXI.loginWithAccount, `Target.createBrowserContext {oxiAccount}` 그랜트 게이트+세션 복원+credential_mode, SMS/이메일 2FA·Interactive 챌린지 즉시 상승, 리다이렉트 허용목록 거부권. CLI/REPL `--mode agent`. |
| 배포 리뷰 | ✅ 완료 (2026-09-29) | 보안 리뷰 5건(중요 2·중간 3): 4건 코드 수정(confirmation viewer-전용 승인·beginLogin 토큰 in-band 제거·takeover 차단 확장(fillRef/clickRef/boxScreenshot/Runtime.evaluate)·fillRef/REPL/MCP 비밀번호 리터럴 거부+importStorageState 게이트), 1건(document.cookie 경유 non-HttpOnly 쿠키 가시성)은 브라우저 의미론상 구조적 — §8.4 문구 정정으로 반영. |

M-D stretch(별도 PR, 우선순위 낮음): `Fetch.authRequired` 이벤트 + `Fetch.continueWithAuth`를
브로커 자격증명과 연결(HTTP Basic 사이트 무인화).

## 10. 실패 모드

- **FM-L1 역할 게이팅은 소프트 경계** — 정직한 에이전트 안전망(§5.2). 로컬 악성 프로세스는
  out-of-band 토큰·loopback·auth-token로만 막는다. 한계 문서화.
- **FM-L2 지문/egress 불일치** — 계정 컨텍스트에서 지문 변경 시도 거부(fail-closed). 단 이것도
  사전 점검일 뿐(하위 설계 FM-5 승계): 챌린지 루프를 예방하지 못할 수 있음.
- **FM-L3 detector 거짓 양성** — 저장 전 프로브 필수(§4.3). 프로브 불가 사이트는 명시 `done`
  또는 import 검증으로만 저장.
- **FM-L4 import의 기기 불일치** — 타 기기에서 캡처한 storageState는 지문·egress 연속성 없음.
  현재 지문을 baseline으로 채택하되 CF급 보호 사이트는 거부될 수 있음을 login 출력에 경고.
- **FM-L5 IndexedDB 인증 사이트** — 미지원(하위 설계 비목표 승계). 프로브는 통과하는데 로그인이
  풀리는 사이트 패턴 → `state_detail: "unsupported_storage"`로 기록하고 안내. 프로파일 지속(M8b)
  이 근본 해법.
- **FM-L6 프로브 비용·부작용** — 자동 주기 재검증 없음(기본). 프로브 자체가 리스크 엔진에
  신호가 될 수 있어 최소 빈도 원칙.
- **FM-L7 병렬 세션 봉투 충돌** — last-wins + 감사. 한 계정의 동시 자동화는 M8b 잠금까지
  미지원(문서화). 서로 다른 계정·컨텍스트는 격리되어 영향 없음.
- **FM-L8 irreversible 패턴 인식의 불완전성** — 패턴 목록은 최선 노력. 그랜트 기본값이
  `navigate`+`interactive`임을 문서화; 민감 계정은 `--actions navigate`로 축소 권장.
- **FM-L9 크래시 윈도우** — 로그인 성공~저장 사이 크래시 = 세션 상실(재로그인 필요). 값 유출은
  없음(메모리 내 jar는 소멸). 허용 가능한 지연 가능성으로 기록.
- **FM-6/7/8 승계** — 키체인 키 손실=봉투 복구 불가, ACL 재빌드 프롬프트, 감사 로그 변조 가능성
  (로컬 침해자) — 하위 설계 그대로.

## 11. 의존성

하위 설계 §10 표에서 변경 없음. 본 설계가 추가하는 신규 외부 의존은 **0** —

- AEAD(`chacha20poly1305`), 키체인(`keyring` 4.x + `apple-native-keyring-store`),
  zeroize, sha2, totp-rs, getrandom: 전부 하위 설계 도입 예정 목록 그대로(M-A 자체는 의존 추가
  없음 — 순수 core 재구성).
- psl(스코프 계산), serde_json: 기존 워크스페이스 의존.

---

재검증일: 2026-09-28. 코드 사실(글로벌 jar, flat localStorage, browserContextId 하드코딩,
지속성 부재, Fetch.authRequired 부재, Storage 도메인 부재)은 스카우트 전수 확인 + targeted grep.
