# hdir MVP 구현 계획

## Context
`docs/mvp계획.md`: DOS Mdir 조작감 + 현대 Bash/Zsh 통합한 개발자용 TUI 파일 관리자 구상.
저장소는 빈 상태(Initial commit, `docs/mvp계획.md`, `.env`만 존재). Rust 1.97 / zsh 환경 확인됨.
목표: 문서 §5 MVP 범위 구현 — 파일 탐색, 듀얼 패널, F키, 복사·이동·휴지통, 텍스트 미리보기, 셸 디렉터리 동기화.
핵심 요구: Mdir식 키보드 조작감 + 즉각 반응(느려지면 안 됨). 클래식/개발자 모드 토글도 MVP에 가볍게 포함.
추가 범위: 확장자 연결, 테마(기본 Mdir 컬러), SSH 원격 패널 + scp 전송, 서버별 테마. Git·검색·Docker는 이후 단계.

## 기술 스택
- Rust (edition 2021) 단일 바이너리 `hdir`
- `ratatui` + `crossterm` — TUI/입력
- `trash` — macOS/Linux 휴지통
- `anyhow` — 에러, `dirs` — 홈 경로, `unicode-width` — 한글 파일명 폭 계산
- `serde` + `toml` — 설정 파일(확장자 연결·테마·서버)
- SSH/전송: 시스템 `ssh`/`scp` 바이너리 호출 (`~/.ssh/config`·agent·ProxyJump 그대로 사용, 네이티브 crate 불필요)
- dev: `tempfile` — fs 작업 테스트

## 디렉터리 구조
```
Cargo.toml
.gitignore            # target/, .env
src/
  main.rs     # 인자(--cwd-file <path>), 터미널 setup/restore, panic hook, 이벤트 루프
  app.rs      # App: panels[2], active, mode(Classic|Dev), dialog, status, job 수신
  panel.rs    # Panel: location(Local|Remote), cwd, entries, cursor, scroll, selected, 정렬, 숨김 토글, theme override
  vfs/
    mod.rs    # trait Vfs { list, read_head, mkdir, rename, remove, is_remote }
    local.rs  # 로컬 구현
    ssh.rs    # 원격 구현: ControlMaster 세션 위 ssh 명령
  transfer.rs # 패널 간 복사/이동 라우팅: local↔local=fs_ops, local↔remote=scp, remote↔remote=scp -3
  servers.rs  # 서버 목록: ~/.ssh/config Host 파싱 + config [[servers]] 병합
  fs_ops.rs   # 로컬 copy(재귀)/move(rename→실패 시 copy+remove)/mkdir/rename/trash, undo 스택
  job.rs      # 백그라운드 스레드 + mpsc로 복사·이동 진행률 전달 (UI 블로킹 방지)
  preview.rs  # 텍스트 앞부분 N KB, 바이너리 감지(NUL 바이트), 디렉터리 요약(파일/폴더 수)
  dialog.rs   # 확인(Y/N), 입력(새 폴더명, 이름 변경, 대상 경로) 프롬프트
  ui.rs       # 헤더 / 좌우 패널 / 미리보기(Dev) / 명령줄 / F키 바 렌더링, 테마
  keymap.rs   # Action enum + 기본 키맵(mdir 프리셋) + 사용자 [keys] 병합, 키 문자열 파서
  config.rs   # ~/.config/hdir/config.toml 로드(없으면 기본값), 파싱 오류 시 상태줄 경고 + 기본값
  assoc.rs    # 확장자 연결: 확장자/glob → 명령 템플릿 매칭, 실행
  theme.rs    # Theme 구조체(색 슬롯), 내장 프리셋, 사용자 테마 로드
themes/       # 내장 테마 toml (include_str!로 바이너리 포함)
  mdir.toml  dark.toml  light.toml
shell/
  hdir.zsh    # 셸 함수 (아래)
  hdir.bash
```

## 기능별 구현
1. **탐색** — ↑↓ PgUp/PgDn Home/End, Enter(디렉터리 진입), Backspace(상위, 이전 위치로 커서 복원), Tab(패널 전환), Space/Insert(선택 토글), `.`(숨김 토글).
   디렉터리 로딩은 `read_dir` + `symlink_metadata`; 권한 오류 시 상태줄 표시하고 유지.
2. **듀얼 패널** — 좌우 독립 cwd. 복사·이동 대상 기본값 = 반대 패널 cwd (dialog에서 수정 가능).
3. **F키**
   | 키 | 동작 |
   |---|---|
   | F3 | 보기: 전체화면 텍스트 뷰어 (스크롤, q 종료) |
   | F4 | 편집: TUI suspend → `$EDITOR`(기본 vi) 실행 → 복귀 후 새로고침 |
   | F5/F6 | 복사/이동 → 반대 패널 (선택 항목 or 커서 항목) |
   | F7 | 폴더 생성, Shift+F6 또는 F2: 이름 변경 |
   | F8 | 휴지통 (확인 dialog) |
   | F9 | 메뉴: 모드 전환·정렬·숨김 토글 등 |
   | F10 | 종료 (cwd 기록) |
   | Ctrl+Z | undo (move/rename/mkdir만; 휴지통 복원은 안내만) |
   | Ctrl+O 또는 `:` | 명령줄 |
4. **미리보기** — Dev 모드에서 오른쪽 하단/우측 영역. 커서 이동 시 동기 읽기하되 64KB 상한 + 첫 200줄만 → 즉각 반응 유지.
5. **명령줄(셸 연결 ①의 최소판)** — `:` 입력 후 명령 실행. `cd <path>`는 내부 처리(활성 패널 이동). 그 외는 TUI suspend → `$SHELL -c` 를 활성 패널 cwd에서 실행 → "아무 키나 누르세요" → 복귀·새로고침.
6. **셸 디렉터리 동기화** — 종료 시 활성 패널 cwd를 `--cwd-file` 경로에 기록. 셸 함수:
   ```zsh
   h() {
     local tmp; tmp=$(mktemp -t hdir.XXXXXX)
     command hdir --cwd-file "$tmp" "$@"
     local dir; dir=$(<"$tmp"); rm -f "$tmp"
     [[ -n $dir && -d $dir && $dir != $PWD ]] && cd -- "$dir"
   }
   ```
   bash 버전 동일 로직. Ctrl+Q 등 "cd 없이 종료" 키도 제공.
7. **모드 토글 (F9 메뉴 + Ctrl+T)** — 모드 = 레이아웃, 색은 테마가 담당
   - Classic: 2패널 + F키 바만.
   - Dev: 2패널 + 미리보기 + 명령줄 강조. (Git 상태/명령 팔레트 자리만 확보, 내용은 P1)
   - 두 모드 모두 기본 테마 `mdir`. 모드별 테마는 설정에서 변경 가능.
8. **확장자 연결 (Mdir 확장자 실행 기능)**
   - 파일에서 Enter → 연결 규칙 찾아 실행. 없으면: 텍스트면 F3 뷰어, 아니면 `open`(macOS)/`xdg-open`.
   - 설정 예:
     ```toml
     [[assoc]]
     ext = ["md", "txt", "rs", "toml"]
     cmd = "$EDITOR {}"
     [[assoc]]
     ext = ["png", "jpg", "pdf"]
     cmd = "open {}"
     background = true        # TUI 유지, 결과 기다리지 않음
     [[assoc]]
     ext = ["sh"]
     cmd = "bash {}"
     wait = true              # 실행 후 "아무 키나" 대기
     [[assoc]]
     glob = "Dockerfile*"
     cmd = "docker build -f {} ."
     ```
   - 템플릿 변수: `{}` 파일 경로(셸 이스케이프), `{dir}` 디렉터리, `{name}` 확장자 뺀 이름. 실행은 활성 패널 cwd에서 `$SHELL -c`.
   - 매칭 순서: glob → ext(대소문자 무시) → 기본 동작. 첫 매칭 사용.
   - Shift+Enter 또는 F9 메뉴 "연결 프로그램 선택": 매칭되는 규칙 목록 중 골라 실행.
   - 파일 목록에서 연결된 확장자는 테마 색(`assoc_file`)으로 구분, 실행 파일(+x)도 별도 색.
9. **테마**
   - 색 슬롯: `bg, fg, border, border_active, title, dir, file, exec, symlink, hidden, assoc_file, selected, cursor_bg, cursor_fg, status_bg, status_fg, fkey_num, fkey_label, dialog_bg, preview_bg`
   - 색 표기: `"blue"` 같은 이름, `"#1e3a8a"` hex, `"236"` 256색 인덱스. 누락 슬롯은 base 테마 값 상속(`base = "dark"`).
   - 내장: **`mdir`(기본값)**, `dark`, `light`.
   - `mdir` 테마 = DOS 16색(CGA/VGA) 팔레트 hex 고정값으로 Mdir 화면 재현:
     | 슬롯 | 색 |
     |---|---|
     | bg | 파랑 `#0000AA` |
     | fg / file | 밝은 회색 `#AAAAAA` |
     | border, border_active | 밝은 청록 `#55FFFF` / 흰색 `#FFFFFF` |
     | dir | 흰색 굵게 `#FFFFFF` |
     | exec (+x, .sh/.command) | 밝은 초록 `#55FF55` |
     | 압축(.zip/.tar/.gz) | 밝은 자홍 `#FF55FF` |
     | 문서/소스(assoc_file) | 밝은 청록 `#55FFFF` |
     | hidden | 진회색 `#555555` |
     | selected | 노랑 `#FFFF55` |
     | cursor_bg / cursor_fg | 청록 `#00AAAA` / 검정 `#000000` |
     | status_bg / status_fg | 청록 `#00AAAA` / 검정 |
     | fkey_num / fkey_label | 흰색 on 검정 / 검정 on 청록 |
     | dialog_bg | 회색 `#AAAAAA` + 검정 글자 (DOS 대화상자) |
   - 확장자 그룹별 색(exec/압축/문서) 지원 위해 테마에 `[ext_colors]` 테이블 추가: `archive = ["zip","tar","gz"]` 처럼 그룹 → 확장자 목록 → 색.
   - 값은 Mdir III 스크린샷 대조로 구현 시 미세 조정.
   - 사용자 테마: `~/.config/hdir/themes/*.toml`. 설정 `theme.classic = "mdir"`, `theme.dev = "mdir"` 기본.
   - 내장 추가: `mdir-red`(빨강 `#AA0000` 배경 — 운영 서버 경고용), `mdir-green`(초록 `#00AA00` — 스테이징용). 둘 다 `base = "mdir"`로 bg/border만 변경.
   - **서버별 테마**: 패널 단위 적용. 원격 패널은 해당 서버 테마, 로컬 패널은 전역 테마 → 한 화면에서 좌(로컬 파랑)/우(prod 빨강) 구분.
     헤더·상태줄·F키 바는 활성 패널의 테마를 따름 (지금 어느 서버에서 조작 중인지 즉시 인지).
   - F9 메뉴 "테마 선택": 목록에서 커서 이동 시 즉시 미리 적용, Enter 확정(설정 파일에 저장), Esc 원복.
   - truecolor 미지원 터미널(`COLORTERM` 확인)은 hex → 256색 근사 변환.
11. **SSH 패널 (좌/우 각각 로컬 또는 원격)**
   - 연결: Alt+F1(좌)/Alt+F2(우) → 위치 선택 목록(로컬 + 서버 목록), 또는 명령줄 `:ssh prod:/var/www`. `:local`로 복귀.
   - 서버 목록 = `~/.ssh/config`의 Host(와일드카드 제외) + `config.toml`의 `[[servers]]`:
     ```toml
     [[servers]]
     name  = "prod-web"        # 목록 표시명
     host  = "prod-web"        # ssh alias 또는 user@host
     port  = 22                # 선택
     path  = "/var/www"        # 접속 시 시작 경로
     theme = "mdir-red"        # 서버별 테마
     [[servers]]
     match = "stg-*"           # glob: ssh config 호스트 일괄 지정
     theme = "mdir-green"
     ```
     테마 결정 순서: name 일치 → match glob → 전역 테마.
   - 세션: 첫 연결 시 TUI suspend 후 `ssh -o ControlMaster=yes -o ControlPersist=10m -o ControlPath=~/.cache/hdir/cm-%C -fN host` 실행 → 비밀번호·호스트키 확인을 일반 터미널에서 처리. 이후 모든 명령은 같은 ControlPath + `BatchMode=yes`로 재인증 없이 즉시 실행.
   - 연결 직후 `uname -s`로 원격 OS 감지 → 목록 명령 선택:
     - Linux: `find <dir> -mindepth 1 -maxdepth 1 -printf '%y\t%s\t%T@\t%m\t%f\0'`
     - BSD/macOS: `stat -f` 기반 대체 스크립트
     - 경로는 항상 셸 이스케이프, 출력은 NUL 구분 → 공백·한글 파일명 안전.
   - 디렉터리 로딩은 워커 스레드에서 비동기, 로딩 중 패널에 `읽는 중…` 표시 → 네트워크 지연 중에도 반대 패널 조작 가능.
   - 원격 파일 작업: F7 `mkdir -p`, 이름 변경 `mv`, F8 `rm -rf` (원격은 휴지통 없음 → 대화상자에 "영구 삭제" 빨간 경고 + 서버명 표시).
   - 미리보기/F3: `head -c 65536`. F4 편집: 임시 파일로 scp 다운로드 → `$EDITOR` → 수정 시각 변경되면 scp 업로드.
   - 확장자 연결: 원격 파일은 임시 다운로드 후 연결 명령 실행.
   - 명령줄: 원격 활성 패널에서 `:` 명령은 `ssh -t host 'cd <cwd> && <cmd>'`로 실행.
   - 연결 끊김: ControlMaster 소켓 확인 실패 시 상태줄 경고 + Enter로 재연결.
   - cwd 동기화: 종료 시 활성 패널이 원격이면 로컬 패널 cwd를 기록(로컬 셸은 원격 경로로 cd 불가).
12. **scp 파일 전송**
   - F5/F6이 패널 위치 조합에 따라 자동 라우팅 (`transfer.rs`):
     | 원본 → 대상 | 방법 |
     |---|---|
     | 로컬 → 로컬 | fs_ops |
     | 로컬 → 원격 | `scp -rp -o ControlPath=… src host:dst` |
     | 원격 → 로컬 | `scp -rp host:src dst` |
     | 원격 → 원격 | `scp -3 -rp hostA:src hostB:dst` |
   - F6 이동 = scp 성공 확인 후 원본 삭제 (원격이면 `rm -rf`, 로컬이면 휴지통).
   - 진행률: 선택 항목별 scp 실행 → `전송 중 3/12 · prod-web` 표시, 시작 전 원격 `du -sb`로 총 크기 계산해 바이트 진행률 근사.
   - 덮어쓰기 충돌: 대상 존재 여부 사전 확인 → 기존 충돌 dialog 재사용.
   - Esc로 전송 취소(scp 프로세스 kill, 부분 파일 남음을 알림).
12-1. **키 설정 (키맵 변경)**
   - 모든 조작은 `Action` enum(`cursor_up`, `enter`, `parent`, `switch_panel`, `toggle_select`, `view`, `edit`, `copy`, `move`, `mkdir`, `rename`, `delete`, `menu`, `quit`, `quit_no_cd`, `undo`, `command_line`, `toggle_mode`, `toggle_hidden`, `connect_left`, `connect_right`, `open_with` …)로 정의. 키 → Action은 테이블 조회만 → 하드코딩 금지.
   - 내장 프리셋: `mdir`(기본, 위 F키 표), `vim`(hjkl·`/`·`yy`→copy 등 단일키 위주). `keys.preset = "mdir"`.
   - 사용자 재정의 (`config.toml`):
     ```toml
     [keys]
     preset = "mdir"
     [keys.bind]
     copy        = ["F5", "c"]          # 여러 키 바인딩
     delete      = ["F8", "Delete"]
     quit        = ["F10", "Ctrl+Q"]
     toggle_mode = "Ctrl+T"
     view        = []                    # 빈 배열 = 비활성화
     ```
   - 키 문자열 문법: `Ctrl+`/`Alt+`/`Shift+` 수식어 + `F1`~`F12`, `Enter`, `Esc`, `Tab`, `Backspace`, `Delete`, `Insert`, `Home`, `End`, `PageUp`, `PageDown`, `Up`/`Down`/`Left`/`Right`, `Space`, 단일 문자. 대소문자 무시.
   - 검증: 알 수 없는 action/키 → 상태줄 경고 + 해당 항목 무시. 같은 키가 두 action에 바인딩 → 경고, 사용자 정의가 프리셋보다 우선.
   - 문맥별 키맵: `panel`(기본), `viewer`(F3), `dialog`, `menu` 섹션 분리 (`[keys.viewer]` 등). Esc/Enter는 dialog에서 고정(잠금 방지).
   - F키 바·F1 도움말 화면은 현재 키맵에서 자동 생성 → 재정의가 화면에 즉시 반영.
   - F9 메뉴 "키 설정 보기": 현재 바인딩 목록 + 설정 파일 경로 표시, `$EDITOR`로 열기 → 저장 후 자동 재로드.
   - 터미널이 못 받는 키(macOS Terminal의 F키 등) 대비: `mdir` 프리셋에 F키와 함께 대체키(Alt+숫자: Alt+5=copy) 기본 포함.
13. **긴 작업** — copy/move는 `job.rs` 워커 스레드에서 실행, 상태줄에 `복사 중 3/12 · 45%`. 이벤트 루프는 `crossterm::event::poll(50ms)`로 키 입력과 job 메시지 동시 처리. 덮어쓰기 충돌 시 dialog(덮어쓰기/건너뛰기/모두).

## 구현 순서 (마일스톤)
0. **GitHub 공개 리포 + Plane 등록** (아래 "GitHub 등록"·"Plane 등록 계획" 섹션) — 코드 작업 전에 먼저 실행
1. 프로젝트 scaffold, `.gitignore`(target/, `.env` — 현재 미무시 상태), 터미널 setup/restore + 단일 패널 탐색
2. 듀얼 패널 + Tab + 선택 + Mdir 색 + F키 바
3. fs_ops (mkdir/rename/trash/copy/move) + dialog + undo + 단위 테스트
4. job 스레드 진행률 표시
5. F3 뷰어, F4 편집, 미리보기, Dev 모드 토글
6. config.rs + keymap 설정(프리셋·재정의·검증·F1 도움말) + theme.rs: 내장 테마(기본 mdir), 확장자 그룹 색, 사용자 테마, 테마 선택 메뉴 (마일스톤 2의 하드코딩 색을 테마 슬롯으로 교체)
7. assoc.rs: 확장자 연결 + Enter 기본 동작 + 연결 프로그램 선택
8. 명령줄 + `--cwd-file` + `shell/hdir.{zsh,bash}`
9. Vfs trait 추출(로컬 코드 이전) → ssh.rs: ControlMaster 연결, 목록, 미리보기, 원격 파일 작업
10. servers.rs + 위치 선택(Alt+F1/F2) + 서버별 테마 적용
11. transfer.rs: scp 라우팅 4종, 진행률, 취소, 원격 편집(다운로드→편집→업로드)
12. README: 설치·설정 예시(확장자 연결, 테마, 서버)

## 검증
- GitHub: `gh repo view hunchulchoi/hdir --json visibility,url` → PUBLIC, 웹에서 `.env` 없음 확인
- 룰: `/agents`에 hdir-impl(sonnet)·hdir-ops(haiku) 표시, `scripts/codex-review.sh main` 시험 실행 → `.reviews/` 3개 파일 생성
- Plane: `plane pages get` 로 생성 페이지 내용 확인
- 설정 파일 없이 첫 실행 → Mdir 파란 화면으로 시작
- `cargo test` — panel 정렬/선택, fs_ops 복사·이동·이름변경·undo를 `tempfile` 임시 디렉터리에서 검증
  - assoc: glob/ext 매칭 우선순위, 대소문자, 공백·따옴표 포함 파일명 이스케이프
  - theme: hex/이름/인덱스 파싱, base 상속, 잘못된 값 → 기본값 + 경고
  - servers: ssh config 파싱(와일드카드 제외, Include), 서버 테마 결정 순서
  - ssh: `find -printf` / `stat -f` 출력 파서(NUL·탭·한글·공백 파일명), scp 명령 인자 생성(4종 라우팅, 이스케이프)
- SSH 통합: `localhost` 대상(macOS 원격 로그인 켜기) 또는 Docker `linuxserver/openssh-server` 컨테이너로
  - 연결 → 목록 → 좌로컬/우원격 F5 업로드, F5 다운로드, 원격↔원격(서버 2개 띄움) `scp -3`
  - 원격 F8 영구 삭제 경고, 원격 파일 F4 편집 후 업로드 반영
  - 서버별 테마: prod 패널 빨강, 로컬 패널 파랑 동시 표시, Tab 전환 시 상태줄 색 변경
  - 네트워크 지연 중(`tc`/큰 디렉터리) 반대 패널 조작 가능한지
- `cargo clippy -- -D warnings`
- 수동: `cargo build --release` → `source shell/hdir.zsh` → `h` 실행
  - 방향키/Enter/Backspace/Tab 동작, 한글 파일명 정렬·폭 정상
  - F5/F6로 반대 패널 복사·이동, F8 후 Finder 휴지통 확인, Ctrl+Z로 이동 되돌림
  - 큰 파일(~1GB) 복사 중 키 입력 반응 유지
  - `:git status` 실행 후 복귀, `:cd src` 내부 이동
  - F10 종료 후 셸 `pwd`가 마지막 경로인지 확인
  - Ctrl+T로 Classic ↔ Dev 전환
  - `[keys.bind] copy = "c"` 설정 후 `c`로 복사, F키 바·F1 도움말에 반영, 중복 바인딩 경고 표시
  - `.md` Enter → `$EDITOR`, `.png` Enter → 미리보기 앱 열림 + TUI 유지, 규칙 없는 확장자 → 기본 동작
  - F9 → 테마 선택에서 커서 이동 시 즉시 색 변경, Esc 원복, Enter 후 재실행해도 유지
  - 사용자 테마 파일 추가 후 목록에 표시

## 프로젝트 룰 (첫 커밋에 포함)
생성 파일: `CLAUDE.md`, `.claude/agents/hdir-impl.md`, `.claude/agents/hdir-ops.md`, `scripts/codex-review.sh`

### CLAUDE.md 내용
**1. TDD 필수**
- 모든 기능 이슈: Red(실패 테스트 먼저 작성·실패 확인) → Green(최소 구현) → Refactor.
- 테스트 없는 프로덕션 코드 커밋 금지. 예외: TUI 렌더링 순수 레이아웃 코드 → `ratatui::backend::TestBackend` 스냅샷 테스트로 대체.
- 로직은 렌더링/IO와 분리(순수 함수·trait)해서 단위 테스트 가능하게. 원격·외부 명령은 명령 생성부와 실행부 분리 → 생성부 테스트.
- 커밋 전 게이트: `cargo fmt --check && cargo clippy -- -D warnings && cargo test` 전부 통과.

**2. 체크포인트 커밋**
- 커밋 시점: (a) Green+Refactor 완료로 테스트 통과하는 의미 단위, (b) 이슈 완료, (c) 리뷰 반영 완료. Red 상태 커밋 금지.
- 브랜치: 모듈 단위 `m<N>-<slug>` (예 `m1-core-tui`). `main` 직접 커밋 금지(초기 문서 커밋 제외). 모듈 완료 → PR → 리뷰 → merge.
- 메시지: Conventional Commits, 본문에 Plane 이슈 ID (`Refs: HDIR-12`), 끝에 Co-Authored-By 라인.
- `.env` 등 비밀 파일 커밋 금지, 커밋 전 `git diff --cached --name-only` 확인.

**3. Plane 업데이트**
- 프로젝트: `.env` `PLANE_PROJECT`. state ID: Todo `1dba0214-c797-40d6-8d9a-d406b0ddc4bf`, In Progress `c602bd20-e8a4-4b05-ab17-39cd1bae36dd`, Done `6df7a108-aebb-4dca-98ab-6092627cfa20`, Cancelled `c5df0cd3-c2e2-4889-9ca3-f70cd1a59935`.
- 이슈 착수 → In Progress. 체크포인트 커밋마다 이슈 코멘트(커밋 SHA·요약·테스트 결과). PR 생성 → 코멘트에 PR URL. merge → Done.
- 계획 변경·새 작업 발견 → 이슈 추가 후 해당 모듈에 연결, Plane 페이지 계획 갱신.

**4. 에이전트 협업 (역할 분담)**
| 담당 | 모델 | 역할 |
|---|---|---|
| 메인 세션 | Opus | 이슈 분해, 설계 판단, 서브에이전트 지시, 결과 통합, 리뷰 결과 판정, 최종 커밋·PR |
| `hdir-impl` 서브에이전트 | Sonnet | 이슈 단위 TDD 구현(테스트→구현→리팩터). 독립 이슈는 `isolation: worktree`로 병렬 |
| `hdir-ops` 서브에이전트 | Haiku | Plane 상태·코멘트 갱신, 게이트(fmt/clippy/test) 실행·요약, README·문서, 단순 수정 |
| Codex | codex CLI | 병렬 코드 리뷰(필수), 어려운 부분(ssh 출력 파서, scp 라우팅) 대안 구현·세컨드 오피니언 `codex exec -s read-only` |
- 서브에이전트 지시엔 이슈 ID·완료 조건·관련 파일·TDD 룰을 명시. 결과는 메인이 diff 검토 후 채택.
- 동일 파일 동시 수정 금지: 병렬 작업은 파일 겹치지 않는 이슈끼리만.

**5. Codex 병렬 리뷰 (PR마다 필수, 큰 체크포인트에도 선택)**
- `scripts/codex-review.sh <base>`: 아래 3개를 백그라운드 병렬 실행, 결과를 `.reviews/<branch>/*.md`(gitignore)에 저장
  - `codex review --base <base> "정확성·버그·엣지케이스 중심"`
  - `codex review --base <base> "테스트 커버리지·TDD 준수·누락 테스트 중심"`
  - `codex review --base <base> "보안(셸 인젝션·경로 이스케이프·비밀 노출)·성능(UI 블로킹) 중심"`
- 동시에 Claude `cavecrew-reviewer` 1개로 교차 검토.
- 메인(Opus)이 지적사항 판정: Critical/High → 반드시 수정 후 재리뷰, Medium → 수정 또는 Plane 이슈 등록, 오탐 → PR 코멘트에 사유 기록.
- 리뷰 요약을 PR 본문과 Plane 이슈 코멘트에 기록.

### `.claude/agents/hdir-impl.md`
frontmatter `model: sonnet`, tools: Read/Edit/Write/Grep/Glob/Bash. 본문: CLAUDE.md TDD 룰, 이슈 하나만 처리, 게이트 통과 후 변경 파일·테스트 결과 보고, 커밋·Plane 갱신은 하지 않음(메인이 수행).

### `.claude/agents/hdir-ops.md`
frontmatter `model: haiku`, tools: Bash/Read/Edit. 본문: plane CLI 사용법(`set -a; . ./.env`), state ID 표, 게이트 실행 요약 포맷, 코드 로직 수정 금지.

## GitHub 등록
현재: 빈 Initial commit만 존재, `.env`·`docs/` 미추적, remote 없음. gh 로그인(hunchulchoi, repo scope) OK, `hunchulchoi/hdir` 이름 비어 있음.
1. `.gitignore` 작성: `.env`, `.env.*`, `target/`, `.DS_Store`, `.reviews/` → `git check-ignore .env`로 무시 확인 (**공개 리포라 `.env` 절대 커밋 금지**)
2. `README.md` 초안(프로젝트 소개 1단락 + 계획 문서 링크), `docs/mvp계획.md`, `docs/plan.md`(이 계획 사본), 프로젝트 룰 파일들 커밋
   - 커밋: `docs: add MVP plan and project scaffold docs` + Co-Authored-By 라인
   - 커밋 전 `git diff --cached --name-only`로 `.env` 미포함 재확인
3. `gh repo create hunchulchoi/hdir --public --source . --remote origin --description "Mdir-style dual-panel TUI file manager with shell, SSH/scp integration" --push`
4. 리포 URL을 Plane 페이지 상단과 각 모듈 description에 링크
5. 이후 마일스톤별 커밋은 `main`에서 바로 하지 않고 모듈 단위 브랜치(`m1-core-tui` 등) → PR. Plane 이슈 ID를 커밋/PR 본문에 표기.

## Plane 등록 계획
대상: `.env` `PLANE_PROJECT` (2574e50f…, 현재 모듈·이슈 0개, bot 계정 member). 모든 이슈 state = Todo(`1dba0214-…`).
스크립트 1개(scratchpad의 `plane_seed.sh`)로 순차 실행, 생성 ID를 `--format json`으로 받아 모듈 연결. 중간 실패 시 생성된 ID 목록 보고(중복 생성 방지 위해 재실행 전 `modules list`/`issues list` 확인).

**1) Page** — `plane pages create -p $PLANE_PROJECT --name "hdir MVP 구현 계획" --file plan.html`
   - 이 plan 전체를 Markdown → HTML(표·코드블록 유지) 변환 후 업로드.
   - 사전 확인에서 `pages list` 403 → create도 403이면 페이지만 건너뛰고 모듈·이슈는 계속, 권한(프로젝트 Pages 기능 활성/역할) 요청 보고.

**2) Module + Issue** — 모듈 = 마일스톤 묶음. 이슈 설명은 plain text로 "작업 내용 / 완료 조건 / 관련 파일" 3단 구성 (위 기능 설명에서 발췌).

| Module | Issue (priority) |
|---|---|
| **M1 코어 TUI·탐색** | 프로젝트 scaffold·Cargo 의존성·.gitignore (high) · 터미널 setup/restore·panic hook·이벤트 루프 (high) · Panel 디렉터리 로딩·정렬·숨김 토글·권한오류 처리 (high) · 키 탐색(↑↓/PgUp/Home/Enter/Backspace 커서 복원) (high) · 듀얼 패널·Tab 전환·Space 선택 (high) · 레이아웃: 헤더/패널/F키 바 렌더링, 한글 폭 처리 (medium) |
| **M2 파일 작업** | fs_ops 복사(재귀)·이동(cross-fs 대체) (high) · F7 mkdir·F2 이름변경·F8 휴지통 + dialog (high) · 덮어쓰기 충돌 dialog (medium) · undo 스택(Ctrl+Z) (medium) · job 워커 스레드·진행률 상태줄·취소 (high) · fs_ops 단위 테스트(tempfile) (medium) |
| **M3 뷰어·미리보기·모드** | F3 전체화면 텍스트 뷰어 (medium) · F4 `$EDITOR` suspend/resume (medium) · 미리보기 패널(64KB/200줄, 바이너리 감지, 디렉터리 요약) (medium) · Classic/Dev 모드 레이아웃 + Ctrl+T·F9 메뉴 (medium) |
| **M4 설정·테마·키** | config.rs: config.toml 로드·기본값·오류 경고 (high) · 키 문자열 파서·Action enum·프리셋(mdir/vim) (high) · `[keys.bind]` 재정의·문맥별 키맵·중복/오류 검증 (high) · F키 바·F1 도움말 키맵 자동 생성, 키 설정 보기·재로드 (medium) · Theme 슬롯·색 파싱(이름/hex/256)·base 상속 (high) · 내장 테마 mdir(기본)·mdir-red·mdir-green·dark·light (high) · 확장자 그룹 색 `[ext_colors]` (medium) · 테마 선택 메뉴(즉시 미리보기/Esc 원복/저장) (medium) · truecolor 미지원 256색 근사 (low) |
| **M5 확장자 연결** | assoc 규칙 파싱·매칭(glob→ext→기본) (high) · 명령 템플릿 `{}`/`{dir}`/`{name}` 이스케이프·실행(wait/background) (high) · Enter 기본 동작(텍스트→뷰어, 그외 open) (medium) · 연결 프로그램 선택 메뉴 (low) · assoc 단위 테스트 (medium) |
| **M6 셸 통합** | 명령줄 `:`·내부 `cd`·`$SHELL -c` 실행 후 복귀 (high) · `--cwd-file` 종료 시 cwd 기록·Ctrl+Q cd 없이 종료 (high) · shell/hdir.zsh·hdir.bash 함수 (high) |
| **M7 SSH 원격 패널** | Vfs trait 추출·로컬 구현 이전 (high) · ControlMaster 연결(suspend 후 인증)·끊김 감지·재연결 (high) · 원격 OS 감지 + 목록(find -printf / stat -f) NUL 파서 (high) · 비동기 원격 로딩 `읽는 중…` (high) · 원격 mkdir/mv/rm(영구 삭제 경고) (medium) · 원격 미리보기·F3 (medium) · 원격 F4 편집(다운로드→편집→업로드)·원격 확장자 연결 (medium) · 원격 명령줄 `ssh -t` (low) · servers.rs: ssh config 파싱 + `[[servers]]` 병합 (high) · Alt+F1/F2 위치 선택·`:ssh host:/path`·`:local` (high) · 서버별 테마(패널 단위, 활성 패널 기준 상태줄) (high) |
| **M8 scp 전송** | transfer 라우팅 4종(local↔local/local↔remote/remote↔local/remote↔remote `-3`) (high) · F6 이동 = 전송 성공 후 원본 삭제 (high) · 진행률(`du -sb` 총량, 항목별) (medium) · 전송 전 대상 충돌 확인 (medium) · Esc 취소·부분 파일 안내 (medium) · ssh/scp 인자 생성·파서 단위 테스트 (medium) · Docker openssh 컨테이너 통합 테스트 환경 (low) |
| **M9 문서·릴리스** | README: 설치·셸 함수·config 예시(assoc/테마/서버) (medium) · `cargo build --release`·clippy 무경고 정리 (low) |

총 9 모듈 / 약 55 이슈. 생성 후 모듈별 이슈 수 표로 보고.
