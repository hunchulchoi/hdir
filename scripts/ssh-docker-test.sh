#!/usr/bin/env bash
# Run tests/ssh_real.rs against a throwaway OpenSSH server in Docker.
# Nothing touches ~/.ssh: a fresh key pair lives in a temp dir for one run,
# and the client container has its own known_hosts.
#
#   scripts/ssh-docker-test.sh
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
compose=(docker compose -f "$root/tests/docker/compose.yml" -p hdir-ssh)
# Under target/ (gitignored, inside the home dir that colima/Docker Desktop share).
mkdir -p "$root/target"
keys="$(mktemp -d "$root/target/ssh-docker.XXXXXX")"
cleanup() {
  HDIR_KEYS="$keys" "${compose[@]}" down --remove-orphans >/dev/null 2>&1 || true
  rm -f -- "$keys/id" "$keys/id.pub"
  rmdir -- "$keys"
}
trap cleanup EXIT

ssh-keygen -q -t ed25519 -N '' -C hdir-docker-test -f "$keys/id"
chmod 644 "$keys/id.pub"
# the client runs as root inside its container; the key file must be readable there
chmod 644 "$keys/id"
HDIR_KEYS="$keys" "${compose[@]}" up --build --abort-on-container-exit --exit-code-from client
