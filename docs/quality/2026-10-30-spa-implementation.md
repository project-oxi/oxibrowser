# 일반 SPA 1차 구현과 검증 — 2026-10-30

[목표·설계](2026-10-27-spa-goal-and-design.md)에 따라 고정 SPA 작업을 실제 입력으로 실행하도록 구현했다. 현재 로컬 배포용 `browser` 빌드는 **15개 작업 × 새 프로세스 10회 = 150/150**, 한 프로세스의 **100회 새로고침**, 동시에 열린 **독립 저장소 컨텍스트 2개**를 통과했다. 기준 Chrome도 같은 작업과 반복 검사를 통과했다. 최종 전체 워크스페이스 회귀 검사도 1,314 통과·0 실패로 완료했다.

이 결과는 고정한 예제와 버전에 대한 검증이다. 임의의 SPA, 전체 React/Vue, 전체 CDP나 Web Platform 호환성을 뜻하지 않는다. 청계 실행은 별도 검증 대상이다.

## 구현

- 문서·realm·노드 incarnation으로 DOM wrapper를 관리한다. 같은 노드의 identity, expando, listener를 유지하고 분리된 노드는 약한 참조로 관리한다. 실제 문서 교체에는 새 JS realm을 만들며 이전 전역·리스너·원격 객체를 무효화한다.
- capture → target → bubble, 취소·once·포커스·텍스트 선택 영역·체크 상태·폼 기본 동작을 연결했다. 네이티브 입력은 페이지가 덮어쓴 `dispatchEvent`, 값·선택 영역 접근자를 거치지 않는다. 리스너 예외는 관측 채널에 전달한다.
- computed style, quads, hit test와 paint가 실제 Stylo/Taffy/Parley 상태를 사용한다. 동적 CSS는 실제 응답 바이트·최종 URL·DOM 순서·연결 상태를 반영한다. 절대 위치·인라인 문자·iframe 크기와 내장 컨트롤 배치를 보정했다.
- CDP 원격 객체, 함수 호출·프로퍼티 조회·해제, utility world, frame 수명과 실제 native 입력을 구현했다. 기본 PNG 캡처는 viewport 크기를 따르고 렌더링 실패를 오류로 반환한다. 프레임의 DOM 좌표는 해당 viewport에 남기고 CDP quads는 살아 있는 iframe 소유자 경로를 통해 루트 viewport로 변환한다.
- 같은 출처 iframe은 실제 child 문서를 읽고 네이티브 입력을 받는다. 출처·sandbox·문서·연결 세대가 바뀌면 접근을 취소한다. 하위 프레임의 전체 문서 이동은 `NotSupportedError`로 거부하여 상위 문서를 잘못 이동시키지 않는다.
- History/Location은 네이티브 상태를 사용한다. 같은 문서의 back/forward와 hash 이동은 realm을 유지하며 실제 문서 이동·reload는 교체한다. `location.replace()`와 JSON 호환 history state를 처리한다.
- 기본 CDP `serve`와 일반 stdio MCP에서 앱 작업이 명령 사이에도 진행한다. CDP는 `--no-background-event-loop`로 명령 중심 실행을 선택할 수 있다. 제한된 Ghost MCP 실행 계약은 별도다.

Blitz 수정은 정확한 공개 버전 `0.3.0-beta.1`의 여섯 소스 파일로 한정했다. 아카이브 SHA-256, 라이선스와 수정 범위는 [PROVENANCE](../../vendor/blitz-dom/PROVENANCE.md)에 있다. 원본 아카이브 파일 누락과 의존성 버전 변경은 없다.

## 실제 입력 검증

React `18.2.0`, ReactDOM `18.2.0`, Vue `3.5.13`, Playwright `1.48.2`를 고정했다. React/Vue 자산은 각각 UMD production bundle이다. npm 배포물의 MIT(React/ReactDOM/Vue), Apache-2.0(Playwright) 라이선스를 확인했고 번들을 저장소에 복사하지 않았다. 모든 HTML/CSS/JSON·번들 해시는 [실행 기록](2026-10-30-spa-evidence/runs/run-01.json)에 있다.

각 작업은 실제 클릭·키 입력을 사용한다. `evaluate`는 관찰에 사용한다. 예상하지 않은 page/console 오류, 필수 요청·HTTP 실패, crash, 30초 작업 제한 초과와 캡처 실패는 불합격이다. PNG 헤더의 실제 크기도 설정한 viewport 1280×800과 일치해야 한다. 매 반복마다 기본 `serve`의 새 프로세스와 새 컨텍스트를 사용했다.

| 작업 | 성공 | p50(ms) | p95(ms) |
| --- | ---: | ---: | ---: |
| vanilla-form | 10/10 | 431 | 440 |
| react-counter | 10/10 | 378 | 387 |
| react-controlled-input | 10/10 | 356 | 361 |
| react-list | 10/10 | 482 | 507 |
| react-conditional | 10/10 | 382 | 397 |
| vue-counter | 10/10 | 385 | 410 |
| vue-controlled-input | 10/10 | 354 | 375 |
| vue-list | 10/10 | 478 | 491 |
| vue-conditional | 10/10 | 388 | 403 |
| async-fetch | 10/10 | 312 | 323 |
| async-router | 10/10 | 434 | 452 |
| async-stylesheet | 10/10 | 426 | 440 |
| storage-reload | 10/10 | 492 | 527 |
| frame-nested | 10/10 | 492 | 541 |
| geometry-rect | 10/10 | 276 | 292 |

p50/p95는 각 작업의 성공 표본 10개에서 계산했다. navigation·자동화 왕복·검증·캡처를 포함하는 전체 작업 시간이며 순수 입력 지연이 아니다. 작은 표본의 p95는 잠정치다. 기준 Chrome은 `156.0.8078.49`다. 실행기는 같은 fixture를 기준 Chrome에 먼저 적용하고 실패하면 후보를 실행하지 않는다.

[전체 10회 결과](2026-10-30-spa-evidence/summary.json), [기준 Chrome](2026-10-30-spa-evidence/reference.json), [캡처](2026-10-30-spa-evidence/screenshots/)를 함께 보관했다. JSON은 원래 임시 산출물 경로도 기록한다. 버전 관리한 캡처는 후보 첫 실행 15개와 기준 Chrome의 대표 4개다. 원본 전체는 `/tmp/oxi-spa-viewport-acceptance`에 생성했다. 주요 결과 텍스트·컨트롤·child 화면을 직접 확인했으며 전체 픽셀 동일성 판정은 하지 않았다.

## 문서 회수와 메모리

처음 100회 기능 검사는 성공했지만 샘플 RSS가 175.0 → 1552.6 MiB로 증가했다. 프레임워크 없는 작은 HTML도 100회에 약 1.0 GiB에 도달했다. 네이티브 약한 참조를 측정하자 10회 교체 후 이전 문서까지 11개가 남았고, 두 번의 GC로도 줄지 않았다.

원인은 [stealth API](../../crates/oxibrowser-core/src/js/stealth.rs)의 네이티브 콜백 여섯 곳에서 GC 객체를 추적되지 않는 closure에 보관한 것이었다. 플러그인·MIME 배열, user-agent data, thenable의 객체 참조를 명시적 traced capture로 바꿨다. 분리된 bare Boa realm 실험은 수정 전 실패·수정 후 성공했고, 현재 문서와 utility world를 사용하는 10회 교체 회귀 검사에서는 GC 후 문서가 1개 남는다. 실제 입력 값·선택 영역·포커스·expando·listener와 utility 접근도 보존된다. 진단용 기능 제거·RSS 출력·ablation 스위치는 남기지 않았다.

수정된 동일 배포 빌드의 100회 반복도 성공했다. reload → 카운터 초기화 → 실제 클릭 → 상태 반영의 p50은 290ms, p95는 310ms다. 샘플 RSS는 103.4 → 184.4 MiB, 샘플 최대 184.4 MiB였다. 메모리 증가가 모두 사라졌다는 주장은 하지 않는다. 장시간 RSS 증가·캐시·할당기 영향과 수치 예산은 후속 측정 대상이다.

[수정 전](2026-10-30-spa-evidence/memory-before.json), [수정 후](2026-10-30-spa-evidence/stability.json), [GC 회귀](2026-10-30-spa-evidence/reclamation-after.txt)를 보관했다. RSS는 OxiBrowser 프로세스만 2초 간격으로 샘플링했으며 실제 peak가 아니다. Chrome 하위 프로세스 RSS는 측정하지 않아 브라우저 간 메모리 비율을 비교하지 않는다.

## 빌드와 회귀 검사

검증 환경은 Apple M4 Max, RAM 64GiB, macOS `27.0.1 (26A434)`, Rust `1.99.1`, Node `24.20.0`, Python `3.9.6`이다. SPA는 release + `browser`, WebGL 비활성 빌드로 실행했다. 바이너리는 61,185,680 bytes (58.4 MiB), SHA-256 `7aaaf0a23b2b98161d0203b105f6e4981dbdc30e51ff84c99bd30cb05d50cfba`다. 로컬 서버·고정 응답·기본 viewport 1280×800, 배율 1을 사용했다. 공유 호스트에서 다른 검증 프로세스도 실행됐으므로 시간 수치는 격리된 성능 벤치마크나 SLO가 아니다. 준비 완료 시간은 250ms polling 관측이며 정밀 시작 시간으로 해석하지 않는다.

- release `browser` 빌드, 전체 target strict Clippy, rustfmt: 통과.
- 전체 workspace 기본 병렬 실행: **1,314 통과, 0 실패, 35 제외**. 새로 발견한 응답 전 이벤트 수신 테스트 헬퍼 결함을 수정한 뒤 전체를 다시 실행했다.
- 관련 WebGL·이미지·입력 회귀: 35 통과, 1 수동 자산 검사 제외.
- 두 번의 독립 Codex 검토에서 확인한 수명·보안·이동·이벤트 문제를 회귀 검사로 보완했다. 이후 메모리 원인은 진단 패널·분리 실패 재현·네이티브 수명 계측으로 확인했다.
- CI에 `browser` profile check/test/Clippy와 SPA 10회·100회·2컨텍스트 검사, 산출물 업로드 및 release 선행 gate를 추가했다. 원격 CI 결과는 아직 이 로컬 결과에 포함하지 않는다.

명령과 로그 해시는 [checks.json](2026-10-30-spa-evidence/checks.json)에 있다. [실행 방법](../../tests/spa/README.md)으로 재현할 수 있다.

## 현재 제한

- History state는 JSON 호환 값이다. 일반 structured clone 전체·완전한 폼 제약 검증을 제공하지 않으며, 검증한 제약은 required/custom validity다.
- iframe 부모 경계는 제한된 읽기 membrane이다. 임의의 다른 realm 함수 호출·DOM 변경과 child 전체 문서 이동을 지원하지 않는다. 교차 출처·opaque sandbox의 읽기/API 권한은 별도 보안 제한을 따른다.
- localStorage의 새로고침 지속·출처 및 컨텍스트 격리를 검증했다. 기존 sessionStorage의 문서 이동 시 초기화 동작은 남아 있다.
- CDP 캡처는 PNG·배율 1·현재 viewport 안의 정수 clip을 지원한다. 다른 format/scale과 beyond-viewport 요청은 명시적 오류다. 기존 OXI/CLI/PDF의 full-page 경로는 유지한다.
- IME/composition, rich text, 모든 텍스트 편집·CSS fragment/3D 기하, 모든 CDP 명령을 검증하지 않았다. 비동기 네이티브 I/O와 긴 작업은 협력적 스케줄링 경계를 넘을 수 있다.
- ESM의 HTTP 로딩·동적 import·TLA·실패는 별도 기존 회귀로 검사한다. 이 React/Vue 사용자 작업은 UMD 조합이며 Vite/Next 및 최신 framework 버전 지원의 대리 지표로 사용하지 않는다.
- Worker/Service Worker, 미디어, WebGPU, Canvas/WebGL 게임과 청계의 전체 실행은 이 milestone 밖이다. 전체 WPT conformance와 절대 지연·메모리 예산도 확정하지 않았다.
