# SPA CI 보완 — 2026-10-30

[초기 구현](2026-10-30-spa-implementation.md)의 원격 [CI #124](https://github.com/project-oxi/oxibrowser/actions/runs/40991824787)에서 Linux의 전체 테스트, check, format, SPA 150개 작업·100회 반복·2컨텍스트 검사는 통과했다. 보안 감사와 CI 도구체인의 Clippy는 실패했다. Release Build는 이 선행 조건 때문에 실행되지 않았다.

## Rust 1.96 Clippy

로컬 Rust 1.99에서 통과한 세 조건식에 대해 CI의 1.96은 `nonminimal_bool`을 보고했다. `!option.is_some_and(predicate)`를 동치인 `option.is_none_or(반대 조건)`으로 바꿨다. 문서가 없는 `None` 분기의 거부 동작도 유지된다. 수정 후 Rust **1.96.1 및 1.99.1**에서 전체 target strict Clippy를 통과했다.

## zstd 의존성

보안 감사는 SPA 변경 이전부터 있던 zstd 계열 패키지 버전을 최신 권고 DB와 대조해 네 건을 보고했다: [RUSTSEC-2026-0391](https://rustsec.org/advisories/RUSTSEC-2026-0391.html), [0398](https://rustsec.org/advisories/RUSTSEC-2026-0398.html), [0403](https://rustsec.org/advisories/RUSTSEC-2026-0403.html), [0410](https://rustsec.org/advisories/RUSTSEC-2026-0410.html). 패키지 버전 기준의 감사 결과이며 모든 경로의 실제 악용 가능성을 뜻하지 않는다.

| 패키지 | 기존 | 수정 |
| --- | --- | --- |
| compression-codecs | 0.4.39 | 0.4.40 |
| zstd | 0.13.3 | 0.14.3 |
| zstd-safe | 7.2.4 | 8.2.0 |
| zstd-sys | 2.0.16+zstd.1.5.7 | 2.1.2+zstd.1.5.7 |

HTTP 경로의 `async-compression 0.4.44`는 codecs 0.4.40을 허용한다. 하지만 공개 `wreq-util` rc.13/rc.14의 `zstd ^0.13.3` 제약은 수정 버전 0.14.3을 허용하지 않는다. rc.11은 사용하는 Chrome149 emulation을 제공하지 않아 대체할 수 없다.

따라서 정확한 공개 `wreq-util 3.0.0-rc.13` 아카이브를 보존하고 두 manifest의 zstd 버전만 바꿨다. root dependency와 patch를 같은 정확한 버전에 고정했다. 원본 55개 파일을 대조해 두 manifest 외의 변경과 누락이 없음을 확인했다. source, TLS profile, feature, 라이선스는 그대로다. [체크섬과 유지보수 조건](../../vendor/wreq-util/PROVENANCE.md)에 근거를 남겼다.

`deny.toml`의 예외·정책은 바꾸지 않았다. HTTP zstd 해제도 하지 않았다. 루트 dependency graph에는 수정된 zstd 체인 하나만 남는다. 실제 `Content-Encoding: zstd` 응답을 풀어 바이너리 `ArrayBuffer`로 받는 회귀 검사와 `Accept-Encoding` 광고 확인을 추가했다. 프레임에는 압축 블록과 checksum이 있다.

## 검증

- 기본 병렬 workspace: **1,315 통과, 0 실패, 35 제외**.
- 관련 WebGL·이미지·입력: **35 통과, 1 수동 검사 제외**.
- release `browser` 빌드, Rust 1.96.1/1.99.1 strict Clippy, rustfmt: 통과.
- `cargo deny check advisories licenses bans sources`: 네 항목 모두 통과.
- 갱신한 배포 바이너리의 실제 입력 SPA **150/150**, 모든 PNG **1280×800** 검사 통과.
- 동일 바이너리의 **100회 reload·2컨텍스트 격리** 통과. 동시 빌드 부하가 있던 검사와 자체 검증 프로세스 종료 후의 관측을 모두 보관했다. 실패를 재시도로 가린 경우는 없다.
- 별도 작성자가 패치했고 새 문맥의 읽기 전용 검토자가 archive·manifest·선택 제약·API 사용·테스트·조건식 동치를 확인했다. 추가 수정이 필요한 지적은 없었다.

[정확한 바이너리·명령·해시·측정 조건](2026-10-30-spa-evidence/checks.json), [10회 실행 결과](2026-10-30-spa-evidence/summary.json), [반복 검사](2026-10-30-spa-evidence/stability.json)를 보관했다. 원격 최종 판정은 [PR #4](https://github.com/project-oxi/oxibrowser/pull/4)의 현재 head checks를 기준으로 확인한다. 로컬 산출물을 원격 CI 결과로 표시하지 않는다.
