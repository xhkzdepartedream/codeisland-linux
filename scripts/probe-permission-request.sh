#!/usr/bin/env bash
#
# Premise check for phase 2: does Claude Code actually fire a PermissionRequest
# hook event on this machine?
#
# Why this needs asking rather than assuming. A machine's global
# ~/.claude/settings.json may set permissions.defaultMode to a bypassing mode,
# which auto-approves tool calls -- and a bypassed permission is never requested,
# so a PermissionRequest hook may never fire at all. In that case the approval
# card would be dead code for Claude Code, and the island's headline feature would
# need a different trigger.
#
# Nothing here writes to the global settings. The permission mode is overridden by
# a throwaway *project-scoped* .claude/settings.json, which Claude Code reads
# without touching the user's file.
#
# Usage: scripts/probe-permission-request.sh [haiku]

set -o pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BRIDGE="$REPO_ROOT/target/release/codeisland-bridge"
DAEMON="$REPO_ROOT/target/release/codeisland-daemon"
MODEL="${1:-haiku}"

SCRATCH="${CODEISLAND_PROBE_DIR:-/tmp/codeisland-perm-probe}"
WORKDIR="$SCRATCH/project"
SOCKET="$SCRATCH/hook.sock"
VIEW_SOCKET="$SCRATCH/view.sock"
JOURNAL="$SCRATCH/events.jsonl"
TARGET="$SCRATCH/touched.txt"

[ -x "$BRIDGE" ] || { echo "FAIL: build first: cargo build --release"; exit 1; }
command -v claude >/dev/null || { echo "FAIL: claude not on PATH"; exit 1; }

rm -rf "$SCRATCH"; mkdir -p "$WORKDIR/.claude"

# Project-scoped settings: hooks plus a real permission mode. Note this does NOT
# inherit the global bypassPermissions, which is the whole point of the probe.
python3 - "$BRIDGE" "$WORKDIR/.claude/settings.json" <<'PY'
import json, sys
bridge, out = sys.argv[1], sys.argv[2]
def entry(timeout):
    return [{"matcher": "", "hooks": [{"type": "command",
            "command": f"{bridge} --source claude", "timeout": timeout}]}]
settings = {
    "permissions": {"defaultMode": "default"},
    "hooks": {e: entry(60) for e in [
        "SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse",
        "PermissionRequest", "Stop", "SessionEnd"]},
}
with open(out, "w") as fh:
    json.dump(settings, fh, indent=2)
PY

export CODEISLAND_SOCKET_PATH="$SOCKET"
export CODEISLAND_VIEW_SOCKET="$VIEW_SOCKET"
export CODEISLAND_JOURNAL="$JOURNAL"

"$DAEMON" >"$SCRATCH/daemon.log" 2>&1 &
DAEMON_PID=$!
for _ in $(seq 1 60); do [ -S "$SOCKET" ] && [ -S "$VIEW_SOCKET" ] && break; sleep 0.1; done
[ -S "$SOCKET" ] || { echo "FAIL: daemon did not start"; exit 1; }

cd "$WORKDIR" || exit 1
echo "--- running claude (--model $MODEL, project defaultMode=default) ---"
# A tool call that genuinely needs permission: writing a file outside the project.
timeout 240 claude --model "$MODEL" -p \
  "Use the Bash tool to run exactly this command and nothing else: touch $TARGET . Then reply: ok" \
  >"$SCRATCH/claude.out" 2>&1
RC=$?
kill "$DAEMON_PID" 2>/dev/null; wait "$DAEMON_PID" 2>/dev/null

echo "claude exit=$RC"
echo "claude said: $(tail -3 "$SCRATCH/claude.out" | tr '\n' ' ')"
echo

if [ ! -s "$JOURNAL" ]; then
  echo "RESULT: no events at all -- hooks did not fire in this configuration"
  exit 1
fi

python3 - "$JOURNAL" "$TARGET" <<'PY'
import json, sys, collections, os
journal, target = sys.argv[1], sys.argv[2]
events = [json.loads(l) for l in open(journal) if l.strip()]
names = collections.Counter(e.get("normalized_event") or e.get("event") for e in events)
print("event mix from this single run:")
for name, count in names.most_common():
    print(f"  {count:3d}  {name}")

perm = [e for e in events
        if (e.get("normalized_event") or e.get("event")) == "PermissionRequest"]
print()
if perm:
    e = perm[0]
    print("RESULT: PermissionRequest DOES fire")
    print(f"  tool={e.get('tool_name')} detail={e.get('summary')!r}")
    print(f"  cwd={e.get('cwd')}")
else:
    print("RESULT: PermissionRequest did NOT fire")
    print("  Either bypassPermissions still applies, or the call was allowed by")
    print("  another rule, or this Claude Code version names it differently.")
    print(f"  PreToolUse fired: {names.get('PreToolUse', 0)} time(s)")
    print(f"  file created: {os.path.exists(target)}")
    print()
    print("  Implication for phase 2: the approval card needs a different trigger")
    print("  for Claude Code on this machine (for example PreToolUse on a mutating")
    print("  tool, which is what the upstream project's PermissionRequest routing")
    print("  effectively amounts to when the CLI auto-answers).")
PY
