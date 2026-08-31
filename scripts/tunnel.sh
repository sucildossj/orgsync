#!/usr/bin/env bash
#
# Runs the seed server behind a public tunnel, so phones can enrol from any
# network without deploying anything.
#
# There are two separate things a phone needs from the server, and most tunnel
# products only carry the first:
#
#   1. The enrolment API — ordinary HTTPS. Any HTTP tunnel handles it.
#   2. Rendezvous and relay — raw TCP carrying libp2p. Needs a TCP tunnel.
#
# So this script sets up whatever your installed tools can do and tells you
# plainly which of the two you ended up with. With HTTP only you still get a
# fully working system on a shared network, because devices find each other by
# mDNS and talk directly; what you lose is two phones on *different* networks
# reaching each other.
#
# Routing through someone else's tunnel does not expose the org's data: libp2p
# authenticates the remote peer id with a Noise handshake and encrypts end to
# end, so the tunnel operator carries ciphertext it cannot read or forge.
#
# Usage:  scripts/tunnel.sh [--data-dir DIR] [--http-port N] [--tcp-port N]
set -euo pipefail

DATA_DIR="${DATA_DIR:-./seed-data}"
HTTP_PORT="${HTTP_PORT:-8080}"
TCP_PORT="${TCP_PORT:-4001}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --data-dir)  DATA_DIR="$2"; shift 2 ;;
    --http-port) HTTP_PORT="$2"; shift 2 ;;
    --tcp-port)  TCP_PORT="$2";  shift 2 ;;
    -h|--help)   sed -n '2,25p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

mkdir -p "$DATA_DIR"
DATA_DIR="$(cd "$DATA_DIR" && pwd)"
LOG_DIR="$DATA_DIR/logs"
mkdir -p "$LOG_DIR"

# Backgrounded children inherit this trap. Without the guard, one child exiting
# would run the whole cleanup — killing its siblings and deleting the logs that
# explain why it died.
MAIN_PID=$$
PIDS=()
cleanup() {
  [[ $$ -eq $MAIN_PID ]] || return 0
  for pid in ${PIDS[@]+"${PIDS[@]}"}; do kill "$pid" 2>/dev/null || true; done
}
trap cleanup EXIT INT TERM

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m  %s\n' "$*"; }
die()  { printf '\033[1;31mxx\033[0m  %s\n' "$*" >&2; exit 1; }

BIN="$ROOT/target/release/seed-server"
if [[ ! -x "$BIN" ]]; then
  say "Building the seed server (release)…"
  (cd "$ROOT" && cargo build --release -p seed-server)
fi

if [[ ! -f "$DATA_DIR/seed.db" ]]; then
  say "Creating the organisation in $DATA_DIR"
  "$BIN" --data-dir "$DATA_DIR" init --org-name "${ORG_NAME:-My Organisation}"
fi

# --- raw TCP tunnel, for rendezvous and relay -------------------------------
ANNOUNCE_ARGS=()
P2P_REACHABLE="no"

if command -v ngrok >/dev/null 2>&1; then
  say "Starting an ngrok TCP tunnel for libp2p on port ${TCP_PORT}…"
  ngrok tcp "$TCP_PORT" --log stdout --log-format logfmt >"$LOG_DIR/ngrok.log" 2>&1 &
  PIDS+=($!)
  ENDPOINT=""
  for _ in $(seq 1 40); do
    ENDPOINT=$(curl -s http://127.0.0.1:4040/api/tunnels 2>/dev/null \
      | sed -n 's/.*"public_url":"tcp:\/\/\([^"]*\)".*/\1/p' | head -1)
    [[ -n "$ENDPOINT" ]] && break
    sleep 0.5
  done
  if [[ -n "$ENDPOINT" ]]; then
    ANNOUNCE_ARGS=(--announce "/dns4/${ENDPOINT%%:*}/tcp/${ENDPOINT##*:}")
    P2P_REACHABLE="yes"
    say "libp2p reachable at tcp://$ENDPOINT"
  else
    warn "ngrok started but exposed no TCP endpoint; see $LOG_DIR/ngrok.log"
  fi
elif command -v bore >/dev/null 2>&1; then
  say "Starting a bore TCP tunnel for libp2p on port ${TCP_PORT}…"
  bore local "$TCP_PORT" --to bore.pub >"$LOG_DIR/bore.log" 2>&1 &
  PIDS+=($!)
  PORT=""
  for _ in $(seq 1 40); do
    PORT=$(sed -n 's/.*listening at bore.pub:\([0-9]*\).*/\1/p' "$LOG_DIR/bore.log" | head -1)
    [[ -n "$PORT" ]] && break
    sleep 0.5
  done
  if [[ -n "$PORT" ]]; then
    ANNOUNCE_ARGS=(--announce "/dns4/bore.pub/tcp/$PORT")
    P2P_REACHABLE="yes"
    say "libp2p reachable at tcp://bore.pub:$PORT"
  else
    warn "bore started but exposed no port; see $LOG_DIR/bore.log"
  fi
else
  warn "No raw-TCP tunnel tool found (looked for ngrok, bore)."
fi

# --- the server itself ------------------------------------------------------
say "Starting the seed server…"
# An empty array must be expanded with the `+` form: under `set -u`, bash 3.2
# (what macOS ships) treats a bare "${arr[@]}" on an empty array as unbound and
# kills the subshell.
"$BIN" --data-dir "$DATA_DIR" run \
  --http-port "$HTTP_PORT" --tcp-port "$TCP_PORT" --quic-port "$TCP_PORT" \
  ${ANNOUNCE_ARGS[@]+"${ANNOUNCE_ARGS[@]}"} >"$LOG_DIR/server.log" 2>&1 &
SERVER_PID=$!
PIDS+=($SERVER_PID)

for _ in $(seq 1 40); do
  curl -fsS --max-time 2 "http://127.0.0.1:$HTTP_PORT/health" >/dev/null 2>&1 && break
  kill -0 "$SERVER_PID" 2>/dev/null || die "the seed server exited; see $LOG_DIR/server.log"
  sleep 0.5
done
curl -fsS --max-time 2 "http://127.0.0.1:$HTTP_PORT/health" >/dev/null 2>&1 \
  || die "the seed server did not become healthy; see $LOG_DIR/server.log"
say "Seed server healthy on port $HTTP_PORT"

# --- HTTPS tunnel for enrolment --------------------------------------------
PUBLIC_URL=""
TUNNEL_NOTE=""
if command -v cloudflared >/dev/null 2>&1; then
  say "Starting a Cloudflare tunnel for the enrolment API…"
  cloudflared tunnel --url "http://localhost:$HTTP_PORT" --no-autoupdate \
    >"$LOG_DIR/cloudflared.log" 2>&1 &
  PIDS+=($!)
  for _ in $(seq 1 120); do
    PUBLIC_URL=$(grep -oE 'https://[a-z0-9-]+\.trycloudflare\.com' \
      "$LOG_DIR/cloudflared.log" 2>/dev/null | head -1 || true)
    [[ -n "$PUBLIC_URL" ]] && break
    sleep 0.5
  done
  if [[ -n "$PUBLIC_URL" ]]; then
    # The hostname is registered a moment after it is printed.
    for _ in $(seq 1 40); do
      curl -fsS --max-time 4 "$PUBLIC_URL/health" >/dev/null 2>&1 && break
      sleep 1
    done
    curl -fsS --max-time 4 "$PUBLIC_URL/health" >/dev/null 2>&1 \
      || TUNNEL_NOTE="the tunnel URL is not answering yet; give it a moment"
  fi
elif command -v ngrok >/dev/null 2>&1; then
  PUBLIC_URL=$(curl -s http://127.0.0.1:4040/api/tunnels 2>/dev/null \
    | sed -n 's/.*"public_url":"\(https:[^"]*\)".*/\1/p' | head -1)
fi

if [[ -z "$PUBLIC_URL" ]]; then
  warn "No public URL — the enrolment API is only reachable on this machine."
  warn "See $LOG_DIR/cloudflared.log"
  PUBLIC_URL="http://localhost:$HTTP_PORT"
fi

ADMIN_TOKEN="$("$BIN" --data-dir "$DATA_DIR" token)"

cat <<EOF

────────────────────────────────────────────────────────────────────
  Seed server is up.

  Enrolment URL   $PUBLIC_URL
  Admin token     $ADMIN_TOKEN
  Logs            $LOG_DIR

  Cross-network peer-to-peer: $P2P_REACHABLE
EOF

if [[ "$P2P_REACHABLE" == "no" ]]; then
cat <<'EOF'
      An HTTP tunnel cannot carry libp2p. Devices can still enrol from
      anywhere, and any two on the same network will find each other by
      mDNS and sync directly. For phones on different networks, install
      ngrok or bore and re-run, or deploy the server to a public host.
EOF
fi
[[ -n "$TUNNEL_NOTE" ]] && warn "$TUNNEL_NOTE"

cat <<EOF

  Create an invite:
    ./scripts/invite.sh "$PUBLIC_URL" "$ADMIN_TOKEN" ada

  Point the app at it:
    SEED_URL=$PUBLIC_URL

  Ctrl-C to stop everything.
────────────────────────────────────────────────────────────────────

EOF

wait
