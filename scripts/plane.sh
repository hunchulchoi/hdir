#!/usr/bin/env bash
# Plane 이슈 헬퍼. 이슈는 sequence 번호(HDIR-NN의 NN)로 지정한다.
#
#   scripts/plane.sh state   <NN> <backlog|todo|progress|done|cancelled>
#   scripts/plane.sh comment <NN> "<text>"
#   scripts/plane.sh status                 # 모듈별 이슈 상태 출력
#
# 필요: plane CLI (~/.config/plane 에 API 키·워크스페이스), 리포 루트의 .env (PLANE_PROJECT).
# rate limit(429)이면 30초 대기 후 재시도한다.
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
set -a; . "$root/.env"; set +a
: "${PLANE_PROJECT:?PLANE_PROJECT missing in .env}"

retry() {
  local out n
  for n in 1 2 3 4 5 6 7 8; do
    if out=$("$@" 2>&1); then printf '%s\n' "$out"; return 0; fi
    if grep -q RATE_LIMIT <<<"$out"; then sleep 30; else printf '%s\n' "$out" >&2; return 1; fi
  done
  return 1
}

issues_json() { retry plane --format json issues list -p "$PLANE_PROJECT" --all-pages; }

issue_id() {
  issues_json | python3 -I -c '
import json, sys
o = sys.stdin.read(); o = o[min(i for i in (o.find("{"), o.find("[")) if i >= 0):]
j = json.loads(o); rows = j.get("results", j) if isinstance(j, dict) else j
seq = int(sys.argv[1])
print(next(x["id"] for x in rows if x.get("sequence_id") == seq))' "$1"
}

state_id() {
  case "$1" in
    backlog)   echo 163c484e-7f59-46b6-8f6d-eb2a5cef1814 ;;
    todo)      echo 1dba0214-c797-40d6-8d9a-d406b0ddc4bf ;;
    progress)  echo c602bd20-e8a4-4b05-ab17-39cd1bae36dd ;;
    done)      echo 6df7a108-aebb-4dca-98ab-6092627cfa20 ;;
    cancelled) echo c5df0cd3-c2e2-4889-9ca3-f70cd1a59935 ;;
    *) echo "unknown state: $1" >&2; exit 2 ;;
  esac
}

case "${1:-}" in
  state)   retry plane issues update -p "$PLANE_PROJECT" --state "$(state_id "$3")" "$(issue_id "$2")" | head -1 ;;
  comment) retry plane comments add -p "$PLANE_PROJECT" -i "$(issue_id "$2")" "$3" | head -1 ;;
  status)
    mods=$(retry plane --format json modules list -p "$PLANE_PROJECT")
    issues=$(issues_json)
    for mid in $(python3 -I -c '
import json, sys
o = sys.argv[1]; o = o[min(i for i in (o.find("{"), o.find("[")) if i >= 0):]
j = json.loads(o); rows = j.get("results", j) if isinstance(j, dict) else j
for m in sorted(rows, key=lambda m: int(m["name"].split()[0][1:])): print(m["id"])' "$mods"); do
      members=$(retry plane --format json modules issues -p "$PLANE_PROJECT" -m "$mid" --all-pages)
      python3 -I - "$mods" "$issues" "$members" "$mid" <<'EOF'
import json, sys
def rows(o):
    o = o[min(i for i in (o.find("{"), o.find("[")) if i >= 0):]
    j = json.loads(o); return j.get("results", j) if isinstance(j, dict) else j
names = {
    "163c484e-7f59-46b6-8f6d-eb2a5cef1814": "Backlog", "1dba0214-c797-40d6-8d9a-d406b0ddc4bf": "Todo",
    "c602bd20-e8a4-4b05-ab17-39cd1bae36dd": "In Progress", "6df7a108-aebb-4dca-98ab-6092627cfa20": "Done",
    "c5df0cd3-c2e2-4889-9ca3-f70cd1a59935": "Cancelled",
}
mods, issues, members, mid = rows(sys.argv[1]), rows(sys.argv[2]), rows(sys.argv[3]), sys.argv[4]
by = {x["id"]: x for x in issues}
print("##", next(m["name"] for m in mods if m["id"] == mid))
for m in sorted(members, key=lambda x: x.get("sequence_id", 0)):
    x = by.get(m["id"], m); s = x["state"] if isinstance(x["state"], str) else x["state"]["id"]
    print(f'  HDIR-{x["sequence_id"]}\t{names.get(s, "?")}\t{x["name"]}')
EOF
    done ;;
  *) sed -n '2,9p' "$0"; exit 2 ;;
esac
