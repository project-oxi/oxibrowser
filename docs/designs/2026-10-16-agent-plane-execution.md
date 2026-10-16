# Agent plane 실행 설계 (2026-10-16)

> [2026-10-16-agent-first-browser.md](2026-10-16-agent-first-browser.md)의 단계(P0–P3)를
> 코드 수준으로 전개한 실행 계획. 모든 착지점은 2026-10-16 스카우트 보고(실제 심볼 기준)에
> 근거한다. 시간 추정은 의도적으로 없음 — 의존 순서와 완료 기준만.

## 0. 조사로 확정된 사실 (계획의 전제)

| 설계 필요사항 | 이미 있는 구현 | 갭 |
|---|---|---|
| ariaSnapshot 관측 | `DomSnapshot::render_aria_yaml` (core/js/dom_snapshot.rs:2271), CDP `oxi.rs aria_snapshot()` | CDP 밖 노출 + 토큰 예산 |
| ref epoch 무효화 | `oxibrowser-cdp/src/refs.rs` RefRegistry + `oxi.rs resolve_and_validate` (generation·fingerprint·재질의) | 에이전트 모듈에서 재사용 |
| 불가역/consent 게이트 | `oxi.rs:299 irreversible_gate` → `account/irreversible.rs matched_pattern` + `LoginSurface::irreversible_allowed` | REPL 경로에 동일 강제 |
| 비밀 리터럴 거부 | `OXI.fillRef` → `selector_targets_password` → `passwordFillRequiresCredential` (oxi.rs:486); `fillCredential` 존재 | 로케이터 fill에 동일 규칙 |
| 이벤트 구독 | `Session::set_event_sink(CoreEvent)` (session.rs:1899) — Dialog/Download/Console/Fetch 등 | REPL 이벤트 큐로 연결 |
| 계정/로그인 오케스트레이션 | `AccountRuntime`/`LoginOrchestrator`, `AgentLoginEngine`(폼 직접 POST, 비밀 JS 리터럴화 없음) | REPL `page.login()` 랩 |
| 뷰포트 | `set_viewport_override(w,h)` 프로세스 전역 (session.rs:313), 기본 1280×720 | 1440×900 에이전트 고정 |
| boa 컨텍스트 패턴 | `JsRuntime` 전용 스레드 + `JsCommand` mpsc + fetch/localStorage 브리지(스레드 로컬 + 응답 펌프) | agent-world 별도 스레드가 동일 이디엄 |

알려진 구멍: 자발적 팝업(링크 → 새 탭)은 코어 이벤트가 없음(target.rs:133 — 명시적
생성만 이벤트). M2에서 TabManager 폴링으로 보완하고 `CoreEvent::TabSpawned`는 별도 PR.

## 1. 구조 결정

- **에이전트 모듈 위치**: `crates/oxibrowser/src/agent/` (bin 크레이트). session/·mcp/와 동일
  패턴. `oxibrowser-cdp`를 라이브러리로 의존해 `refs`·게이트 헬퍼를 재사용한다(지금
  core로 추출하는 리팩터는 하지 않는다 — cdp 크레이트 테스트 75개를 흔들 이유가 없다.
  oxios 임베드 시점에 `oxibrowser-agent` 승격과 함께 재검토).
- **agent-world 실행 컨테이너**: 페이지 `JsRuntime`과 **별도 스레드 + 별도 boa `Context`**.
  페이지 런타임은 전혀 손대지 않는다(페이지 호환 리스크 0). 호스트 바인딩(`page`, `oxi`)
  은 native fn → mpsc 커맨드 → 비동기 쪽에서 `Arc<RwLock<Session>>` 획득 → oneshot 응답.
  REPL 스레드는 어떤 공유 락도 직접 잡지 않는다(교착 원천 차단). fetch 브리지
  (`PENDING_FETCH` + 응답 펌프)가 이 블로킹 패턴의 선례다.
- **CLI 표면**: `oxibrowser repl`(신규, JS REPL), `oxibrowser agent run|resume`(M3). 기존
  `session`(JSON 명령 REPL)은 그대로 — 관객이 다르다(스크립트/파이프라인 vs 모델).
- **bash 스코프(설계 문서 §8-1 결정)**: 기본 `none`. `--shell workspace` 옵트인 시
  작업 디렉터리(작업별 `~/.local/state/oxibrowser/tasks/<id>/ws`)로 제한. 그 밖은
  irreversible 게이트와 같은 consent 카드로 승격. `--shell full`은 만들지 않는다( 필요하면
  그때 다시 결정).
- **레인(§8-4 결정)**: `--trust full|restricted` 단일 플래그. `restricted`는 REPL 없이
  기존 MCP 관측/액션 도구만 = 사실상 오늘의 ghost 레인. ghost 바이너리 경로는 유지.

## 2. 마일스톤

### M0 — 관측 기반 (P0 전반)

새 파일 `crates/oxibrowser/src/agent/mod.rs`, `agent/obs.rs`.

- `observe(session, ObsOpts{budget_bytes, max_actionable}) -> ObsResult{ aria_yaml, actionable: Vec<{ref, role, name, kind}>, url, title, generation, truncated, omitted }`
  - `Session::dom_snapshot()` → `render_aria_yaml`(ref 클로저는 cdp `RefRegistry::allocate` 재사용).
  - 예산 초과 시 트리 하단 절단 + `truncated` 보고(ghost 스냅샷의 8,000B 캡과 같은 규칙).
- session REPL에 `observe <tab> [--budget N]` 명령 추가(parser.rs/executor.rs — 기존 27개 명령 패턴).
- 토큰 계산은 바이트 근사(4B/token)로 시작. 정확도는 코퍼스 평가(M5)에서 교정.

완료 기준: `printf 'new\ngoto t1 https://example.com\nobserve t1 --budget 1500\nexit\n' | cargo run -- session --json`이 1,500토큰 이하 출력 + ref 포함. 예산 절단·ref 무효화(재네비게이션 후 stale) 단위 테스트.

### M1 — Oxiwright REPL (P0 후반)

새 파일 `agent/world.rs`(boa 컨텍스트+스레드), `agent/bridge.rs`(커맨드 mpsc + oneshot), `agent/locators.rs`.

- 로케이터: `page.getByRole(name), getByText, getByLabel, getByPlaceholder, locator(css), .first, .nth(i)` — 전부 최신 스냅샷 질의로 ref 발급 후 `resolve_and_validate` 재검증. 액션: `.click(), .fill(text), .press(key), .check()`. 탐색: `page.goto/back/forward/reload`, `page.content()`, `page.ariaSnapshot()`, `page.screenshot()`.
- 게이트 강제(전부 비동기 측 세션 락 안에서):
  - `.click()` → `irreversible_gate` 동일 조건(패턴 매치 + credential_mode + grant) 실패 시 `irreversibleActionRequiresGrant` — consent 흐름은 M3.
  - `.fill()` 대상이 password → `selector_targets_password` 참이면 거부 + 안내: `oxi.fillCredential(locator, 'account-id')` → BrokerSource 해결 → 네이티브 주입. 모델 컨텍스트에 비밀 미경유.
- 다이얼로그: 액션 중 `DialogGate` 걸림은 기존 폴링 세미닉 그대로, REPL에는 `page.on('dialog')` 로 노출(M2에서 이벤트 큐로 완성).
- CLI: `oxibrowser repl [--account id] [--headless]` — stdin 한 블록 = 한 프로그램(`await` 최상위 허용), 결과는 JSON. 전체 라이프사이클에서 OXSESS 봉투 저장(`session`의 SaveState/LoadState 재사용).

완료 기준: 로컬 fixture(루프백 서버)에서 (1) `await page.getByRole('button', {name: 'Submit'}).click()` 이 실제 폼 제출로 이어지고, (2) password 필드 literal fill이 거부되며 `fillCredential` 경로는 성공하는 e2e 2개. boa 브리지 타임아웃(액션 30s) 테스트.

### M2 — steering 이벤트 (P1 전반)

- `agent/events.rs`: `set_event_sink`로 CoreEvent 구독 → agent-world 이벤트 큐. REPL API: `page.waitForEvent('dialog'|'download'|'console'|'popup')`, 루프(M3)에는 인터럽트로 주입.
- 팝업 gap 보완: TabManager 폴링(500ms, 탭 세트 diff) → `popup` 이벤트. `CoreEvent::TabSpawned` 코어 추가는 별도 소형 PR로 분리.
- 다운로드: `set_download_behavior(작업 디렉터리)` + `Download/DownloadFailed` 이벤트.

완료 기준: fixture에서 `alert()` 페이지 → `waitForEvent('dialog')` 수신 → `dialog.accept()` 해제 e2e. 다운로드 시작 이벤트 수신 e2e.

### M3 — 에이전트 루프 (P1 후반)

새 파일 `agent/loop.rs`, `agent/model.rs`, `agent/prompt.rs`.

- `ModelProvider` trait: `complete(messages, tools, opts) -> Stream<Delta>`. 구현 1개: OpenAI 호환 HTTP(CHAT 엔드포인트, wreq 재사용). Anthropic은 인터페이스만 남기고 M4+.
- API 키: credential 브로커에 `llm:<provider>` 스코프로 저장(`credential` CLI 재사용).
- 루프: 관측(M0) → 모델 → REPL 코드 실행(M1) → 결과/steering 피드백. compaction: 전사 길이 임계 초과 시 직전 관측+요약으로 압축(요약 호출도 같은 provider).
- consent 통합: 게이트 거부 → exit-5 envelope의 `request_id/ttl` 파싱 → viewer에 에이전트 consent 카드(기존 확인카드·일회용 토큰 패턴 재사용, `include_str! viewer.html` 확장) → `ConsentStore::active_for` 폴링(1s, TTL 한도) → 승인 시 재시도.
- 프롬프트 규율: `agent/prompt.rs`에 상수 시스템 프롬프트 + 도구 정의. CI에서 대략 토큰 상한(10K) 검증 — 상한 초과면 빌드 실패 테스트.
- CLI: `oxibrowser agent run "<task>" [--account id] [--shell workspace|none] [--max-steps N] [--budget-tokens N]`.

완료 기준: 로컬 fixture 3-스텝 과업(검색→폼→결과 확인) 자율 완료 e2e(더미 모델 provider로 결정론 테스트 + 실제 모델 1회 스모크). consent 일시정지→viewer 승인→재개 e2e.

### M4 — 기억 + API 재생 (P2)

- site file: `~/.local/share/oxibrowser/sites/<origin>.json` — `{verified_selectors, login_markers, api_endpoints, bot_notes, stats}`. 루프가 성공 액션의 로케이터/엔드포인트를 기록, 다음 실행에 관측 주석으로 주입("이 사이트는 `.search-input` 가 검증됨").
- `task resume <id>`: 전사(OXSESS + audit JSONL)에서 마지막 관측 복원 후 재개.
- `page.replay(requestId)`: `network_log_snapshot()` 항목을 in-page `fetch()`로 재생(쿠키/헤더는 세션 것 사용 — 사이트 자체 요청과 동일 지문).

완료 기준: 동일 과업 2회 실행에서 2회째 관측/스텝 수 감소를 로그로 실측하는 회귀 테스트. replay가 fixture API를 인증 없이 재생하는 e2e.

### M5 — CUA 폴백 + 코퍼스 (P3)

- `page.screenshot()` → `capture_png`(vello_cpu), `page.mouse.click(x,y)`, `page.viewport = {1440, 900}`(에이전트 탭은 `set_viewport_override` 고정, viewer 미러와 무관).
- `oxibrowser agent eval <corpus-dir>`: 과업 YAML + 채점 함수 코퍼스 러너, 통과율 JSON 리포트. 로드맵 2번(공개 호환 벤치마크)과 같은 CI 자리에 배치.

완료 기준: 로케이터 불가 canvas fixture를 좌표 클릭으로 조작 성공. 코퍼스(최소 10 과업) 통과율 리포트 생성.

## 3. 수정하지 않는 것 (리스크 차단)

- 페이지 `JsRuntime`/js_thread_loop — M1~M5 전 구간 무변경 (agent-world는 별도 스레드).
- CDP 12 도메인, stdio MCP 12 도구, ghost 레인 — 표면 동결.
- `oxibrowser-cdp` 내부 구조(게이트/refs 재사용은 읽기만).
- 5-크레이트 구조 유지 (agent는 bin 크레이트 모듈).

## 4. 리스크

| 리스크 | 완화 |
|---|---|
| boa native fn 블로킹 브리지 교착 | REPL 스레드가 공유 락 미보유 원칙 + oneshot 30s 타임아웃 + 브리지 테스트 |
| RefRegistry 프로세스 전역(LazyLock) | 단일 CLI 프로세스 사용은 안전; 멀티 세션 서빙 시점에 세션 키 검증 강화 |
| 토큰 추정 부정확(바이트 근사) | M0에서 근사, M5 코퍼스에서 실측 토큰으로 보정 계수 도입 |
| 모델 provider 편차 | 더미 provider로 루프 로직 결정론 테스트; 실모델은 스모크만 |
| viewer 카드 확장 범위 확대 | 기존 확인카드 컴포넌트 재사용, 신규 스타일 없음 |
| consent 폴링 TTL 경쟁 | exit-5 envelope의 ttl을 그대로 존중, 만료 시 작업 실패 처리(재시도 없음) |

## 5. 브랜치/검증 규율

각 마일스톤 = `feat/agent-m<N>` 브랜치(main에서), 완료 기준 명령 실측 + 관련 테스트
통과 후에만 커밋→푸시. main의 현재 미커밋 ghost 변경은 별소 작업 — 절대 함께 stage하지
않는다. 문서 변경(본 파일)은 design 브랜치에 이어서.
