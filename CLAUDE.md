# hdir 프로젝트 룰

Mdir 스타일 듀얼 패널 TUI 파일 관리자 (Rust + Ratatui). 전체 계획: `docs/plan.md`, 원 구상: `docs/mvp계획.md`. 인수인계: `docs/HANDOFF.md`.

## 1. TDD 필수
- 모든 기능 이슈는 Red → Green → Refactor 순서.
  - Red: 실패하는 테스트를 먼저 작성하고 `cargo test`로 실패를 확인한다.
  - Green: 테스트를 통과시키는 최소 구현.
  - Refactor: 테스트 통과 상태를 유지하며 정리.
- 테스트 없는 프로덕션 코드 커밋 금지. TUI 렌더링 레이아웃은 `ratatui::backend::TestBackend` 버퍼 스냅샷 테스트로 검증.
- 로직은 렌더링/IO와 분리(순수 함수·trait). 외부 명령(ssh/scp/$EDITOR)은 "명령 생성"과 "실행"을 분리하고 생성부를 테스트.
- 파일시스템 테스트는 `tempfile` 임시 디렉터리에서만.
- 커밋 전 게이트 (전부 통과해야 커밋):
  ```sh
  cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
  ```

## 2. 체크포인트 커밋
- 커밋 시점: (a) Green+Refactor 완료된 의미 단위, (b) 이슈 완료, (c) 리뷰 반영 완료. Red 상태 커밋 금지.
- 브랜치: 모듈 단위 `m<N>-<slug>` (예: `m1-core-tui`). `main` 직접 커밋 금지. 모듈 완료 → PR → 리뷰 → merge.
- 메시지: Conventional Commits (`feat:`, `fix:`, `test:`, `refactor:`, `docs:`, `chore:`). 본문에 Plane 이슈 ID (`Refs: HDIR-12`).
- `.env` 등 비밀 파일 커밋 금지. 커밋 전 `git diff --cached --name-only` 확인. 공개 리포임.

## 3. Plane 업데이트
- CLI: `plane` (`set -a; . ./.env; set +a` 후 `$PLANE_PROJECT` 사용).
- State ID:
  | State | ID |
  |---|---|
  | Backlog | `163c484e-7f59-46b6-8f6d-eb2a5cef1814` |
  | Todo | `1dba0214-c797-40d6-8d9a-d406b0ddc4bf` |
  | In Progress | `c602bd20-e8a4-4b05-ab17-39cd1bae36dd` |
  | Done | `6df7a108-aebb-4dca-98ab-6092627cfa20` |
  | Cancelled | `c5df0cd3-c2e2-4889-9ca3-f70cd1a59935` |
- 헬퍼: `scripts/plane.sh state <NN> <todo|progress|done|backlog>`, `scripts/plane.sh comment <NN> "..."`, `scripts/plane.sh status`.
- 이슈 착수 → In Progress. 체크포인트 커밋마다 이슈 코멘트(커밋 SHA·요약·테스트 결과). PR 생성 → PR URL 코멘트. merge → Done.
- 계획 변경·새 작업 발견 → 이슈 추가 후 모듈에 연결, Plane 계획 페이지 갱신.

## 4. 에이전트 협업
| 담당 | 모델 | 역할 |
|---|---|---|
| 메인 세션 | Opus | 이슈 분해, 설계 판단, 서브에이전트 지시, 결과 통합, 리뷰 판정, 커밋·PR |
| `hdir-impl` | Sonnet | 이슈 단위 TDD 구현. 파일이 겹치지 않는 독립 이슈는 `isolation: worktree`로 병렬 |
| `hdir-ops` | Haiku | Plane 상태·코멘트 갱신, 게이트 실행·요약, 문서, 단순 수정 |
| Codex | codex CLI | 병렬 코드 리뷰(필수), 난이도 높은 부분(ssh 출력 파서, scp 라우팅) 세컨드 오피니언: `codex exec -s read-only "<질문>"` |
- 서브에이전트 지시에는 이슈 ID·완료 조건·관련 파일·TDD 룰을 명시. 결과는 메인이 diff 검토 후 채택.
- 같은 파일 동시 수정 금지.
- 서브에이전트는 커밋·Plane 갱신을 하지 않는다(메인 또는 hdir-ops가 수행).

## 5. Codex 병렬 리뷰 (PR마다 필수)
- `scripts/codex-review.sh <base>` → 3개 관점(정확성 / 테스트·TDD / 보안·성능)을 병렬 실행, 결과는 `.reviews/<branch>/`(gitignore).
- 동시에 Claude `cavecrew-reviewer`로 교차 검토.
- 판정(메인): Critical/High → 수정 후 재리뷰. Medium → 수정 또는 Plane 이슈 등록. 오탐 → PR 코멘트에 사유.
- 리뷰 요약을 PR 본문과 Plane 이슈 코멘트에 기록.

## 6. Linux 테스트
- 로컬은 macOS. Linux 동작(renameat2, non-UTF-8 파일명, FIFO, 대소문자 구분 FS, 원격 `find -printf` 등)은 Linux 서버에서 검증한다.
- 서버: `ssh choi@stage.devfor.link`, 경로 `/home/choi/workspace/project/hdir` (origin = GitHub 리포 clone).
- 절차: 브랜치를 push한 뒤 서버에서 같은 브랜치를 맞추고 게이트 실행.
  ```sh
  git push
  ssh choi@stage.devfor.link 'cd /home/choi/workspace/project/hdir && git fetch -q origin && git checkout -q <branch> && git reset -q --hard origin/<branch> && source ~/.cargo/env && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test'
  ```
- 시점: PR 생성 전 필수, `#[cfg(target_os = "linux")]`·플랫폼 분기 코드를 바꾼 체크포인트마다 실행.
- 결과(통과 수, 실패 원문)를 PR 본문과 Plane 이슈 코멘트에 `[linux]` 태그로 기록.
- 서버 작업 디렉터리에서는 테스트만 한다. 서버에서 커밋·push 금지, 리포 밖 파일 수정 금지.

### Windows / PowerShell 테스트
- Windows 동작(경로·드라이브 문자, 휴지통, 읽기 전용 속성, symlink 권한, PowerShell 셸 통합, Windows Terminal 키 입력)은 Windows 서버에서 검증한다.
- 서버: `ssh win11.dgst` (기본 셸 PowerShell 7), 경로 `C:\Users\Docker\workspace\project\hdir`.
- 절차 (명령은 PowerShell 문법으로 전달):
  ```sh
  git push
  ssh win11.dgst 'Set-Location C:\Users\Docker\workspace\project\hdir; git fetch -q origin; git checkout -q <branch>; git reset -q --hard origin/<branch>; cargo fmt --check; if ($LASTEXITCODE -eq 0) { cargo clippy --all-targets -- -D warnings }; if ($LASTEXITCODE -eq 0) { cargo test }'
  ```
- 시점: PR 생성 전 필수 (Linux와 함께), `cfg(windows)`·`cfg(unix)` 분기를 바꾼 체크포인트마다.
- 결과를 PR 본문과 Plane 코멘트에 `[windows]` 태그로 기록. 서버에서 커밋·push 금지.
- 플랫폼 전용 코드는 `#[cfg(unix)]` / `#[cfg(windows)]`로 분리하고, 공통 로직은 플랫폼 독립 순수 함수로 둔다.
- **Windows 지원 범위 (사용자 결정)**: 무리하지 않는다. Windows는 빌드·테스트 통과만 유지하고, 어려운 기능(휴지통 보기, 셸 통합, 확장자 실행, SSH, 드라이브 이동 등)은 Windows에서 "Windows 지원 예정" 안내로 막는다. 관련 이슈는 Plane M10 Backlog.

## 7. 코드 원칙
- 반응성 최우선: UI 스레드에서 블로킹 IO(원격 명령, 큰 복사) 금지 → 워커 스레드 + mpsc.
- 외부 명령 인자는 셸 이스케이프 필수. 파일명은 공백·한글·따옴표·개행 포함을 가정.
- 기본 테마는 `mdir` (DOS 16색 팔레트).
