# 계정 프로세스 실행 모델 — 승인·오류·감사 계약 (2026-09-29)

> 상위: `2026-09-28-account-login-session-management.md`(M-A~M-D 구현 완료)의 다음 단계.
> CLI 프로세스 모델(knock: 프로세스마다 새로 뜨는 CLI 조합)에서 요구되는 16항목
> 로드맵 중 **Wave 1(계약) + Wave 2(프로세스 모델)** 구현. knock가 파싱 없이 소비하는
> 계약(exit code, 오류 envelope, 감사 스키마, 그랜트 장부, `--ref` 조인 키, 락, 휘발성
> 그랜트 래퍼)을 이 문서가 고정한다.

## 로드맵 (재정리 원문 기준)

| Wave | 항목 | 상태 |
|---|---|---|
| 3 | ④ MCP 계정 바인딩·도구 / ⑮ version·키체인 / ⑯ irreversible 집행 | ✅ 2026-09-29 |
| 2 | ② 계정별 flock / ① `account exec` / ③ `account capture` | ✅ 2026-09-29 |

| D | ⑩ 웹 뷰어 / ⑪ 가이드 캡처 UX | 미착수 |
| E | ⑫ IndexedDB(우선순위 상향) / ⑬ 봇 차단 실측 / ⑭ Fetch.authRequired stretch | 미착수 |

## 1. 오류 계약 — `CONSENT_REQUIRED` (exit 5)

- exit 테이블 확장: `0 OK · 1 RUNTIME · 2 INPUT · 3 TIMEOUT · 4 NETWORK · 5 CONSENT_REQUIRED`
  (`output.rs::exit_code`, `main.rs::print_error_details`, `describe` 스키마 동기화).
- `--json` 모드: 표준 envelope에 `details` 객체 추가.
  `{ok:false, error, error_code:"CONSENT_REQUIRED", details:{error_code, request_id, ttl, account, action, origin?}}`.
  `request_id`/`ttl`는 대기 중 확인(confirmation)이 있을 때만 값 — 없으면 `null`.
- 비JSON 모드: stderr에 사람 줄 1행 + **bare JSON 객체 1행**(같은 내용). 마지막 stderr
  줄을 JSON으로 파싱하면 된다. stdout은 오염 없음(`credential get`의 비밀 stdout 불변).
- 도달 경로(현재):
  - `credential get` — deny 룰 또는 확인 검증 실패.
  - `account login --mode agent` — `login` 액션 그랜트 부재(core `SourceError::ConsentRequired`
    → `NeedsLogin{reason:"consent_required:…"}` → CLI가 구조 오류로 전환).
- 해소 가이드(에이전트용): `request_id` 있음 → 뷰어 확인 대기. `null` →
  `account grant <id> --agent <A> --actions <action>` 또는
  `credential authorize --id <handle> --actions <action>` 후 재시도.

## 2. 감사 스키마 v1 (audit.jsonl)

이벤트 필드(전 줄 자기서술 — 헤더 아님, 부분 미러·로테이션 내구):

```jsonc
{
  "schema_version": 1,          // AUDIT_SCHEMA_VERSION; 모르는 버전 줄은 불투명 취급
  "ts": "…",                    // RFC3339 ms
  "seq": 12,                    // ⚠ 프로세스 로컬 — 병렬 프로세스 충돌 가능. 조인 키 금지
  "event_id": "evt-<instance>-<seq>",  // 전역 유일(AuditLog open마다 nanos+pid instance). 중복 제거·조인 키
  "ref": "task-9",              // 호출자 상관 태그(--ref). 없으면 생략
  "kind": "account_use",        // 기존 10종 그대로
  …
}
```

- **knock 미러 규칙**: dedupe/조인은 `event_id`로. `seq`는 프로세스 내 순서 참고용만.
- 구버전 줄(필드 없음)은 `schema_version:0`으로 파싱된다(serde default).

## 3. `--ref` 상관 태그

| 표면 | 효과 |
|---|---|
| `fetch --account <id> --ref TAG` | `session_restore`/`session_capture` 계열 감사에 `ref` 스탬프 |
| `serve --account … --ref TAG` | 위 + CDP `account_use`(createBrowserContext 게이트 양쪽)에 스탬프 |
| `account grant <id> --ref TAG` | 그랜트 기록(`consents.jsonl` `"ref"` 필드) + 감사 줄 + `grants` 목록에 저장 |
| `account revoke <id> --ref TAG` | 감사 줄 스탬프 |
| `account exec <id> … --ref TAG` | 휘발성 그랜트 기록 + 생성/폐기 감사 줄 모두 |

`ConsentRecord`에 `"ref"` 필드 추가(serde default — 기존 기록 역호환). 감사·그랜트·외부
Run 영수증이 같은 키로 조인된다.

## 4. `account grants <id> [--agent A] [--json]`

- 살아 있는(미폐기) 그랜트만 나열 — 폐기 이력은 감사 로그에만 존재.
- 행: `consent_id, agent, actions, origin, granted_at, expires_at, uses, max_uses, active, ref`.
- `active=false`는 만료/소진 표시(레코드는 보존). `--agent` 필터는 정확 일치.

## 5. Wave 2 — 프로세스 스코프 실행 모델 (shipped)

### 5.1 계정별 어드바이저리 락 (항목 ②)

- `~/.oxibrowser/accounts/<id>/lock`에 **flock(2) 배타적 잠금**
  (`core/src/account/lock.rs`, libc 직접 의존 추가). 커널이 보유자 사망 시 해제 —
  스테일 락 없음, 데몬 불필요.
- **보호 대상 = 봉투·레지스트리 뮤테이션**: `capture_session`, `logout`,
  `mark_stale`, `verify_with_probe`의 상태 전이 갈래, `set_state_locked`
  (엔진/오케스트레이터 전이 경로). `restore`는 디스크 읽기 전용이라 잠그지
  않고, 네트워크 프로브 중에는 절대 보유하지 않는다(serve가 CLI를 굶기지
  않음). 중첩 획득 금지 — 매니저 진입점 구조로 보장(같은 프로세스의 두 번째
  fd 획득은 블록되므로).
- CLI 전역 플래그 `--lock-wait`(블록) / `--lock-timeout <SEC>`(기본 fail-fast).
  충돌 시 `error_code:"ACCOUNT_LOCKED"`, exit 1.
- Advisory임을 문서화: 협력 표면(oxibrowser CLI/serve)만 존중.

### 5.2 `account exec` (항목 ①)

```
oxibrowser account exec <id> --agent <A> [--actions navigate,interact]
       [--ttl SEC(기본 3600, 상한 86400)] [--max-uses N(기본 1)]
       [--ref TAG] -- <CMD> [ARGS…]
```

- 흐름: 휘발성 그랜트 생성(ref 기록) → 자식 실행(stdio 패스스루) → **어떤 종료든**
  톰스톤(`exec_exit:N` / `exec_signal:N` / spawn 실패 `exec_spawn_failed`).
  자식 exit code 그대로 전파(시그널은 128+N).
- 자식 stdout은 자식 것 — 래퍼의 수명 주기 로그는 **stderr JSONL**
  (`{"exec":"grant",…}`, `{"exec":"exit","code":N,…}`).
- 자식 환경: `OXIBROWSER_ACCOUNT`, `OXIBROWSER_AGENT_ID` 주입(중첩 CLI용).
- **크래시 한계(정직한 문서화)**: 래퍼가 톰스톤 전 SIGKILL되면 그랜트는 TTL까지
  생존. per-grant flock liveness(그랜트별 락 파일 보유 = 유효)는 대안이나
  모든 `active_for` 검사 경로에 프로브를 심어야 해서 이번 파동에서는 TTL 상한
  (기본 1시간)을 경계로 채택. 필요시 아래 미결 참고.

### 5.3 `account capture` + `OXI.captureSession` (항목 ③)

- CDP 신규 명령 **`OXI.captureSession {accountId}`** → 바인딩된 컨텍스트 안에
  카메라 세션을 만들어(컨텍스트의 살아있는 jar/storage가 보임) 봉투로 봉인,
  세션 즉시 폐쇄. 감사는 기존 `session_capture`. 뷰어 역할은 기본 차단(역할
  허용목록 밖), 미바인딩 계정은 `captureFailed: noBoundContext`.
- CLI `account capture <id> --ws ws://…` — 최소 JSON-RPC-over-WS 클라이언트.
  knock가 serve 자식을 끊기 직전 최신 쿠키를 남기는 경로.

### 5.4 미결(다음 파동 후보)

- per-grant liveness flock(크래시 즉시 무효화), exec 그랜트의 `login` 액션
  프리플라이트, 감사 `account_use`의 자식 exit 코드 연동.

## 6. Wave 3 — 표면·운영 (2026-09-29 shipped)

### 6.1 MCP 계정 표면 (항목 ④)

- `serve --mcp --account <IDS>`: 각 계정 봉투를 컨텍스트로 복원(credential
  mode on), 첫 계정이 브라우저 도구(`browser_*`) 탭의 주 컨텍스트. 이전의
  **silent drop(플래그만 받고 무시) 수정**.
- `serve --mcp --as-agent <ID>`: 장부·상승 주석용 에이전트 신원.
- MCP 도구 3종 추가(기존 9종 → 12종): `account_list`(레지스트리 읽기 전용),
  `account_status {account_id}`(상태+세션 지평선, 네트워크 프로브는 CLI 유지),
  `login_request {account_id, reason?}` → `{action_required:"human_login",
  command, host_command, agent}` — stdio 서버는 뷰어 창을 못 열므로 운영자가
  칠 정확한 명령을 반환한다.
- `session --account <IDS> [--ref]`: REPL 시작 시 바인딩, `new` 탭이 주 계정
  컨텍스트에서 생성.

### 6.2 운영 계약 (항목 ⑮)

- `version --json`(숨김 해제): `{version, name, install_path,
  keychain_service_prefix}` — knock가 바이너리 고정·접두사 일치를 자가검증.
- `OXIBROWSER_KEYCHAIN_PREFIX` 환경변수: 자격증명·세션 키 양쪽 프로바이더에
  동일 적용(병렬 설치 격리). 항상 `version --json`으로 실효값 확인.
- **설치 경로 관례**: 키체인 ACL은 경로·서명 기반이므로 knock는 릴리스마다
  바이너리를 **고정 경로**(예: `~/.local/bin/oxibrowser`)에 복사할 것. 경로가
  바뀌면 macOS 키체인이 재확인 프롬프트를 띄운다 — 키 포맷엔 경로 성분이
  없어 조회 자체는 끊기지 않는다.

### 6.3 irreversible 집행 (항목 ⑯)
- **집행점**: `OXI.clickRef`/`fillRef` — 계정 바인딩 컨텍스트(credential mode)에서
  기술자(셀렉터 + http(s) URL + 폼 action + aria-label + 보이는 텍스트)가
  패턴에 걸리면 `irreversibleActionRequiresGrant`로 거부. `data:` URL은 성분에서
  제외(본문 전체를 실어 오탐).
  **범위 한계(문서화)**: 집행은 OXI ref 프로토콜에 한정 — `Input.*`·
  `Runtime.evaluate`로 몰래 치는 경로는 막지 않는다(§6.2 비밀 리터럴 게이트와
  같은 표면). Puppeteer/Playwright 원경로 클라이언트에겐 장식이며, 이 위협이
  실재하면 `deny_in_credential_mode` 확장이 후속 과제다.
- **역량 출처**: CLI 기동 컨텍스트(serve/session --account) = 사용자 직접 →
  허용(`bind_direct`). `Target.createBrowserContext {oxiAgentId}` = 비소모성
  `irreversible` 그랜트 탐침으로 표식.
- **주입**: 기본 목록(`core/account/irreversible.rs`, deny-biased) + 계정별
  `account.json` `irreversible_patterns`(**확장**, 대체 아님) — CLI
  `account irreversible <id> [--add …] [--clear]`.
- 감사: 매 판정이 `policy_violation` 계열 `irreversible_gate` 이벤트로
  (pattern, allowed 포함). 불완전성은 여전히 문서화된 한계(FM-L8).

## 6.5 D트랙 — 사용자 로그인 (2026-09-30 shipped)

### 참조 뷰어 (항목 ⑩)

- `GET /viewer` — 자기완결 단일 HTML(`cdp/src/viewer.html`, 외부 의존 0).
  토큰 붙여넣기(또는 호스트가 `?token=` 전달) → WS 연결 →
  `Page.startScreencast` 미러(프레임 ack 흐름제어 준수) → 좌표 스케일
  마우스/키 입력 → 내비게이션 바 → **`OXI.confirmationRequired` 승인 카드**
  (뷰어 전용 승인면 — 에이전트 자승인 금지 구조 유지).
- **쿼리 파라미터 역할 주장**: 브라우저 WebSocket은 커스텀 헤더를 못
  설정하므로 `?role=viewer&viewer_token=…`를 헤더와 동등하게 수용(원타임
  세미antics 동일, 잘못된 토큰 업그레이드 거부). 토큰은 여전히 out-of-band —
  사람이 페이지에 붙여넣거나 CLI stdout을 받은 호스트가 전달.

### 가이드 캡처 (항목 ⑪)

- 흐름: 사용자 실브라우저 로그인 → 명시적 내보내기(Playwright
  `storageState` JSON 또는 Netscape `cookies.txt`) → `account login <id>
  --mode import --storage-state <FILE>` → 프로브 검증 → 봉투 봉인.
- **실제 파싱 결함 수리(리뷰 검증 포함)**: Playwright는 `expires`를 부동소수로,
  필드명을 `httpOnly`로 내보내는데 둘 다 거부되던 것을 수용(정수·부동소수·null
  및 두 표기 병행). 세션쿠키 관례 `expires: -1`은 `None`으로 매핑 — 숫자
  그대로 접으면 만료 폴딩이 과거로 판정해 **쿠키를 삭제하며 조용히 세션이
  유실**되던 경로(P2). 직렬화도 `httpOnly` 표기로 통일(구 `http_only`
  스냅샷은 별칭으로 읽기), 빈 이름 쿠키는 jar 삽입에서 거부. 회귀 테스트
  `playwright_float_epochs_parse`.
- 경계 유지: 일상 프로파일 attach 금지는 그대로 — 명시적 1회 내보내기만
  허용되고, FM-L4(타 기기 지문 불일치) 경고는 import 시 그대로 적용.

## 6.6 E트랙 — 실사이트 신뢰성 (2026-09-30 측정)

### IndexedDB v1 (항목 ⑫ / FM-L5 근본 해법의 첫 단)

- **저장 평면**: 오리진별 `indexed_db` 버킷(context 공유) + 봉투 `OriginState.indexed_db`
  (`{name, version, stores: {store → key → record-json}, key_paths}`). 봉투가 곧
  지속 계층 — 별도 디스크 스토어 불필요(프로세스 모델의 "프로필"이 곧 봉투).
- **JS 표면 v1**: `indexedDB.open(versioned upgrade)/deleteDatabase`,
  `createObjectStore(keyPath)/deleteObjectStore/transaction`, `put/get/getAll/
  delete/count`. **동기 실행 + 지연 이벤트**: 연산은 즉시 적용되고
  `onsuccess`/`onupgradeneeded`는 다음 JS 펌프에서 발화(`drain_timers` 경유) —
  "호출 후 핸들러 할당" 정격 패턴이 동작. 문자열 키 + JSON 직렬화 값.
- **동기화**: localStorage와 동일한 채널·드레인 배리어·시드 계약 — 전체 db
  블롯 last-wins. `export_state_for_scope`는 IDB-only 오리진도 내보낸다.
- **검증**: 단위(keyPath put/get 왕복, 블롯에 key_paths 동행, 재등록 후 토큰
  생존) + e2e `test_indexeddb_persists_across_navigations`(내비게이션으로 JS
  맵이 완전히 교체된 뒤에도 토큰 생존). 미지원: 인덱스·커서·비-JSON
  structured clone — 측정된 사이트 요구 시 확장.

### 실측·우선순위·기타

- **실측(2026-09-29, 본 머신)**: 실사이트 스위트 전 녹색 — cli 11/11,
  integration 7/7, smoke 3/3. 이 과정에서 확인된 외부 변화 3건:
  example.com 본문 개편("Example Domain" 문구 소멸 → 단언 갱신),
  api.github.com 비인증 루트 403(측정 기록, 엔드포인트 재지정),
  smoke 하니스의 사전 결함 2건(피처 누락 빌드 실패·SSRF로 로컬 fixture
  차단 → `--allow-private-ips`).
- **봇 차단(항목 ⑬)**: 봇월 통과율의 지속 공개는 호환성 벤치마크
  (roadmap #2)의 결과물로 — 1회성 측정이 아닌 CI 집계가 계약. 실패
  사이트는 가이드 캡처(⑪)로 폴백.
- **큐레이티드 probe(항목 ⑫)**: `core/account/probes.rs` — 검증된
  마커만 시드(GitHub `meta[name=user-login]`), `account add`가 자동 적용.
  근거 없는 추측 마커는 거부(PR에 증거 요구). 호환 매트릭스는
  `docs/roadmap.md`로 이동·유지.
- **IndexedDB(항목 ⑫)**: 우선순위 상향 확정(roadmap #1) — CLI 프로세스
  모델에서 봉투 복원이 매 실행이라 IDB-인증 사이트는 매번 로그인 유실.
- **Fetch.authRequired(항목 ⑭)**: stretch 유지(roadmap #8 명시).

## 7. 검증 (2026-09-29)


Wave 3 검증:
- MCP stdio 스모크(격리 HOME): `account_list`/`account_status`/`login_request`
  응답 계약(에이전트·명령 포함), 미지정 계정 `isError`, `tools/list` 12개.
- `version --json` install_path·접두사 필드 + `OXIBROWSER_KEYCHAIN_PREFIX`
  실효 반영 확인.
- `account irreversible --add/--clear` 지속성(account.json 왕복).
- e2e `test_irreversible_gate_blocks_and_allows`: 기본 패턴 거부 → 주입 패턴
  거부 → 무해 통과 → 감사 기록 → 역량 부여 후 통과.
- `session --account`/`serve --mcp --account` 플래그 수용.

기존(Wave 1·2):
- CLI 스모크(격리 HOME): `account grant --ref` → consents.jsonl·audit 동시
  `"ref":"task-9"`; `account grants --json` 행·`--agent` 필터; revoke 후 빈 목록;
  `account exec` → 자식 exit 7 전파 + grant/exit stderr JSONL + 톰스톤 + 감사
  ref 2건; `OXIBROWSER_ACCOUNT/AGENT_ID` 환경 주입; 락 보유 중 `account logout`
  → `ACCOUNT_LOCKED` 후 해제 시 성공; `fetch/serve --ref` 플래그.
- 제약: `account login --mode agent` consent 분기의 실사이트 재현은 키체인 필요로
  생략(분기 로직은 core `SourceError::ConsentRequired` 테스트가 상류 보장).

재검증일: 2026-09-29.
