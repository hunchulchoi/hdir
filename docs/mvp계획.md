이거 꽤 재미있는 프로젝트가 될 것 같아. DOS 시절 Mdir의 조작감에 현대적인 Bash/Zsh 기능을 합치는 것이 핵심이라고 봐.

단순히 파일 관리자를 만드는 것보다는 Mdir 감성의 개발자용 터미널 워크스페이스로 만들면 차별화할 수 있어.

## 1. 화면 구성

■ MDiR 26

zsh · macOS · UTF-8

LEFT / ~/Projects

▸ src/

▸ docker/

▸ scripts/

package.json

compose.yml

[README.md](http://README.md)

RIGHT / ~/Backup

▸ projects/

▸ archives/

backup.tar.gz

[notes.md](http://notes.md)

PREVIEW · src/

12 files · 3 directories · Git main ✓

$ ~/Projects git status

On branch main · working tree clean

F3 보기

F4 편집

F5 복사

F6 이동

F7 폴더

F8 삭제

F9 메뉴

F10 종료

화면 콘셉트 예시 — 클래식 Mdir 색감과 2패널 구조, 명령 실행창의 결합

## 2. 넣으면 좋은 기능


| 우선순위 | 기능        | 설명                           |
| ---- | --------- | ---------------------------- |
| P0   | 클래식 파일 탐색 | 방향키, Enter, Backspace, F키 조작 |
| P0   | 듀얼 패널     | 좌우 디렉터리 간 복사·이동              |
| P0   | 파일 미리보기   | 코드, Markdown, JSON, 이미지, 로그  |
| P0   | 셸 통합      | 종료 시 현재 셸의 디렉터리 변경           |
| P0   | 파일 작업     | 복사, 이동, 이름 변경, 휴지통, 실행 취소    |
| P1   | 빠른 검색     | `fzf`, `fd`, `ripgrep` 연동    |
| P1   | 압축파일 탐색   | ZIP, TAR 내부를 폴더처럼 탐색         |
| P1   | 디렉터리 즐겨찾기 | 북마크와 `zoxide` 방문 기록 활용       |
| P1   | Git 상태    | 변경·추가·무시 파일 표시, diff 미리보기    |
| P1   | 일괄 이름 변경  | `$EDITOR`에서 파일명 목록을 편집 후 반영  |
| P2   | 원격 파일 관리  | SSH/SFTP로 서버 파일 탐색·전송        |
| P2   | Docker 탐색 | 컨테이너 내부 파일시스템 접근             |
| P2   | 디렉터리 비교   | 양쪽 패널의 변경 파일 비교 및 동기화        |
| P2   | 사용자 명령    | 스크립트 및 외부 도구 단축키 등록          |


## 3. 차별화할 만한 기능

기존 Yazi에도 미리보기, Git 연동, 대량 이름 변경, 검색, 플러그인 기능 등이 있어.

![](https://www.google.com/s2/favicons?domain=https://github.com&sz=32)

GitHub

따라서 단순한 TUI 파일 관리자보다 다음 세 가지를 강조하면 좋겠어.



① 셸과 완전히 연결되는 Mdir

Mdir 안에서 `cd`, Git, Docker 명령을 실행하고 다시 파일 관리 화면으로 돌아오게 하는 거야. 나갈 때는 마지막으로 탐색하던 경로가 실제 Bash/Zsh의 현재 경로가 되도록 하고.

② 개발자 특화 파일 관리자

프로젝트에 들어가면 `package.json`, `Cargo.toml`, `pom.xml`, `compose.yaml` 등을 인식해서 프로젝트별 명령을 제공하는 거지. 예를 들어 Node 프로젝트에서는 `npm run` 스크립트를, Docker 프로젝트에서는 Compose 명령을 바로 선택할 수 있어.

③ 원격 서버용 Mdir

SSH 서버에 연결하면 로컬과 원격 디렉터리를 좌우 패널로 표시하고, 파일 전송·권한 변경·로그 확인을 할 수 있게 하는 거야. 서버 운영용으로 특히 활용도가 높겠지.

## 4. 구현 방식

내가 권하는 구성은 다음과 같아.

Bash / Zsh

실행 함수 · 현재 디렉터리 동기화 · 명령 기록

Rust + Ratatui

파일 탐색 · 키보드 입력 · TUI 렌더링 · 비동기 작업

파일시스템

Git / Docker

SSH / SFTP

Bash/Zsh만으로 모든 TUI를 만드는 것도 가능하지만, 터미널 입력 처리나 다양한 파일명 지원, 대용량 디렉터리 탐색이 복잡해져. 실제 프로그램은 Rust로, 셸 통합은 Bash/Zsh 함수로 분리하는 편이 좋아.

## 5. MVP 범위

처음에는 파일 탐색, 듀얼 패널, F키, 복사·이동·휴지통, 텍스트 미리보기, 셸 디렉터리 동기화만 구현해도 충분해. 그다음 Git과 검색, 마지막으로 SSH와 Docker를 추가하면 돼.

그리고 중요한 건 Mdir 특유의 키보드 조작감과 즉각적인 반응을 살리는 거야. 기능을 많이 넣느라 느려지면 오히려 매력이 없어져.

개인적으로는 하나 더 넣고 싶어. Mdir 클래식 모드와 현대적인 개발자 모드를 단축키 하나로 전환하는 기능. 클래식 모드에서는 DOS 감성의 파란 화면과 F키를, 개발자 모드에서는 파일 미리보기·Git 상태·명령 팔레트를 제공하는 거지.

이렇게 만들면 단순한 추억용 복제품이 아니라 실제로 매일 쓸 만한 도구가 될 것 같아.