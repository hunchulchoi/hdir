# hdir 핸드오프

작성: 2026-10-30 · 기준 커밋: `main` (PR #12 merge 직후)

Mdir 스타일 듀얼 패널 TUI 파일 관리자 (Rust + Ratatui). 원 구상 `docs/mvp계획.md`, 전체 설계 `docs/plan.md`, 개발 룰 `CLAUDE.md`.

## 1. 한눈에 보는 상태

- 이슈 79개 중 **Done 54**, Todo 18, Backlog 7 (Windows 지원 예정).
- 끝난 모듈: M1 코어 TUI, M2 파일 작업 + 휴지통, M3 뷰어·미리보기·모드, M4 설정·테마·키, M5 확장자 연결, M6 셸 통합, M7 SSH 원격 패널(파트 A·B).
- **다음 작업: M7 남은 High 4개(HDIR-73~76) → M8 scp 전송 → M9 문서·릴리스.**
- 열린 PR 없음. 모든 작업은 `main`에 merge됨. 진행 중인 브랜치·stash·worktree 없음.
- 테스트 (PR #12 기준): macOS 876, Linux 876, Windows 709 통과. 셸 통합 테스트 6, 실제 ssh 테스트 3(`#[ignore]`).

## 2. 새 서버 준비

| 항목 | 내용 |
|---|---|
| 리포 | `git@github.com:hunchulchoi/hdir.git` (공개). 이전 머신은 SSH 키 미등록이라 HTTPS + `gh auth setup-git` 사용 |
| Rust | 1.97+ (stable). Linux 서버는 1.99 |
| `.env` (리포 루트, git 무시) | `PLANE_PROJECT=2574e50f-48e1-473e-9f73-4823cd87aee0` — 새 서버에 직접 만들 것 |
| Plane CLI | `plane` (`~/.local/bin/plane`), 설정 `~/.config/plane` 에 `PLANE_BASE_URL=https://plane.devfor.link`, `PLANE_WORKSPACE=zodiac`, API 키. 봇 계정(codex bot)으로 동작 |
| GitHub CLI | `gh auth login` (repo scope) |
| Codex CLI | `codex` 0.165+ 로그인 (병렬 리뷰용) |
| SSH | `choi@stage.devfor.link` (Linux 테스트 + 원격 패널 실서버), `win11.dgst` (Windows 테스트, 기본 셸 PowerShell 7). 새 서버의 공개키를 두 서버에 등록해야 함 |
| 선택 | `zsh`, `bash` (셸 통합 테스트), `fish` (미설치 시 건너뜀) |

확인 명령:
```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
scripts/plane.sh status | head
ssh -o BatchMode=yes choi@stage.devfor.link true && ssh -o BatchMode=yes win11.dgst 'echo ok'
```

## 3. 작업 방식 (CLAUDE.md 요약)

1. **TDD**: 실패하는 테스트 먼저 → 실패 확인 → 최소 구현 → 정리. 커밋 전 `fmt --check / clippy -D warnings / test` 게이트.
2. **브랜치**: 모듈 단위 `m<N>-<slug>` → 체크포인트 커밋(`Refs: HDIR-NN`, Co-Authored-By 라인) → PR → 리뷰 → squash merge. `main` 직접 커밋 금지.
3. **Plane**: 착수 `progress`, 체크포인트마다 코멘트(`[checkpoint] <sha> ...`), merge 후 `[pr] <url>` 코멘트 + `done`.
   ```sh
   scripts/plane.sh state 45 progress
   scripts/plane.sh comment 45 "[checkpoint] abc1234 ... · tests: N"
   scripts/plane.sh state 45 done
   ```
   새 이슈는 `plane issues create` + `plane modules add-issue` (모듈 ID는 `plane modules list -p $PLANE_PROJECT`).
4. **에이전트**: 메인(Opus)은 분해·판단·통합, `hdir-impl`(Sonnet)은 이슈 단위 TDD 구현, `hdir-ops`(Haiku)는 Plane·게이트, Codex는 리뷰. 서브에이전트에게는 이슈 ID·완료 조건·관련 파일을 명시.
5. **리뷰 (PR마다)**: `scripts/codex-review.sh main` → `.reviews/<branch>/{correctness,tests,security-perf}.md`. Critical/High는 수정 후 재리뷰, Medium은 수정 또는 이슈 등록, 반박은 PR에 사유 기록.
6. **3개 플랫폼 게이트 (PR 전 필수)**: CLAUDE.md 6절 명령 그대로. Windows는 "빌드·테스트 통과만, 기능은 지원 예정" (사용자 결정).
7. **서버에서 정리할 때 `rm -rf <변수>` 금지**: 만든 파일만 지정해서 지우고 `rmdir`.

## 4. 남은 작업 (우선순위 순)

### M7 SSH — 리뷰 3차에서 넘어온 High (먼저 처리)
| 이슈 | 내용 |
|---|---|
| HDIR-73 | 원격 삭제·편집 시 `pwd` 끝 개행 보존 (센티널). 이름이 개행으로 끝나는 디렉터리에서 다른 경로를 건드릴 수 있음 |
| HDIR-74 | 하드링크 파일 제자리 쓰기 전 백업, 실패 시 복원 |
| HDIR-75 | 편집 중 원격 파일 동시 변경 감지 → 교체 중단, 로컬 편집본 보존 |
| HDIR-76 | 원격 편집 교체 시 원본 GID 보존 |
| HDIR-77 | 원격 디렉터리 미리보기 경량화, 다중 삭제 거부 테스트 보강 (medium) |
| HDIR-78 | `tests/ssh_real.rs` 병렬 실행 시 간헐 exit 255 (`--test-threads=1`이면 통과) (medium) |

### M8 scp 전송
| 이슈 | 내용 |
|---|---|
| HDIR-45 | F5/F6 라우팅 4종: 로컬↔로컬(fs_ops, 이미 동작), 로컬→원격, 원격→로컬, 원격↔원격(`scp -3`). 현재 원격이 끼면 "원격: 다음 단계에서 지원" |
| HDIR-46 | 이동 = 전송 성공 확인 후 원본 삭제 (원격 `rm`, 로컬 휴지통) |
| HDIR-47 | 진행률 (원격 `du`로 총량, 항목별) |
| HDIR-48 | 전송 전 대상 충돌 확인 → 기존 충돌 dialog 재사용 |
| HDIR-49 | Esc 취소 (scp kill), 부분 파일 안내 |
| HDIR-51 | Docker openssh 통합 테스트 환경 (low) |

### M9 문서·릴리스
HDIR-52 README 설치·설정 예시 전체, HDIR-53 release 빌드.

### 낮은 우선순위
HDIR-56 term 테스트 구조, HDIR-59 xattr 보존, HDIR-63 fd 기반 순회 TOCTOU, HDIR-79 job 진행률 테스트 간헐 실패.

### Windows (Backlog, 지원 예정)
HDIR-66~72. 사용자 결정으로 보류. 착수하지 말 것 (요청이 오면 진행).

## 5. 구조 지도

| 파일 | 역할 |
|---|---|
| `main.rs`, `cli.rs`, `term.rs` | 인자(`--cwd-file`, START_DIR), 이벤트 루프, 터미널 init/restore/suspend (`term::reenter`로 커서 질의 없이 복귀) |
| `app.rs` | 앱 상태·키 디스패치·BgTask·job·dialog·각종 플로우 (가장 큼) |
| `panel.rs`, `loader.rs`, `vfs.rs` | 패널 목록·탐색, 비동기 로딩(패널당 1+1, 최대 4), Local/Ssh Vfs |
| `fs_ops.rs`, `platform.rs`, `job.rs`, `undo.rs` | 안전한 복사·이동(O_EXCL 임시, no-replace rename, 백업), 플랫폼 원시 함수, 워커, undo |
| `trash.rs` | OS 휴지통만 사용 (Linux `trash::os_limited`, macOS Finder) |
| `ui.rs`, `dialog.rs`, `menu.rs`, `screen.rs`, `viewer.rs` | 렌더링, dialog, F9 메뉴, F1 도움말 등 화면, F3 뷰어 |
| `config.rs`, `theme.rs`, `keymap.rs` | config.toml(toml_edit 저장), 테마(기본 mdir), 키맵(mdir/vim 프리셋) |
| `preview.rs`, `assoc.rs`, `launcher.rs`, `editor.rs`, `cmdline.rs` | 미리보기, 확장자 연결, 프로세스 실행, $EDITOR, `:` 명령줄 |
| `servers.rs`, `session.rs`, `remote.rs`, `remote_ops.rs` | ssh config·서버 목록, ControlMaster 세션, 원격 명령·scp argv·파서, 원격 파일 작업 |
| `shell/hdir.{zsh,bash,fish}` | `h` 함수 (종료 시 cd) |
| `themes/*.toml` | 내장 테마 |
| `tests/` | smoke, shell(셸 함수 E2E), linux_trash(XDG 격리 실제 휴지통), ssh_real(`HDIR_SSH_HOST`) |

## 6. 꼭 알아야 할 결정·함정

- **휴지통은 OS 기능만 사용** (사용자 결정). 자체 인덱스·freedesktop 직접 구현 금지. trash crate `restore_all`은 디렉터리 symlink를 복원 못 함(IsADirectory, Linux 실측) → symlink 항목 복원 거부 가드.
- **Windows는 지원 예정**: 빌드·테스트만 유지, 어려운 기능은 "Windows 지원 예정" 메시지.
- **scp (OpenSSH 9+ SFTP 모드)**: 원격 경로는 따옴표 없이 raw, 다운로드 원본의 `* ? [ ] \`는 backslash escape. legacy 모드(-O)는 stage 서버에서 실패 → 미검증. `remote::ScpMode`, `scp_mode_from_version`.
- **TUI 복귀**: `Terminal::clear()`는 커서 위치를 질의해 느린 터미널에서 실패 → `term::reenter`가 Terminal을 새로 만듦. 다시 `clear()` 쓰지 말 것.
- **ssh 소켓 경로 길이**: macOS 캐시 경로가 길면 `/tmp/hdir-<uid>/` 사용 (소유자·0700·symlink 아님 검증).
- **ssh_real 테스트**: `HDIR_SSH_HOST=choi@stage.devfor.link cargo test --test ssh_real -- --ignored --test-threads=1`. 서버의 `mktemp -d` 안에서만 작업하고 만든 것만 정리.
- **Linux 휴지통 테스트**는 `XDG_DATA_HOME`을 임시 폴더로 바꿔 실제 사용자 휴지통을 건드리지 않음.
- **Plane rate limit**: 429면 30초 대기 (`scripts/plane.sh`가 처리). 웹 모듈 화면이 비어 보이면 강력 새로고침.
- **macOS 휴지통 테스트 잔여물**: 이전 머신 Finder 휴지통에 `hdir-trash-test-<pid>.txt` 1개가 남아 있을 수 있음 (권한 문제로 미삭제).
- `.reviews/`, `.claude/worktrees/`, `.env`는 git 무시.

## 7. 링크

- GitHub: https://github.com/hunchulchoi/hdir (PR #1~#12 merge)
- Plane: https://plane.devfor.link/zodiac/projects/2574e50f-48e1-473e-9f73-4823cd87aee0/ (모듈 M1~M10, 페이지 "hdir MVP 구현 계획")
