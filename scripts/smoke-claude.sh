#!/usr/bin/env bash
#
# End-to-end smoke test for phase 0.
#
# Proves the two things phase 0 exists to prove:
#   1. A real `claude` run produces events that reach the daemon and land in the
#      journal.
#   2. With the daemon stopped, `claude` behaves exactly as if nothing was
#      installed -- this is the safety contract the whole design rests on.
#
# The hooks are written to a *project-scoped* .claude/settings.json inside a
# throwaway directory, never to the user's global ~/.claude/settings.json. That
# avoids the known failure mode where Claude Code regenerates the global settings
# file and drops externally injected hooks.
#
# Usage: scripts/smoke-claude.sh [--keep]
#   --keep   leave the scratch directory in place for inspection

set -o pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BRIDGE="$REPO_ROOT/target/release/codeisland-bridge"
DAEMON="$REPO_ROOT/target/release/codeisland-daemon"

SCRATCH="${CODEISLAND_SMOKE_DIR:-/tmp/codeisland-smoke}"
WORKDIR="$SCRATCH/project"
SOCKET="$SCRATCH/codeisland.sock"
JOURNAL="$SCRATCH/events.jsonl"
DAEMON_LOG="$SCRATCH/daemon.log"

KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "PASS: $*"; }

[ -x "$BRIDGE" ] || fail "bridge not built: $BRIDGE (run: cargo build --release)"
[ -x "$DAEMON" ] || fail "daemon not built: $DAEMON (run: cargo build --release)"
command -v claude >/dev/null || fail "claude not on PATH"

rm -rf "$SCRATCH"
mkdir -p "$WORKDIR/.claude"

# --- project-scoped hooks -----------------------------------------------------
python3 - "$BRIDGE" "$WORKDIR/.claude/settings.json" <<'PY'
import json, sys
bridge, out = sys.argv[1], sys.argv[2]
def entry(event):
    return [{"matcher": "", "hooks": [{"type": "command",
            "command": f"{bridge} --source claude", "timeout": 30}]}]
settings = {"hooks": {e: entry(e) for e in
    ["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse",
     "PermissionRequest", "Stop", "SessionEnd"]}}
with open(out, "w") as fh:
    json.dump(settings, fh, indent=2)
PY
ok "wrote project-scoped hooks to $WORKDIR/.claude/settings.json"

export CODEISLAND_SOCKET_PATH="$SOCKET"
export CODEISLAND_JOURNAL="$JOURNAL"
export CODEISLAND_DEBUG=1
export CODEISLAND_BRIDGE_LOG="$SCRATCH/bridge.log"

cd "$WORKDIR" || fail "cannot cd $WORKDIR"

# --- case 1: daemon running -> events must land in the journal ----------------
"$DAEMON" >"$DAEMON_LOG" 2>&1 &
DAEMON_PID=$!
for _ in $(seq 1 50); do [ -S "$SOCKET" ] && break; sleep 0.1; done
[ -S "$SOCKET" ] || fail "daemon never created $SOCKET"

echo "--- case 1: claude run WITH daemon ---"
timeout 180 claude -p "Reply with the single word: ok" >"$SCRATCH/claude-with-daemon.out" 2>&1
CLAUDE_RC=$?
echo "claude exit=$CLAUDE_RC"
sed -n '1,20p' "$SCRATCH/claude-with-daemon.out"

kill "$DAEMON_PID" 2>/dev/null
wait "$DAEMON_PID" 2>/dev/null

[ -s "$JOURNAL" ] || fail "no events reached the journal (claude rc=$CLAUDE_RC)"

EVENT_COUNT=$(wc -l < "$JOURNAL")
SESSION_START=$(grep -c '"normalized_event":"SessionStart"' "$JOURNAL" || true)
echo "journal: $EVENT_COUNT events, $SESSION_START SessionStart"

[ "$SESSION_START" -ge 1 ] || fail "SessionStart never reached the journal"
ok "events reached the daemon and were journaled ($EVENT_COUNT events)"

# --- case 2: daemon stopped -> claude must be unaffected ----------------------
echo
echo "--- case 2: claude run WITHOUT daemon (safety contract) ---"
timeout 180 claude -p "Reply with the single word: ok" >"$SCRATCH/claude-without-daemon.out" 2>&1
CLAUDE_RC_NO_DAEMON=$?
echo "claude exit=$CLAUDE_RC_NO_DAEMON"
sed -n '1,20p' "$SCRATCH/claude-without-daemon.out"

if [ "$CLAUDE_RC_NO_DAEMON" -ne 0 ]; then
  fail "claude failed when the daemon was absent (rc=$CLAUDE_RC_NO_DAEMON) -- the hook is in the critical path"
fi
grep -qi "ok" "$SCRATCH/claude-without-daemon.out" \
  || fail "claude produced no answer without the daemon"
ok "claude completed normally with no daemon running"

echo
echo "--- summary ---"
echo "journal events:        $EVENT_COUNT"
echo "normalized names seen:"
python3 -c "
import json,sys,collections
names=collections.Counter()
for line in open('$JOURNAL'):
    line=line.strip()
    if not line: continue
    d=json.loads(line)
    names[d.get('normalized_event') or d.get('event')] += 1
for k,v in names.most_common(): print(f'  {v:4d}  {k}')
"

[ "$KEEP" -eq 1 ] || echo "(scratch kept at $SCRATCH -- pass --keep to silence this note)"
echo "SMOKE OK"
