#!/usr/bin/env bash
#
# End-to-end proof that the system does what it claims.
#
# Fully self-contained: it creates a throwaway organisation on its own ports,
# enrols two peers, and checks that a message written on one arrives on the
# other by direct peer-to-peer sync. It then checks the parts that are supposed
# to say *no*. Nothing here touches ./seed-data or any server you have running.
#
# Every step prints what it did, so you can audit the result rather than trust
# a green tick.
#
# Usage:  scripts/verify.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HTTP_PORT="${VERIFY_HTTP_PORT:-8477}"
P2P_PORT="${VERIFY_P2P_PORT:-4477}"
WORK="$(mktemp -d)"
URL="http://127.0.0.1:$HTTP_PORT"

MAIN_PID=$$
PIDS=()
cleanup() {
  [[ $$ -eq $MAIN_PID ]] || return 0
  for pid in ${PIDS[@]+"${PIDS[@]}"}; do kill "$pid" 2>/dev/null || true; done
  rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

PASSED=0
FAILED=0
pass() { printf '  \033[1;32m✓\033[0m %s\n' "$*"; PASSED=$((PASSED + 1)); }
fail() { printf '  \033[1;31m✗\033[0m %s\n' "$*"; FAILED=$((FAILED + 1)); }
step() { printf '\n\033[1;36m%s\033[0m\n' "$*"; }

SEED="$ROOT/target/release/seed-server"
CLI="$ROOT/target/release/p2p-cli"
if [[ ! -x "$SEED" || ! -x "$CLI" ]]; then
  step "Building release binaries (one time)"
  (cd "$ROOT" && cargo build --release -p seed-server -p p2p-cli)
fi

# ---------------------------------------------------------------- 1. the org
step "1. Create a throwaway organisation"
"$SEED" --data-dir "$WORK/seed" init --org-name "Verify Org" >"$WORK/init.log" 2>&1
ORG=$(grep -E '^\s+org id' "$WORK/init.log" | awk '{print $3}')
TOKEN=$("$SEED" --data-dir "$WORK/seed" token)
[[ -n "$ORG" ]] && pass "org created: ${ORG:0:12}…" || fail "no org id"

"$SEED" --data-dir "$WORK/seed" run \
  --http-port "$HTTP_PORT" --tcp-port "$P2P_PORT" --quic-port "$P2P_PORT" \
  >"$WORK/server.log" 2>&1 &
PIDS+=($!)
UP=no
for _ in $(seq 1 40); do
  curl -fsS --max-time 2 "$URL/health" >/dev/null 2>&1 && { UP=yes; break; }
  sleep 0.5
done
[[ "$UP" == yes ]] && pass "seed server answering on $URL" || { fail "server never came up"; exit 1; }

# ------------------------------------------------------------ 2. admin gate
step "2. The admin API must refuse anonymous callers"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$URL/v1/admin/invites" \
  -H 'Content-Type: application/json' -d '{"user_id":"mallory"}')
[[ "$CODE" == "401" ]] && pass "no token -> HTTP 401" || fail "no token -> HTTP $CODE (expected 401)"

mint() {
  curl -fsS -X POST "$URL/v1/admin/invites" -H "Authorization: Bearer $TOKEN" \
    -H 'Content-Type: application/json' -d "{\"user_id\":\"$1\",\"role\":\"${2:-member}\"}" \
    | python3 -c "import json,sys; print(json.load(sys.stdin)['invite_code'])"
}
ADA_CODE=$(mint ada)
BOB_CODE=$(mint bob)
[[ -n "$ADA_CODE" && -n "$BOB_CODE" ]] && pass "with token -> two invites minted" || fail "could not mint invites"

# -------------------------------------------------------------- 3. enrolment
step "3. Enrol two devices"
# Typed the way a person would: grouped and lower-cased. The server must
# normalise it back to the stored token.
TYPED=$(printf '%s' "$ADA_CODE" | fold -w5 | paste -sd- - | tr 'A-Z' 'a-z')
if "$CLI" --data-dir "$WORK/ada" enroll --url "$URL" --code "$TYPED" --name ada >"$WORK/ada-enroll.log" 2>&1; then
  pass "ada joined using a dashed, lower-cased code ($TYPED)"
else
  fail "ada could not enrol"; cat "$WORK/ada-enroll.log"
fi
"$CLI" --data-dir "$WORK/bob" enroll --url "$URL" --code "$BOB_CODE" --name bob >/dev/null 2>&1 \
  && pass "bob joined" || fail "bob could not enrol"

if "$CLI" --data-dir "$WORK/mallory" enroll --url "$URL" --code "$ADA_CODE" --name mallory >/dev/null 2>&1; then
  fail "a spent invite code was accepted a second time"
else
  pass "a spent invite code is refused"
fi

# ------------------------------------------------- 4. the actual replication
step "4. Two devices sync peer-to-peer"
"$CLI" --data-dir "$WORK/bob" run --watch >"$WORK/bob.log" 2>&1 &
PIDS+=($!)
"$CLI" --data-dir "$WORK/ada" run >"$WORK/ada.log" 2>&1 &
PIDS+=($!)

LINKED=no
for _ in $(seq 1 60); do
  grep -q "bob (member) joined" "$WORK/ada.log" 2>/dev/null && { LINKED=yes; break; }
  sleep 0.5
done
[[ "$LINKED" == yes ]] && pass "ada and bob authenticated each other" || fail "the peers never connected"

# Written through a *separate* sqlite3 client, so this also proves the capture
# triggers see writes the app makes with its own database handle.
MSG="verified at $(date +%H:%M:%S)"
if python3 - "$WORK" "$MSG" <<'PY'
import pathlib, subprocess, sys, time
work, msg = pathlib.Path(sys.argv[1]), sys.argv[2]
boblog = work / "bob.log"
t0 = time.time()
subprocess.run(["sqlite3", str(work / "ada" / "replica.db"),
    "INSERT INTO messages (id, room, author, author_name, body, sent_at_ms) "
    f"VALUES ('v1','general','ada','ada','{msg}', {int(time.time()*1000)});"], check=True)
while time.time() - t0 < 30:
    if msg in boblog.read_text():
        print(f"  \033[1;32m✓\033[0m bob received it in {time.time()-t0:.2f}s "
              f"(written by a plain sqlite3 client, never through the server)")
        sys.exit(0)
    time.sleep(0.05)
print("  \033[1;31m✗\033[0m the message never arrived")
sys.exit(1)
PY
then PASSED=$((PASSED + 1)); else FAILED=$((FAILED + 1)); fi

# grep -c exits 1 on a zero count, which is the answer we want, not an error.
REFUSALS=$(grep -c "! refused" "$WORK/bob.log" 2>/dev/null || true)
[[ "${REFUSALS:-0}" == "0" ]] && pass "no connection churn (0 refusals in bob's log)" \
  || fail "bob logged $REFUSALS refusals"

# ------------------------------------------------------------ 5. revocation
step "5. Revoking a device is published in a signed list"
"$SEED" --data-dir "$WORK/seed" revoke --serial 1 >/dev/null 2>&1
if curl -fsS "$URL/v1/crl" | python3 -c '
import json,sys
d = json.load(sys.stdin)
ok = d["revoked"] == [1] and bool(d["signature"])
print(("  \033[1;32m✓\033[0m" if ok else "  \033[1;31m✗\033[0m"),
      "revocation list names serial 1 and is signed" if ok else f"unexpected list: {d}")
sys.exit(0 if ok else 1)
'
then PASSED=$((PASSED + 1)); else FAILED=$((FAILED + 1)); fi

# ------------------------------------------------------------------ summary
step "Summary"
printf '  %d passed, %d failed\n\n' "$PASSED" "$FAILED"
[[ "$FAILED" -eq 0 ]] || exit 1
