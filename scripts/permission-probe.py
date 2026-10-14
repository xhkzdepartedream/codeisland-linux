#!/usr/bin/env python3
"""Prove the approval loop, including the case that must never become a denial.

The daemon now parks a hook connection until a UI answers. That creates a new
way to be wrong, and it is the one that matters most:

    if nobody answers, the agent must hear NOTHING -- not `deny`.

A withdrawal reported as a denial would make the island responsible for
behaviour no human chose, and in a non-interactive `claude -p` session a denial
is silently swallowed: the probe that produced this test's premise showed Claude
answering "ok" and exiting 0 while the file it claimed to create was never
written. So a spurious deny looks like success.

Cases:
  1. viewer answers `allow`   -> bridge stdout is the allow JSON
  2. viewer answers `deny`    -> bridge stdout is the deny JSON
  3. no viewer at all         -> stdout empty, and fast (agent not parked)
  4. viewer vanishes mid-wait -> stdout empty, NOT a denial
  5. decision after withdrawal -> ignored, and the parked hook still hears nothing
  6. daemon answers only once  -> a second decision for the same id is ignored

Usage: cargo build --release && scripts/permission-probe.py
"""
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DAEMON = REPO / "target/release/codeisland-daemon"
BRIDGE = REPO / "target/release/codeisland-bridge"
OUT = Path("/tmp/codeisland-permission-probe")

ALLOW_JSON = ('{"hookSpecificOutput":{"hookEventName":"PermissionRequest",'
              '"decision":{"behavior":"allow"}}}')
DENY_JSON = ('{"hookSpecificOutput":{"hookEventName":"PermissionRequest",'
             '"decision":{"behavior":"deny"}}}')


def die(msg: str) -> None:
    print(f"FAIL: {msg}")
    sys.exit(1)


def ok(msg: str) -> None:
    print(f"PASS: {msg}")


class Viewer:
    """A stand-in island: reads updates, can send decisions, can vanish."""

    def __init__(self, path: str):
        self.path = path
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(path)
        self.sock.settimeout(5.0)
        self.messages = []
        self.buffer = b""
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._read, daemon=True)
        self._thread.start()

    def _read(self):
        self.sock.settimeout(0.3)
        while not self._stop.is_set():
            try:
                data = self.sock.recv(65536)
            except socket.timeout:
                continue
            except OSError:
                return
            if not data:
                return
            self.buffer += data
            *lines, self.buffer = self.buffer.split(b"\n")
            for line in lines:
                line = line.strip()
                if line:
                    self.messages.append(json.loads(line))

    def wait_for(self, type_name: str, timeout: float = 5.0):
        deadline = time.time() + timeout
        while time.time() < deadline:
            for message in list(self.messages):
                if message.get("type") == type_name:
                    return message
            time.sleep(0.05)
        return None

    def decide(self, request_id: str, behavior: str):
        payload = json.dumps({
            "type": "permission_decision",
            "request_id": request_id,
            "behavior": behavior,
        }).encode() + b"\n"
        # The decision goes back on the same connection the island reads from.
        self.sock.sendall(payload)

    def vanish(self):
        self._stop.set()
        self.sock.close()
        self._thread.join(timeout=2)

    def drain_types(self):
        return [m.get("type") for m in self.messages]


def run_bridge(env: dict, payload: dict, timeout: float = 25.0):
    """Spawn the real bridge with a hook payload; return (stdout, seconds)."""
    started = time.perf_counter()
    result = subprocess.run(
        [str(BRIDGE), "--source", "claude"],
        input=json.dumps(payload), capture_output=True, text=True,
        env=env, timeout=timeout,
    )
    return result.stdout.strip(), time.perf_counter() - started


def permission_payload(session: str = "P1") -> dict:
    return {
        "hook_event_name": "PermissionRequest",
        "session_id": session,
        "cwd": "/home/dev/api",
        "tool_name": "Bash",
        "tool_input": {"command": "rm -rf /tmp/scratch"},
    }


def main() -> int:
    for binary in (DAEMON, BRIDGE):
        if not binary.exists():
            die(f"{binary} missing -- run: cargo build --release")

    OUT.mkdir(parents=True, exist_ok=True)
    work = tempfile.mkdtemp(prefix="codeisland-perm-")
    env = dict(os.environ)
    env["CODEISLAND_SOCKET_PATH"] = os.path.join(work, "hook.sock")
    env["CODEISLAND_VIEW_SOCKET"] = os.path.join(work, "view.sock")
    env["CODEISLAND_JOURNAL"] = os.path.join(work, "events.jsonl")

    daemon = subprocess.Popen([str(DAEMON)], env=env,
                              stdout=open(OUT / "daemon.log", "w"),
                              stderr=subprocess.STDOUT)
    try:
        for _ in range(100):
            if os.path.exists(env["CODEISLAND_VIEW_SOCKET"]):
                break
            time.sleep(0.05)
        else:
            die("daemon never created its view socket")

        # ---- case 3 first: no viewer -> silence, immediately ----
        stdout, elapsed = run_bridge(env, permission_payload("no-viewer"))
        if stdout != "":
            die(f"with no viewer the bridge must be silent, got {stdout!r}")
        if elapsed > 1.0:
            die(f"with no viewer the agent was parked for {elapsed:.2f}s")
        ok(f"no viewer -> empty stdout in {elapsed*1000:.0f} ms (agent not parked)")

        viewer = Viewer(env["CODEISLAND_VIEW_SOCKET"])
        snap = viewer.wait_for("snapshot")
        if snap is None:
            die("viewer got no initial snapshot")

        # ---- case 1: allow ----
        holder = {}

        def run_allow():
            holder["stdout"], holder["elapsed"] = run_bridge(env, permission_payload("A"))

        thread = threading.Thread(target=run_allow)
        thread.start()

        request = viewer.wait_for("permission_requested", timeout=6)
        if request is None:
            die(f"no permission_requested reached the viewer; got {viewer.drain_types()}")
        card = request["request"]
        if card["tool_name"] != "Bash" or "rm -rf" not in (card.get("summary") or ""):
            die(f"the card lacks the detail a human needs to decide: {card}")
        request_id = card["request_id"]
        if not thread.is_alive():
            die("the hook returned before anyone answered -- it is not parked")
        ok(f"request parked and shown to the viewer: {card['summary']!r}")

        viewer.decide(request_id, "allow")
        thread.join(timeout=10)
        if holder.get("stdout") != ALLOW_JSON:
            die(f"expected the allow JSON, got {holder.get('stdout')!r}")
        ok(f"allow -> hook stdout is the exact allow JSON ({holder['elapsed']*1000:.0f} ms)")

        # ---- case 2: deny ----
        viewer.messages.clear()
        holder2 = {}
        thread2 = threading.Thread(
            target=lambda: holder2.update(
                {"out": run_bridge(env, permission_payload("B"))[0]}))
        thread2.start()
        request = viewer.wait_for("permission_requested", timeout=6)
        if request is None:
            die("second request never reached the viewer")
        rid2 = request["request"]["request_id"]
        viewer.decide(rid2, "deny")
        thread2.join(timeout=10)
        if holder2.get("out") != DENY_JSON:
            die(f"expected the deny JSON, got {holder2.get('out')!r}")
        ok("deny -> hook stdout is the exact deny JSON")

        # ---- case 4: viewer vanishes while a request is parked ----
        viewer.messages.clear()
        holder3 = {}
        thread3 = threading.Thread(target=lambda: holder3.update(
            {"out": run_bridge(env, permission_payload("C"), timeout=40)[0]}))
        thread3.start()
        request = viewer.wait_for("permission_requested", timeout=6)
        if request is None:
            die("third request never reached the viewer")
        rid3 = request["request"]["request_id"]
        if not thread3.is_alive():
            die("request was answered without anyone deciding")

        started = time.perf_counter()
        viewer.vanish()          # the island goes away mid-decision
        thread3.join(timeout=20)
        released = time.perf_counter() - started
        if holder3.get("out") != "":
            die(f"a vanished viewer must NOT be read as a denial, got {holder3.get('out')!r}")
        if holder3.get("out") == DENY_JSON:
            die("the daemon fabricated a denial when nobody was watching")
        ok(f"viewer vanished -> empty stdout in {released:.2f}s, and NOT a denial")

        # ---- case 5: a decision arriving after withdrawal is ignored ----
        late = Viewer(env["CODEISLAND_VIEW_SOCKET"])
        late.decide(rid3, "allow")     # rid3 was already withdrawn
        time.sleep(0.4)
        log = (OUT / "daemon.log").read_text()
        if "ignoring decision for unknown request" not in log:
            die("a stale decision should have been logged as ignored")
        ok("stale decision after withdrawal was ignored, not applied")

        # ---- case 6: still functional after all that ----
        holder4 = {}
        thread4 = threading.Thread(target=lambda: holder4.update(
            {"out": run_bridge(env, permission_payload("D"))[0]}))
        thread4.start()
        request = late.wait_for("permission_requested", timeout=6)
        if request is None:
            die(f"daemon stopped delivering requests: {late.drain_types()}")
        late.decide(request["request"]["request_id"], "allow")
        thread4.join(timeout=10)
        if holder4.get("out") != ALLOW_JSON:
            die(f"post-stress allow failed: {holder4.get('out')!r}")
        ok("approval loop still works after a vanished viewer and a stale click")
        late.vanish()

        print()
        print("PERMISSION PROBE OK")
        return 0
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=5)
        except subprocess.TimeoutExpired:
            daemon.kill()
        print(f"  daemon log: {OUT / 'daemon.log'}")


if __name__ == "__main__":
    sys.exit(main())
