# hdir

DOS 시절 Mdir의 키보드 조작감과 현대 Bash/Zsh 기능을 합친 개발자용 듀얼 패널 TUI 파일 관리자. Rust + Ratatui로 만든다.

> 개발 중 (MVP 단계). 아직 실행 가능한 빌드는 없다.

## 계획된 기능
- Mdir식 탐색과 F키 조작 (F3 보기, F4 편집, F5 복사, F6 이동, F7 폴더, F8 삭제, F10 종료)
- 좌우 듀얼 패널, 텍스트 미리보기, 클래식/개발자 모드 전환
- 종료 시 셸의 현재 디렉터리 동기화 (zsh/bash 함수)
- 확장자 연결: 확장자별 실행 명령 지정
- 테마: 기본 Mdir 파란 화면, 사용자 테마
- SSH 원격 패널 + scp 파일 전송, 서버별 테마

## 지원 플랫폼
- macOS, Linux: 지원
- Windows: 빌드는 되지만 기능은 **지원 예정** (휴지통 보기, PowerShell 셸 통합, 확장자 실행, SSH 등)

## 셸 통합
`h`로 hdir를 실행하고, 종료할 때 활성 패널의 디렉터리로 셸이 `cd`한다. 종료 키는 F10/`q`(cd 함), Shift+F10/Ctrl+X(cd 없이 종료).

```sh
# ~/.zshrc
source /path/to/hdir/shell/hdir.zsh
# ~/.bashrc
source /path/to/hdir/shell/hdir.bash
# ~/.config/fish/config.fish
source /path/to/hdir/shell/hdir.fish
```

직접 쓰려면 `hdir --cwd-file PATH [START_DIR]`: 종료 시 PATH에 디렉터리를 기록한다 (cd 없이 종료하면 빈 파일). `:` 명령줄에서 `cd <경로>`/`cd -`/`cd`는 내부 처리, 그 외는 `$SHELL -c`로 실행한다. Windows는 지원 예정.

## 문서
- [구현 계획](docs/plan.md)
- [초기 구상](docs/mvp계획.md)
- [개발 룰](CLAUDE.md)
