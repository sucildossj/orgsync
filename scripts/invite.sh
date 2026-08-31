#!/usr/bin/env bash
# Mints a single-use invite code.
# Usage: invite.sh <url> <admin-token> <user> [role]
set -euo pipefail

URL="${1:?usage: invite.sh <url> <admin-token> <user> [role]}"
TOKEN="${2:?admin token required}"
USER_ID="${3:?user id required}"
ROLE="${4:-member}"

RESPONSE=$(curl -fsS -X POST "${URL%/}/v1/admin/invites" \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d "{\"user_id\":\"$USER_ID\",\"role\":\"$ROLE\",\"ttl_hours\":24}") || {
    echo "Could not create the invite. Check the URL and the admin token." >&2
    exit 1
  }

field() {
  # Read one string field, keeping the JSON parsing out of the shell quoting.
  printf '%s' "$RESPONSE" | python3 -c "import json,sys; print(json.load(sys.stdin)['$1'])"
}

CODE=$(field invite_code)
WHO=$(field user_id)
WHAT=$(field role)
# Grouped for reading aloud and typing; the server ignores the dashes.
GROUPED=$(printf '%s' "$CODE" | fold -w5 | paste -sd- -)

cat <<EOF

  Invite code for $WHO ($WHAT):

      $GROUPED

  Single use, valid 24h. Enter it in the app along with:
      $URL

EOF
