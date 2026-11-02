# hdir

DOS 시절 Mdir의 키보드 조작감과 현대 셸 기능을 합친 개발자용 듀얼 패널 TUI 파일 관리자. Rust + Ratatui.

- Mdir식 F키 조작, 좌우 듀얼 패널, 텍스트·이미지 정보 미리보기
- 종료할 때 셸이 마지막 폴더로 `cd` (zsh / bash / fish)
- 휴지통 삭제와 되돌리기(Ctrl+Z), 휴지통 보기·복원
- 확장자 연결: 확장자·패턴별 실행 명령
- 테마: 기본 Mdir 파란 화면, 내장 테마, 사용자 테마
- SSH 원격 패널: 목록·보기·편집·삭제, 패널 간 scp 복사·이동, 서버별 테마

## 지원 플랫폼

| | 상태 |
|---|---|
| macOS, Linux | 지원 |
| Windows | 빌드·기본 탐색만. 휴지통 보기, PowerShell 셸 통합, 확장자 실행, SSH 등은 **지원 예정** (화면에 안내) |

## 설치

Rust 1.88 이상이 필요합니다 ([rustup](https://rustup.rs)).

```sh
git clone https://github.com/hunchulchoi/hdir.git
cd hdir
cargo install --path .        # ~/.cargo/bin/hdir 설치
# 또는
cargo build --release         # target/release/hdir
```

`hdir`가 `PATH`에 있으면 됩니다. SSH 기능은 시스템의 `ssh`·`scp`(OpenSSH)를 그대로 씁니다.

```
hdir [--cwd-file PATH] [START_DIR]
  --cwd-file PATH  종료 시 활성 패널의 폴더를 PATH에 기록 (셸 통합용)
  -h, --help / -V, --version
```

## 셸 통합

`h`로 실행하면, 종료할 때 활성 패널의 폴더로 셸이 이동합니다. F10/`q`는 이동하며 종료, Shift+F10/Ctrl+X는 이동 없이 종료합니다.

```sh
# ~/.zshrc
source /path/to/hdir/shell/hdir.zsh
# ~/.bashrc
source /path/to/hdir/shell/hdir.bash
# ~/.config/fish/config.fish
source /path/to/hdir/shell/hdir.fish
```

`:`를 누르면 명령줄이 열립니다. `cd <경로>`, `cd -`, `cd`는 hdir 안에서 처리하고, 그 밖의 명령은 `$SHELL -c`로 실행합니다(원격 패널에서는 그 서버에서 실행).

## 기본 키 (`mdir` 프리셋)

| 키 | 동작 | 키 | 동작 |
|---|---|---|---|
| F1 | 도움말 | F2 / Shift+F6 | 이름 바꾸기 |
| F3 | 보기 | F4 | 편집 (`$EDITOR`) |
| F5 | 복사 | F6 | 이동 |
| F7 | 새 폴더 | F8 | 삭제 (휴지통) |
| Alt+F8 | 휴지통 보기 | F9 | 메뉴 |
| F10 / `q` | 종료 (cd) | Shift+F10 / Ctrl+X | 종료 (cd 없음) |
| Tab | 패널 전환 | Space / Insert | 선택 |
| Enter / Backspace | 들어가기 / 상위 | `.` | 숨김 파일 표시 |
| Ctrl+Z | 되돌리기 | Ctrl+T | 클래식/개발자 모드 |
| Shift+Enter / Ctrl+O | 연결 프로그램 선택 | `:` | 명령줄 |
| Alt+F1 / Alt+F2 | 왼쪽/오른쪽 패널 위치 (로컬·서버) | Esc | 진행 중인 작업 취소 |

F키가 터미널에서 막히면 Alt+숫자(Alt+3 = F3 …)로도 됩니다. `vim` 프리셋은 여기에 `j k h l g G v e y m a d r u ?` 같은 한 글자 키를 더합니다.

## 설정

설정 파일은 다음 위치에 있습니다. 없으면 기본값으로 동작합니다.
- macOS·Linux: `$XDG_CONFIG_HOME/hdir/config.toml`, 없으면 `~/.config/hdir/config.toml`
- Windows: `%APPDATA%\hdir\config.toml`

잘못된 값이 있어도 hdir는 멈추지 않습니다. 해당 부분만 기본값으로 돌아가고, 상태줄에 경고가 뜹니다(F9 메뉴에서 전체 경고 보기). 메뉴에서 설정을 다시 읽거나 테마를 바꿀 수 있습니다.

전체 예시입니다(이 블록은 테스트에서 경고 없이 읽히는지 검사합니다).

<!-- example: config.toml -->
```toml
# 테마: 모드별 (classic = 기본 화면, dev = Ctrl+T 개발자 모드)
[theme]
classic = "my-blue"     # 아래 사용자 테마
dev = "dark"

# 키: 프리셋을 고른 뒤 동작별로 덮어쓴다. 동작 이름은 snake_case.
# 값은 키 하나 또는 목록, []는 그 동작을 끈다.
[keys]
preset = "mdir"         # "mdir" | "vim"

[keys.bind]
copy = ["F5", "c"]
delete = ["F8", "Delete"]
quit = ["F10", "Ctrl+Q"]
toggle_mode = "Ctrl+T"

[keys.viewer]
search = "/"

# 확장자 연결: Enter로 실행할 명령. 위에서부터 먼저 맞는 규칙.
# {} = 파일 경로, {dir} = 폴더, {name} = 확장자를 뺀 이름.
# 자리표시자를 하나도 안 쓰면 경로가 끝에 붙는다.
[[assoc]]
ext = ["md", "txt"]
cmd = "less {}"

[[assoc]]
ext = ["png", "jpg", "pdf"]
cmd = "open {}"          # Linux는 "xdg-open {}"
background = true        # 기다리지 않고 띄운다

[[assoc]]
glob = "Makefile*"
cmd = "make -f {}"
wait = true              # 끝난 뒤 키 입력을 기다린다

[[assoc]]
ext = ["py"]
cmd = "python3 {}"
name = "Python 실행"      # Shift+Enter 메뉴에 보이는 이름

# 서버: Alt+F1/Alt+F2 목록. ~/.ssh/config의 Host 별칭도 자동으로 나온다.
[[servers]]
name = "prod"
host = "deploy@prod.example.com"   # ssh 대상 (별칭 또는 user@host)
port = 22
path = "/srv/app"                  # 연결 직후 열 폴더 (없으면 홈)
theme = "mdir-red"                 # 이 서버를 보는 패널의 테마

[[servers]]
match = "stg-*"                    # host 없이 match만: ssh 별칭에 테마만 지정
theme = "mdir-green"
```

### 테마

내장 테마: `mdir`(기본), `mdir-red`, `mdir-green`, `dark`, `light`.

사용자 테마는 설정 폴더의 `themes/*.toml`에 둡니다(예: `~/.config/hdir/themes/my-blue.toml`). 같은 이름이면 내장 테마보다 우선합니다. `base`를 지정하면 적지 않은 색은 그 테마에서 가져옵니다. 색은 `#RRGGBB`이고, 256색 터미널에서는 가장 가까운 색으로 바뀝니다.

<!-- example: themes/my-blue.toml -->
```toml
name = "my-blue"
base = "mdir"

[colors]
bg = "#00005F"
cursor_bg = "#5F87AF"
cursor_fg = "#FFFFFF"
selected = "#FFD75F"

# 확장자별 파일 색
[ext_colors.rust]
exts = ["rs", "toml"]
color = "#FFAF5F"
```

색 이름 전체는 [`themes/mdir.toml`](themes/mdir.toml)을 참고하세요.

## SSH 원격 패널

Alt+F1(왼쪽) / Alt+F2(오른쪽)로 로컬이나 서버를 고릅니다. 목록은 `[[servers]]`와 `~/.ssh/config`(`Include` 포함)의 Host 별칭을 합친 것입니다.

- 연결은 OpenSSH ControlMaster 하나를 패널들이 함께 씁니다. 비밀번호나 호스트 키 확인이 필요하면 한 번만 터미널에서 묻습니다.
- 연결이 끊기면 패널에 "연결 끊김"이 뜹니다. Enter로 다시 연결합니다.
- F3 보기, F4 편집: 편집은 내려받아 로컬 `$EDITOR`로 열고 저장하면 다시 올립니다. 편집하는 동안 원격 파일이 바뀌었거나, 다른 사용자가 쓸 수 있는 폴더라 바꿔치기될 수 있으면 올리지 않고 로컬 사본을 남깁니다.
- F7 폴더, F2 이름 바꾸기, F8 삭제를 지원합니다. 원격 삭제는 휴지통 없이 영구 삭제이며, 홈·`/`와 그 상위 폴더는 거부합니다.
- F5/F6 복사·이동은 패널 조합에 따라 방식이 다릅니다.

  | 출발 → 도착 | 방식 |
  |---|---|
  | 로컬 ↔ 서버 | `scp` |
  | 같은 서버의 두 패널 | 서버 안에서 복사, 이동은 이름 바꾸기 |
  | 서버 A → 서버 B | `scp -3` (두 서버 모두 키 인증이 되어야 함) |

  - 각 항목은 대상 폴더 안의 임시 폴더로 먼저 받고, 다 받은 뒤에 제자리로 옮깁니다. 그래서 Esc로 취소하거나 연결이 끊겨도 기존 파일은 그대로이고 부분 파일도 남지 않습니다.
  - 이미 있는 이름은 덮어쓰기를 묻습니다. 폴더끼리 겹치면 합치지 않고 건너뜁니다.
  - scp는 심볼릭 링크를 따라가기 때문에, 링크가 들어 있는 항목은 건너뛰고 경고합니다.
  - 이동은 사본이 제자리에 있고 원본이 그동안 바뀌지 않았을 때만 원본을 지웁니다. 로컬 원본은 휴지통으로, 원격 원본은 영구 삭제합니다.
  - 다른 사용자가 바꿀 수 있는 대상 폴더에는 보내지 않습니다.

원격 서버는 POSIX `sh`와 기본 유틸리티(`find`, `stat`, `du`, `mktemp`)가 있는 Linux·macOS·BSD를 가정합니다. Linux가 아닌 서버에서는 `/tmp` 같은 공유(sticky) 폴더로 보낼 수 없습니다.

## 개발

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
# 실제 ssh 왕복 (키 인증이 되는 서버)
HDIR_SSH_HOST=user@host cargo test --test ssh_real -- --ignored
# 같은 테스트를 일회용 OpenSSH 컨테이너로 (Docker 필요)
scripts/ssh-docker-test.sh
```

- [구현 계획](docs/plan.md) · [초기 구상](docs/mvp계획.md) · [개발 룰](CLAUDE.md) · [인수인계](docs/HANDOFF.md)
