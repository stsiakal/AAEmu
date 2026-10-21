#!/usr/bin/env bash
# Two `lane.sh new` runs started together must not pick the same port block, and the registry
# check must ignore CRLF line endings. Run: bash Scripts/tests/lane-port-race.sh
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
registry=$root/lanes
bin=$root/bin
mkdir -p "$registry" "$bin"

# Every port looks free, and each check takes long enough for two selections to overlap.
printf '#!/usr/bin/env bash\nsleep 0.2\nexit 0\n' > "$bin/netstat"
chmod +x "$bin/netstat"
export PATH="$bin:$PATH"
export AAEMU_LANES=$registry AAEMU_WT_ROOT=$root/wt AAEMU_TARGET_ROOT=$root/target
# shellcheck disable=SC1091
source "$here/../lane.sh"

fail() { echo "FAIL: $*" >&2; exit 1; }

# CRLF registry entries still mark their prefix as taken.
rm -rf "$registry" && mkdir -p "$registry"
printf 'LANE=crlf\r\nPORT_PREFIX=24\r\n' > "$registry/crlf.env"
pick_prefix || fail "CRLF case: no block selected"
[ "$NEW_RESERVED" != 24 ] || fail "CRLF case: picked the prefix in crlf.env"
rmdir "$registry/.port-$NEW_RESERVED"

# Concurrent selections against an empty registry must reserve different blocks.
for round in 1 2 3; do
  rm -rf "$registry" && mkdir -p "$registry"
  ( pick_prefix; printf '%s\n' "$NEW_RESERVED" ) > "$root/a" &
  local_a=$!
  ( pick_prefix; printf '%s\n' "$NEW_RESERVED" ) > "$root/b" &
  local_b=$!
  wait "$local_a" "$local_b"
  a=$(cat "$root/a"); b=$(cat "$root/b")
  [ -n "$a" ] && [ -n "$b" ] || fail "round $round: a selection found no block (A='$a' B='$b')"
  [ "$a" != "$b" ] || fail "round $round: both selections reserved $a"
  [ -d "$registry/.port-$a" ] && [ -d "$registry/.port-$b" ] \
    || fail "round $round: reservations missing for $a and $b"
done

echo "ok: CRLF prefix skipped, concurrent selections reserved different blocks in 3 rounds"
