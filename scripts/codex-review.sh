#!/usr/bin/env bash
# Codex 병렬 리뷰: 3개 관점을 동시에 실행하고 결과를 .reviews/<branch>/ 에 저장한다.
# 사용법: scripts/codex-review.sh [base-branch]   (기본: main)
set -euo pipefail

base="${1:-main}"
root="$(git rev-parse --show-toplevel)"
branch="$(git rev-parse --abbrev-ref HEAD)"
out="$root/.reviews/${branch//\//_}"
mkdir -p "$out"

run() {
  local name="$1" focus="$2"
  codex review --base "$base" --title "hdir review: $name" \
    "이 저장소는 Rust TUI 파일 관리자 hdir다. CLAUDE.md 룰을 기준으로 리뷰하라. 집중 관점: $focus. 각 지적은 'severity(Critical/High/Medium/Low) | 파일:라인 | 문제 | 수정 제안' 형식으로." \
    >"$out/$name.md" 2>&1 \
    && echo "done: $name" || echo "FAILED: $name (see $out/$name.md)"
}

cd "$root"
run correctness "정확성, 버그, 엣지케이스(빈 디렉터리, 권한 오류, 심볼릭 링크, 한글/공백 파일명)" &
run tests "테스트 커버리지, TDD 준수, 누락된 테스트, 테스트 품질" &
run security-perf "보안(셸 인젝션, 경로 이스케이프, 비밀 노출)과 성능(UI 스레드 블로킹, 불필요한 IO)" &
wait

echo "results: $out"
ls -1 "$out"
