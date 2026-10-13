#!/usr/bin/env python3
"""Drive the daemon over both sockets and print what a UI client would render.

Verifies the view protocol the island depends on:
  - a client receives a full snapshot the moment it connects
  - each hook event through the bridge becomes exactly one delta
  - SessionEnd becomes a removal
  - a late-connecting client gets the same state, ordered most-urgent-first

Uses only the standard library so it runs anywhere, and it drives the real
release binaries rather than stubs, so it exercises the actual socket framing.

Usage:
  cargo build --release
  scripts/view-probe.py            # spawns its own daemon on temp sockets
"""
import json
import os
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
BRIDGE = REPO / "target/release/codeisland-bridge"
DAEMON = REPO / "target/release/codeisland-daemon"


def die(msg: str) -> None:
    print(f"FAIL: {msg}")
    sys.exit(1)


def ok(msg: str) -> None:
    print(f"PASS: {msg}")


class Viewer:
    """Non-blocking reader over the view socket, collecting whole lines."""

    def __init__(self, path: str):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(path)
        self.sock.setblocking(False)
        self.pending = b""
        self.messages = []

    def drain(self) -> list:
        chunks = []
        while True:
            try:
                data = self.sock.recv(65536)
            except BlockingIOError:
                break
            except OSError:
                break
            if not data:
                break
            chunks.append(data)
        self.pending += b"".join(chunks)
        *lines, self.pending = self.pending.split(b"\n")
        for line in lines:
            line = line.strip()
            if line:
                self.messages.append(json.loads(line))
        return self.messages

    def close(self):
        self.sock.close()


def send_hook(daemon_env: dict, payload: dict) -> None:
    """Run the real bridge binary, exactly as an agent CLI would."""
    result = subprocess.run(
        [str(BRIDGE), "--source", payload.get("_expect_source", "claude")],
        input=json.dumps(payload),
        capture_output=True,
        text=True,
        env=daemon_env,
        timeout=20,
    )
    if result.returncode != 0:
        die(f"bridge exited {result.returncode}: {result.stderr}")


def main() -> int:
    for binary in (BRIDGE, DAEMON):
        if not binary.exists():
            die(f"{binary} missing -- run: cargo build --release")

    work = tempfile.mkdtemp(prefix="codeisland-view-probe-")
    hook = os.path.join(work, "hook.sock")
    view = os.path.join(work, "view.sock")
    journal = os.path.join(work, "events.jsonl")

    env = dict(os.environ)
    env["CODEISLAND_SOCKET_PATH"] = hook
    env["CODEISLAND_VIEW_SOCKET"] = view
    env["CODEISLAND_JOURNAL"] = journal

    daemon = subprocess.Popen(
        [str(DAEMON)], env=env,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
    )
    try:
        for _ in range(100):
            if os.path.exists(view) and os.path.exists(hook):
                break
            time.sleep(0.05)
        else:
            die("daemon never created its sockets")

        # ---- a client connecting to an empty daemon gets an empty snapshot ----
        v = Viewer(view)
        time.sleep(0.2)
        msgs = v.drain()
        if not msgs or msgs[0].get("type") != "snapshot":
            die(f"first message was not a snapshot: {msgs}")
        if msgs[0]["sessions"]:
            die(f"fresh daemon should snapshot empty, got {msgs[0]['sessions']}")
        ok("connecting client receives an initial snapshot")

        # ---- each event becomes exactly one delta ----
        deltas_before = len(msgs)
        send_hook(env, {
            "hook_event_name": "SessionStart",
            "session_id": "A",
            "cwd": "/home/dev/blog",
        })
        time.sleep(0.15)
        msgs = v.drain()
        new = msgs[deltas_before:]
        if len(new) != 1 or new[0].get("type") != "session_upsert":
            die(f"SessionStart produced {new}")
        session = new[0]["session"]
        if session["status"] != "working":
            die(f"unexpected session status: {session}")
        # The wire format carries cwd; the label is derived client-side, exactly
        # as the QML card will do it.
        if session.get("cwd") != "/home/dev/blog":
            die(f"cwd not carried on the wire: {session}")
        if session["cwd"].rstrip("/").rsplit("/", 1)[-1] != "blog":
            die("derived label is not 'blog'")
        ok("SessionStart -> one upsert, status=working, cwd present for labelling")

        send_hook(env, {
            "hook_event_name": "PreToolUse",
            "session_id": "A",
            "tool_name": "Bash",
            "tool_input": {"command": "cargo build --release"},
        })
        time.sleep(0.15)
        msgs = v.drain()
        last = msgs[-1]["session"]
        if last["tool_calls"] != 1:
            die(f"tool_calls did not increment: {last}")
        if "cargo build" not in (last.get("detail") or ""):
            die(f"detail not carried through: {last}")
        ok("PreToolUse -> tool_calls incremented and detail carried")

        # ---- a blocked session from another CLI ----
        send_hook(env, {
            "hook_event_name": "PermissionRequest",
            "session_id": "B",
            "cwd": "/home/dev/api",
            "tool_name": "Edit",
            "tool_input": {"file_path": "src/main.rs"},
        })
        time.sleep(0.15)
        msgs = v.drain()
        blocked = [m["session"] for m in msgs
                   if m.get("type") == "session_upsert" and m["session"]["id"] == "B"]
        if not blocked or blocked[-1]["status"] != "waiting_approval":
            die(f"permission did not mark waiting_approval: {blocked}")
        ok("PermissionRequest -> status=waiting_approval")

        # ---- ordering: blocked first, regardless of arrival order ----
        send_hook(env, {"hook_event_name": "Stop", "session_id": "A"})
        time.sleep(0.15)
        late = Viewer(view)
        time.sleep(0.2)
        snap = [m for m in late.drain() if m.get("type") == "snapshot"][0]
        order = [s["id"] for s in snap["sessions"]]
        if order[0] != "B":
            die(f"blocked session must sort first, got {order}")
        ok(f"snapshot orders by urgency: {order}")
        late.close()

        # ---- SessionEnd removes ----
        send_hook(env, {"hook_event_name": "SessionEnd", "session_id": "A"})
        time.sleep(0.15)
        msgs = v.drain()
        removals = [m for m in msgs if m.get("type") == "session_remove"]
        if not removals or removals[-1]["id"] != "A":
            die(f"SessionEnd produced no removal: {msgs[-3:]}")
        ok("SessionEnd -> session_remove")

        # ---- journal holds the raw events too ----
        lines = [l for l in Path(journal).read_text().splitlines() if l.strip()]
        if len(lines) != 5:
            die(f"expected 5 journal lines, got {len(lines)}")
        ok(f"journal recorded all {len(lines)} raw events")

        print()
        print("VIEW PROBE OK")
        return 0
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=5)
        except subprocess.TimeoutExpired:
            daemon.kill()
        v.close()


if __name__ == "__main__":
    sys.exit(main())
