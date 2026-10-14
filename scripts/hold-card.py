#!/usr/bin/env python3
"""Hold one approval card (and optionally one question card) on the live island.

Used only to put the Linux island into the same visual states as upstream's README
screenshots, so the two can be compared side by side instead of from memory. It spawns the
real bridge against the real default sockets, so the running daemon parks the request exactly
as it would for a real CLI.

Usage:  hold-card.py approval|question|both   (then capture; Ctrl-C or kill to release)
"""

import json
import os
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
BRIDGE = REPO / "target/release/codeisland-bridge"


def approval(session="web-app", cwd="/home/xhkz/apps/web-app"):
    return {
        "hook_event_name": "PermissionRequest",
        "session_id": session,
        "cwd": cwd,
        "tool_name": "Bash",
        "tool_input": {"command": "npm run test -- --coverage"},
    }


def question(session="web-app", cwd="/home/xhkz/apps/web-app"):
    return {
        "hook_event_name": "PermissionRequest",
        "session_id": session,
        "cwd": cwd,
        "tool_name": "AskUserQuestion",
        "tool_input": {
            "questions": [
                {
                    "question": "仪表盘应该用哪个状态管理器?",
                    "header": "状态管理",
                    "multiSelect": False,
                    "options": [
                        {"label": "Zustand (推荐)", "description": "轻量的 Hook 式 store，几乎零样板代码"},
                        {"label": "Redux Toolkit", "description": "结构化 slice，自带 DevTools 与 RTK Query"},
                        {"label": "React Context", "description": "React 内置方案，适合不常变化的状态"},
                    ],
                }
            ]
        },
    }


def hold(payload, tag):
    """Spawn the bridge and park it. Returns the Popen.

    The bridge reads stdin to EOF before it forwards -- it is a filter, not a server -- so
    stdin must be closed or the request is never sent. Closing stdin is safe here: what the
    daemon watches for "agent died" is EOF on the *socket* connection, which stays open as long
    as this process lives. (Getting this backwards is why an earlier attempt produced a bridge
    that sat there holding nothing.)
    """
    proc = subprocess.Popen(
        [str(BRIDGE), "--source", "claude"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, start_new_session=True,
    )
    proc.stdin.write(json.dumps(payload))
    proc.stdin.close()
    print(f"  held {tag} (pid {proc.pid})")
    return proc


def main() -> int:
    which = sys.argv[1] if len(sys.argv) > 1 else "both"
    procs = []
    if which in ("approval", "both"):
        procs.append(hold(approval("web-app"), "approval"))
    if which in ("question", "both"):
        procs.append(hold(question("web-app"), "question"))
    print("  cards are up; kill this process to release them")
    try:
        while True:
            time.sleep(1)
    except KeyboardInterrupt:
        pass
    finally:
        for p in procs:
            p.kill()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
