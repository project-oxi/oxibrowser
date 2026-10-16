# Agent plane 실행 설계 (2026-10-16, 리뷰 반영 v2)

> **상태 (2026-10-17): M0–M5 전 단계 구현 완료** — `feat/agent-m0` 브랜치
> (e66c3f5 → b165ef6, 10 커밋). 코퍼스 10/10, 최종 행렬: bin 98 단위 ·
> e2e 18 (loop 5 + repl 10 + obs 3) · core lib 707 · cdp lib 75 · core
> 통합 25. 실모델 스모크는 `--ignored` 등급으로 대기.

> [2026-10-16-agent-first-browser.md](2026-10-16-agent-first-browser.md)의 단계(P0–P3)를
> 코드 수준으로 전개한 실행 계획. v2는 2026-10-16 코드 대조 리뷰(치명 2·중간 7·경미 4)
> 를 반영해 consent 흐름을 3개 메커니즘으로 분리하고, bash를 v1에서 제외하며, 폼
> activation·팝업 기반을 실제 엔진 상태로 정정했다. 시간 추정 없음 — 의존 순서와
> 완료 기준만.

## 0. 조사로 확정된 사실 (계획의 전제)

| 설계 필요사항 | 이미 있는 구현 | 갭 |
|---|---|---|
| ariaSnapshot 관측 | `DomSnapshot::render_aria_yaml`(dom_snapshot.rs:2271), CDP `oxi.rs aria_snapshot()` | CDP 밖 노출 + 토큰 예산 |
| ref epoch 무효화 | cdp `refs.rs` RefRegistry + `resolve_and_validate`(generation·fingerprint·재질의) | 에이전트 모듈에서 재사용 |
| 불가역 게이트 | `irreversible_gate`(oxi.rs:299) — 거부는 CdpError 문자열, remedy는 `account grant` + **컨텍스트 재생성**(능력은 컨텍스트 생성 시 스냅샷) | 대기·승인 흐름은 별도 설계(M3) |
| credential consent | `PolicyEngine::RequireConfirmation` → broker pending + `OXI.confirmationRequired` 이벤트(requestId/timeoutMs) + viewer `OXI.resolveConfirmation` — **승인이 fill을 직접 완료**(재시도 아님). `CONFIRMATION_TTL`=60s. exit-5 envelope는 CLI 서브프로세스 계약 — 프로세스 내 REPL에서는 발생 안 함 | 루프가 이벤트를 구독해 카드 표시 후 결과 수신(M3) |
| 비밀 리터럴 거부 | `selector_targets_password` → `passwordFillRequiresCredential`; `fillCredential` 존재 | 로케이터 fill에 동일 규칙 |
| 안전한 액션 전달 | `Session::native_act`(session.rs:1876) — Rust 측 node/incarnation 검증·변이, 페이지 eval 없음(ghost가 이 경로를 만든 이유가 selector/이벤트 위조 방지) | M1이 이 경로를 채택 |
| 폼 제출 | **`form.submit()`은 noop, 클릭 기본 동작(activation behavior) 부재** — `AgentLoginEngine`도 직접 POST로 우회 | M1.5 코어 작업 |
| 팝업 | `window.open` 바인딩·`target=_blank` 처리 **둘 다 없음** — "이벤트가 없는" 것이 아니라 "새 탭을 여는 페이지 동작이 없는" 것 | M2에서 기능+이벤트 함께 |
| 이벤트 구독 | `Session::set_event_sink(CoreEvent)` — Dialog/Download/Console/Fetch | REPL 이벤트 큐 연결 |
| 뷰포트 | `set_viewport_override` — **프로세스 전역 static**. 다음 네비게이션부터 적용. `set_download_behavior`도 전역 | 에이전트 전용 프로세스 정책(§1) |
| boa 컨텍스트 패턴 | `JsRuntime` 전용 스레드 + `JsCommand` mpsc + fetch 브리지 응답 펌프. `Context::eval`은 script 골 — **최상위 await는 SyntaxError** | agent-world는 async IIFE 랩 + 바인딩 목록(M1) |
| 계정/로그인 오케스트레이션 | `AccountRuntime`/`LoginOrchestrator`, `AgentLoginEngine`(폼 직접 POST) | `page.login()` 랩(M3) |
| 크레이트 경계 | `irreversible_gate`·`resolve_and_validate`는 private, `DispatchContext` 요구 — bin 크레이트 구성 선례는 ghost.rs:272(단 `logins:None`이면 게이트 우회 = 반드시 채워야 함) | §1 재사용 경로 |

## 1. 구조 결정 (v2 확정)

- **에이전트 모듈 위치**: `crates/oxibrowser/src/agent/` (bin 크레이트, session/·mcp/ 패턴).
- **게이트/refs 재사용 경로**: cdp `domains` 디스패치 라우팅(변경 최소) — 에이전트 브리지가
  `DispatchContext`를 `logins`/`credential_mode`를 채워 구성(ghost.rs 선례, 단 ghost와 달리
  `logins` 필수) 후 `OXI.clickRef`/`fillRef` 계열 핸들러를 호출한다. `pub` 승격은 하지
  않는다. 강제 지점이 한 곳(도메인 핸들러)에 유지된다.
- **agent-world**: 페이지 `JsRuntime`과 별도 스레드 + 별도 boa `Context`. 입력 스크립트는
  async IIFE로 랩(최상위 await 지원). REPL 스레드는 공유 락 미보유 + oneshot 타임아웃 30s.
  M1 산출물로 agent-world 바인딩 목록 명시(`console`, `URL`, `fetch`(브리지), `JSON`,
  timers, `page`, `oxi`) — 미지원 심볼(`Buffer`/`process`/`require`) 오류는 "agent-world에는
  없음, 대안" 안내로 모델 자교정 유도.
- **액션 전달**: 전부 `native_act`(스냅샷 node_id → NativeTarget). 페이지 eval 경로
  (`click_js`)는 채택하지 않는다 — 페이지 스크립트의 selector/이벤트/descriptor 위조 차단.
  대가: native 경로의 알려진 한계(캡처 단계 없음, 인라인 onclick 미등록, shadow 콘텐츠
  미포함 — ghost-mcp.md 명시)를 상속; 한계 해소는 코어 후속 과제로 기록.
- **bash 없음(v1)**: 스코프 강제 수단(프로세스 격리+화이트리스트)이 없는 `--shell
  workspace`는 거짓 안전. v1은 브라우저 과업만; bash는 샌드박스 설계를 동반한 별도
  마일스톤(M6 후보)으로 연기.
- **뷰포트/다운로드 정책**: 에이전트는 전용 프로세스에서만 구동(`agent run`은 자기
  프로세스). 1440×900 오버라이드와 다운로드 디렉터리는 그 프로세스에 한정된 전역으로
  선언 — serve/CDP 클라이언트 동시 운영 금지를 문서+시작 시 경고로 강제. 퍼-세션 뷰포트는
  코어 변경 후 재검토.
- **레인**: `--trust full|restricted` 단일 플래그. `restricted` = 기존 MCP 관측/액션만(사실상
  ghost 레인). ghost 바이너리 경로 유지.

## 2. 마일스톤 (v2)

### M0 — 관측 기반

새 파일 `agent/mod.rs`, `agent/obs.rs`.

- `observe(session, ObsOpts{budget_bytes, max_actionable}) -> ObsResult{aria_yaml, actionable, url, title, generation, truncated, omitted}` — `dom_snapshot()` → `render_aria_yaml`(ref 클로저 = cdp `RefRegistry::allocate`). 예산 초과 시 트리 하단 절단 + 보고. 토큰은 바이트 근사(4B/token), M5에서 실측 보정.
- session REPL에 `observe <tab> [--budget N]` 추가. describe/skill 카탈로그 갱신 포함(신규 명령마다 — 이하 모든 마일스톤 동일).

완료 기준: `printf 'new\ngoto t1 https://example.com\nobserve t1 --budget 1500\nexit\n' | cargo run -- session --json` 출력 1,500토큰 이하 + ref 포함. 예산 절단·stale ref 단위 테스트.

### M1 — Oxiwright REPL

새 파일 `agent/world.rs`, `agent/bridge.rs`, `agent/locators.rs`.

- 로케이터: `getByRole/getByText/getByLabel/getByPlaceholder/locator(css)/.first/.nth` → 스냅샷 질의 → ref 발급 → `resolve_and_validate` 재검증. 액션 `.click()/.fill()/.press()/.check()`은 **native_act 전달**. 탐색 `goto/back/forward/reload`, `content()`, `ariaSnapshot()`, `screenshot()`.
- 게이트(§1 디스패치 경유): click → `irreversible_gate` 동일 조건; fill 대상 password → 거부 + `oxi.fillCredential(locator, 'account')` 안내(BrokerSource 해결, 네이티브 주입).
- **오류 계약 산출물**: timeout/strict/stale-ref/unsupported 구조화 코드 + Playwright 문체 메시지 명세.
- agent-world 바인딩 목록 + async IIFE 랩 + 미지원 심볼 안내(§1).
- CLI: `oxibrowser repl [--account id]` — stdin 블록 = 프로그램, 결과 JSON. OXSESS 저장 재사용.

완료 기준: (1) fixture에서 `getByRole('button',{name:'Submit'}).click()`이 **JS 핸들러 경로로** 실제 효과(DOM 변화/카운터)를 내는 e2e — 순수 HTML 폼 제출은 M1.5 전까지 기준에서 제외(activation behavior 부재, §0), (2) password literal fill 거부 + `fillCredential` 성공 e2e, (3) 브리지 30s 타임아웃 테스트.

### M1.5 — 폼 activation (코어 PR)

- submit 버튼 클릭의 기본 동작: 폼 인코딩(application/x-www-form-urlencoded, multipart 지원 여부 결정) → `post()` 네비게이션. `form.submit()` noop 해소. 로드맵 "Web platform gaps"와 병합 가능.
- 완료 기준: 순수 HTML 폼(핸들러 없음)에서 M1의 클릭이 서버 수신으로 이어지는 e2e. 프로브: 로컬 루프백 서버가 받은 바디 검증.

### M2 — steering 이벤트

- `agent/events.rs`: `set_event_sink` → REPL 이벤트 큐. `page.waitForEvent('dialog'|'download'|'console')`.
- **팝업은 기능 구현 포함**: `window.open` 바인딩 + `target=_blank` 링크 처리 → 새 세션 생성 + `CoreEvent::TabSpawned`(신규) 발행. 이것이 없으면 `waitForEvent('popup')`은 관측 대상이 없다(§0).
- 다운로드: 전역 `set_download_behavior`(전용 프로세스 정책 §1 준수) + 이벤트.

완료 기준: alert fixture → dialog 수신→해제 e2e; `window.open` fixture → popup 이벤트 + 새 탭 관측 e2e; 다운로드 시작 이벤트 e2e.

### M3 — 에이전트 루프

새 파일 `agent/loop.rs`, `agent/model.rs`, `agent/prompt.rs`.

- `ModelProvider` trait + OpenAI 호환 구현(wreq). 키는 브로커 `llm:<provider>` 스코프.
- 루프: 관측(M0) → 모델 → REPL(M1) → 결과/steering. compaction: 임계 초과 시 직전 관측+요약.
- **consent 통합 — 메커니즘별 분리(리뷰 치명-1 반영)**:
  - (a) credential consent: broker `OXI.confirmationRequired` 이벤트 수신 → viewer 카드 → `resolveConfirmation` 결과 수신 후 **재관측**(재시도 아님 — 승인이 fill을 완료함).
  - (b) 불가역 거부: 루프 정지 + viewer 카드("이 계정에 irreversible 그랜트 필요") → 사람이 `account grant` 실행 → 루프가 그랜트 관측 후 **컨텍스트 재생성 + 마지막 관측부터 재개**(능력이 컨텍스트 생성 시 스냅샷되므로). 게이트가 pending confirmation을 발행하도록 확장하는 안은 신규 엔진 작업으로 기록만.
  - (c) exit-5 envelope 파싱은 CLI 서브프로세스를 쓸 때만 유효 — 루프는 (a)/(b)만 사용.
- `page.login(id)` 랩(§4.3 설계 — AccountRuntime/LoginOrchestrator 재사용, site file 마커는 M4).
- 프롬프트 규율: 상수 시스템 프롬프트+도구 정의, CI에서 ~10K 토큰 상한 빌드 실패 테스트. 시스템 프롬프트에 위협 모델 격리 규칙 포함(설계 §4.7).
- CLI: `oxibrowser agent run "<task>" [--account id] [--trust full|restricted] [--max-steps N] [--budget-tokens N]` — `--shell` 없음(v1).

완료 기준: 더미 provider로 3-스텝 과업(검색→폼→확인) 자율 완료 e2e + 실모델 1회 스모크(`--ignored`급, CI 비상정). credential consent (a) 흐름 e2e: 카드→승인→재관측. 불가역 (b) 흐름 e2e: 거부→정지→그랜트→컨텍스트 재생성→재개.

### M4 — 기억 + API 재생

- site file `~/.local/share/oxibrowser/sites/<origin>.json` — 검증 셀렉터/로그인 마커/API 엔드포인트. 성공 액션 기록, 다음 실행 관측 주석 주입.
- `agent resume <id>`(명명 통일): 전사(OXSESS+audit JSONL)에서 마지막 관측 복원 재개.
- `page.replay(requestId)`: `network_log_snapshot()` 항목 in-page `fetch()` 재생.

완료 기준: 동일 과업 2회 실행 관측/스텝 수 감소 실측 회귀 테스트. replay가 fixture API를 재생하는 e2e.

### M5 — CUA 폴백 + 코퍼스

- `page.screenshot()`(capture_png), `page.mouse.click(x,y)`(렌더 문서 hit-test → native 전달 — 좌표→노드 해석은 M1의 hit-test 재사용), 뷰포트 1440×900(전용 프로세스 정책, §1).
- `oxibrowser agent eval <corpus-dir>`: 과업 YAML+채점 코퍼스 러너, 통과율 JSON 리포트. 로드맵 2번 벤치마크와 같은 CI 자리. 토큰 근사 보정 계수 도입.

완료 기준: 로케이터 불가 canvas fixture 좌표 클릭 성공. 코퍼스(최소 10 과업) 통과율 리포트.

## 3. 수정하지 않는 것 (리스크 차단)

- 페이지 `JsRuntime`/js_thread_loop — 전 구간 무변경 (agent-world 별도 스레드; M1.5는 엔진 동작 추가이지만 런타임 구조 변경 아님).
- CDP 12 도메인, stdio MCP 12 도구, ghost 레인 — 표면 동결. 도메인 핸들러는 디스패치 경유 재사용만(읽기 전용).
- 5-크레이트 구조 유지.

## 4. 리스크 (v2 확장)

| 리스크 | 완화 |
|---|---|
| boa native fn 블로킹 브리지 교착 | REPL 스레드 공유 락 미보유 + oneshot 30s 타임아웃 + 브리지 테스트 |
| 모델 작성 JS의 Node 표준 분포 가정(`Buffer`/`process`/`URL`/`require`, TMA) | async IIFE 랩, agent-world 바인딩 목록 M1 산출물, 미지원 심볼 자교정 메시지(§1) |
| 프롬프트 인젝션(ariaSnapshot 이름 등 페이지 파생 텍스트) | 설계 §4.7 위협 모델: 비신뢰 조립 + 격리 규칙 + native 액션 + 정책 강제 게이트; 잔여 위험은 코퍼스·감사로 관측 |
| native_act 경로 한계(캡처 없음, 인라인 onclick, shadow 미포함) 상속 | M1 문서화 + 코어 후속 과제 목록; 필요 시 사이트별 회피(site file) |
| consent 3-흐름 혼동 재발 | M3 완료 기준이 (a)/(b) 각각 e2e — 하나의 통합 흐름으로 테스트하지 않음 |
| 뷰포트/다운로드 전역 간섭 | 전용 프로세스 정책 + 시작 시 serve 공존 경고 |
| RefRegistry 프로세스 전역 | 단일 CLI 프로세스 안전; 멀티 태스크 서빙 시점 재검토 |
| 토큰 추정 부정확 | M0 근사 → M5 실측 보정 |
| 모델 provider 편차 | 더미 provider 결정론 테스트; 실모델 `--ignored` 스모크 |

## 5. 브랜치/검증 규율

각 마일스톤 = `feat/agent-m<N>` 브랜치(main에서). 완료 기준 명령 실측 + 관련 테스트 통과
후 커밋→푸시. 각 마일스톤 완료 기준에 describe/skill/main.rs 코멘트 갱신 포함(리뷰 경미-10).
main의 미커밋 ghost 변경은 별소 작업 — 함께 stage하지 않는다.
