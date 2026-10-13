#!/usr/bin/env bash
#
# End-to-end smoke test for phase 0.
#
# Proves four things:
#   1. A real `claude` run produces events that reach the daemon and land in
#      the journal, including at least one tool call.
#   2. With the daemon killed and its stale socket file left behind, `claude`
#      behaves exactly as if nothing were installed. This is the realistic
#      failure mode: a crashed or killed daemon always leaves the file.
#   3. Same again with the socket file absent entirely.
#   4. A permission request is answered immediately when no island exists to
#      answer it, so an agent is never parked.
#
# Hooks go into a *project-scoped* .claude/settings.json in a throwaway
# directory, never into the user's global ~/.claude/settings.json. That avoids
# the known failure mode where Claude Code regenerates the global file and
# silently drops externally injected hooks.
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
VIEW_SOCKET="$SCRATCH/codeisland-view.sock"
JOURNAL="$SCRATCH/events.jsonl"
PROMPT="Run the shell command: echo ok    Then reply with only the word: done"

KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

# Model pinned to haiku by the project owner's standing constraint: real agent
# calls made by this test must use the cheapest tier. Override deliberately via
# CODEISLAND_SMOKE_MODEL.
MODEL="${CODEISLAND_SMOKE_MODEL:-haiku}"

fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "PASS: $*"; }
hr()   { echo; echo "--- $* ---"; }

[ -x "$BRIDGE" ] || fail "bridge not built: $BRIDGE (run: cargo build --release)"
[ -x "$DAEMON" ] || fail "daemon not built: $DAEMON (run: cargo build --release)"
command -v claude >/dev/null || fail "claude not on PATH"

rm -rf "$SCRATCH"
mkdir -p "$WORKDIR/.claude"

# --- project-scoped hooks ------------------------------------------------------
python3 - "$BRIDGE" "$WORKDIR/.claude/settings.json" <<'PY'
import json, sys
bridge, out = sys.argv[1], sys.argv[2]
def entry():
    # A single hook per event. The bridge decides for itself whether the event
    # is blocking, so the timeout here is the CLI's outer net, not our own.
    return [{"matcher": "", "hooks": [{"type": "command",
            "command": f"{bridge} --source claude", "timeout": 60}]}]
settings = {"hooks": {e: entry() for e in
    ["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse",
     "PermissionRequest", "Stop", "SessionEnd"]}}
with open(out, "w") as fh:
    json.dump(settings, fh, indent=2)
PY
ok "wrote project-scoped hooks: $WORKDIR/.claude/settings.json"

export CODEISLAND_SOCKET_PATH="$SOCKET"
export CODEISLAND_VIEW_SOCKET="$VIEW_SOCKET"
export CODEISLAND_JOURNAL="$JOURNAL"
export CODEISLAND_DEBUG=1
export CODEISLAND_BRIDGE_LOG="$SCRATCH/bridge.log"

cd "$WORKDIR" || fail "cannot cd $WORKDIR"

# --- case 1: daemon running, real claude ---------------------------------------
"$DAEMON" >"$SCRATCH/daemon.log" 2>&1 &
DAEMON_PID=$!
for _ in $(seq 1 50); do [ -S "$SOCKET" ] && [ -S "$VIEW_SOCKET" ] && break; sleep 0.1; done
[ -S "$SOCKET" ] || fail "daemon never created $SOCKET"

hr "case 1: real claude WITH daemon (model=$MODEL)"
timeout 240 claude --model "$MODEL" -p "$PROMPT" >"$SCRATCH/claude-1.out" 2>&1
RC1=$?
echo "claude exit=$RC1"
tail -5 "$SCRATCH/claude-1.out"
kill "$DAEMON_PID" 2>/dev/null; wait "$DAEMON_PID" 2>/dev/null

[ -s "$JOURNAL" ] || fail "no events reached the journal (claude rc=$RC1)"

if [ "$RC1" -ne 0 ]; then
  fail "claude exited $RC1 with the daemon running -- a hook must not break the agent"
fi

TOOL_EVENTS=$(grep -cE '"normalized_event":"(Pre|Post)ToolUse"' "$JOURNAL" || true)
SESSION_START=$(grep -c '"normalized_event":"SessionStart"' "$JOURNAL" || true)
[ "$SESSION_START" -ge 1 ] || fail "SessionStart never reached the journal"
[ "$TOOL_EVENTS" -ge 1 ] || fail "no tool-call event reached the journal"
ok "real claude events journaled: SessionStart=$SESSION_START tool=$TOOL_EVENTS"

# --- case 2: daemon killed, stale socket left behind ---------------------------
# `kill` does not let the daemon unlink, so the socket file is still there with
# nothing listening. This is the common real-world case after a crash.
hr "case 2: claude with a STALE socket file (daemon killed)"
[ -S "$SOCKET" ] || fail "expected a stale socket file to remain, got none"
echo "stale socket present: $(ls -l "$SOCKET" | awk '{print $1}')"
timeout 240 claude --model "$MODEL" -p "$PROMPT" >"$SCRATCH/claude-2.out" 2>&1
RC2=$?
echo "claude exit=$RC2"
tail -5 "$SCRATCH/claude-2.out"
[ "$RC2" -eq 0 ] || fail "claude failed against a stale socket (rc=$RC2)"
grep -qi "done" "$SCRATCH/claude-2.out" || fail "claude produced no answer against a stale socket"
ok "claude unaffected by a stale socket"

# --- case 3: socket absent entirely --------------------------------------------
hr "case 3: claude with NO socket at all"
rm -f "$SOCKET"
timeout 240 claude --model "$MODEL" -p "$PROMPT" >"$SCRATCH/claude-3.out" 2>&1
RC3=$?
echo "claude exit=$RC3"
tail -5 "$SCRATCH/claude-3.out"
[ "$RC3" -eq 0 ] || fail "claude failed with no socket (rc=$RC3)"
grep -qi "done" "$SCRATCH/claude-3.out" || fail "claude produced no answer with no socket"
ok "claude unaffected by a missing socket"

# --- case 4: permission request must not park the agent ------------------------
# Phase 0 has no island to ask a human, so the daemon replies with silence and
# closes. That must be immediate: the bridge's 3600 s ceiling is an upper bound
# for a human decision, not a licence to hold an agent when nothing can answer.
hr "case 4: PermissionRequest latency, daemon up but no UI"
"$DAEMON" >"$SCRATCH/daemon-4.log" 2>&1 &
DAEMON_PID4=$!
for _ in $(seq 1 50); do [ -S "$SOCKET" ] && [ -S "$VIEW_SOCKET" ] && break; sleep 0.1; done
LAT=$(python3 - "$BRIDGE" <<'PY'
import subprocess, time, json, os, sys
payload = json.dumps({
    "hook_event_name": "PermissionRequest", "session_id": "lat",
    "tool_name": "Bash", "tool_input": {"command": "echo hi"},
})
t = time.perf_counter()
r = subprocess.run([sys.argv[1], "--source", "claude"], input=payload,
                   capture_output=True, text=True)
elapsed = (time.perf_counter() - t) * 1000
if r.stdout.strip():
    print(f"BAD stdout was not empty: {r.stdout!r}")
else:
    print(f"{elapsed:.1f}")
PY
)
echo "permission round trip: ${LAT} ms (stdout must be empty)"
case "$LAT" in
  BAD*|*.*) ;;
  *) fail "could not measure permission latency: $LAT" ;;
esac
case "$LAT" in
  BAD*) fail "$LAT" ;;
  *) python3 -c "import sys; sys.exit(0 if float('$LAT') < 500 else 1)" \
       || fail "permission event took ${LAT} ms -- an agent must not wait on an island that cannot answer" ;;
esac
ok "permission request returns immediately (${LAT} ms), agent never parked"
kill "$DAEMON_PID4" 2>/dev/null; wait "$DAEMON_PID4" 2>/dev/null

# --- summary -------------------------------------------------------------------
hr "summary: journal event mix (cumulative across cases 1-4)"
python3 -c "
import json, collections
names = collections.Counter()
for line in open('$JOURNAL'):
    line = line.strip()
    if not line: continue
    d = json.loads(line)
    names[d.get('normalized_event') or d.get('event')] += 1
for k, v in names.most_common():
    print(f'  {v:4d}  {k}')
print(f'  ----  {sum(names.values())} total')
"

hr "bridge debug log tail"
tail -5 "$SCRATCH/bridge.log" 2>/dev/null || echo "(empty -- the bridge only logs when it has something to say)"

echo
echo "SMOKE OK  (scratch: $SCRATCH)"
