#!/usr/bin/env python3
"""Assert the island really paints and morphs -- from pixels, not eyeballs.

The model driving this agent cannot open image files, so "it looks right" has to
become a measurement that fails loudly.

Method, and why each guard exists:
  - Target ONE screen by name. This machine has two screens at different scale
    factors (eDP-1 1536x960 dpr 2.0, DP-1 1920x1080 dpr 1.0), so the pill's pixel
    width and its centre depend on which screen hosts it. Pinning a screen also
    tells the script which capture columns to read.
  - Diff the target screen's top band against a pre-launch capture and count only
    pixels that got DARKER, since the pill is 95% opaque black.
  - Calibrate first: capture twice before launching anything. Pixels that change
    between those frames are dynamic desktop content and get excluded. An earlier
    version skipped this and "found" the island across the entire screen, because
    this agent's own terminal was printing inside the capture.

Checks, all inside one unconditional cleanup so a failure cannot leave an orphan
island on the desktop:
  1. an empty island draws a dark pill at the expected collapsed size
  2. a permission request expands it (the morph)
  3. clearing every session collapses it again
  4. steady-state CPU and RSS stay small

Usage:
  cargo build --release
  cmake -B shell/build -S shell -G Ninja && ninja -C shell/build
  scripts/visual-check.py [--screen eDP-1]
"""
import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import numpy as np
from PIL import Image

REPO = Path(__file__).resolve().parent.parent
DAEMON = REPO / "target/release/codeisland-daemon"
BRIDGE = REPO / "target/release/codeisland-bridge"
SHELL = REPO / "shell/build/codeisland-shell"
OUT = Path("/tmp/codeisland-visual-check")

# QML geometry in logical pixels. Keep in step with shell/qml/main.qml.
COLLAPSED_W = 260
MORPH_MIN_GROWTH = 1.3


def die(msg: str) -> None:
    print(f"FAIL: {msg}")
    sys.exit(1)


def ok(msg: str) -> None:
    print(f"PASS: {msg}")


class Screen:
    def __init__(self, name, x, y, w, h, dpr):
        self.name = name
        self.x, self.y, self.w, self.h = x, y, w, h
        self.dpr = float(dpr)

    # Column range inside a full virtual-desktop capture.
    @property
    def col0(self):
        return int(self.x * self.dpr)

    @property
    def col1(self):
        return int((self.x + self.w) * self.dpr)

    @property
    def band_rows(self):
        return int(min(self.h, 460) * self.dpr)


def list_screens():
    env = dict(os.environ)
    env.setdefault("WAYLAND_DISPLAY", "wayland-0")
    env.setdefault("QT_QPA_PLATFORM", "wayland")
    result = subprocess.run([str(SHELL), "--list-screens"], env=env,
                            capture_output=True, text=True, timeout=20)
    screens = []
    for line in result.stdout.strip().splitlines():
        parts = line.split()
        if len(parts) == 6:
            screens.append(Screen(parts[0], int(parts[1]), int(parts[2]),
                                  int(parts[3]), int(parts[4]), parts[5]))
    if not screens:
        die(f"--list-screens produced nothing: stdout={result.stdout!r} "
            f"stderr={result.stderr!r}")
    return screens


def capture():
    """Silent full-desktop capture as a luminance array."""
    path = OUT / "shot.png"
    subprocess.run(["spectacle", "-b", "-n", "-o", str(path)],
                   check=True, capture_output=True, timeout=40)
    arr = np.asarray(Image.open(path).convert("RGB"), dtype=np.float64).mean(axis=2)
    path.unlink(missing_ok=True)
    return arr


def darkened(before, after, screen, ignore):
    """Bounding box of pixels that got darker on this screen's top band."""
    top = screen.band_rows
    window_before = before[:top, screen.col0:screen.col1]
    window_after = after[:top, screen.col0:screen.col1]
    # Absolute near-black, not "got darker". The pill is #f2000000 (95% opaque
    # black), so it composites to luma ~2 no matter what is behind it. Requiring
    # a *difference* instead missed most of the pill over this machine's dark
    # wallpaper (background luma 46) and reported a 5 px tall pill.
    mask = ((window_after < 20) & (window_before - window_after > 4)
            & ~ignore[:top, screen.col0:screen.col1])
    if not mask.any():
        return None
    rows = np.where(mask.any(axis=1))[0]
    cols = np.where(mask.any(axis=0))[0]
    y0, y1 = int(rows[0]), int(rows[-1])
    x0, x1 = int(cols[0]), int(cols[-1])
    return {
        "x0": x0 + screen.col0,
        "x1": x1 + screen.col0,
        "y0": y0,
        "y1": y1,
        "w": x1 - x0 + 1,
        "h": y1 - y0 + 1,
        "luma": float(after[y0:y1 + 1, x0 + screen.col0:x1 + screen.col0 + 1].mean()),
        "under": float(before[y0:y1 + 1, x0 + screen.col0:x1 + screen.col0 + 1].mean()),
    }


def send(env, payload):
    result = subprocess.run([str(BRIDGE), "--source", payload.get("source", "claude")],
                            input=json.dumps(payload), capture_output=True,
                            text=True, env=env, timeout=20)
    if result.returncode != 0:
        die(f"bridge exited {result.returncode}: {result.stderr}")


def cpu_ticks(pid):
    with open(f"/proc/{pid}/stat") as handle:
        fields = handle.read().split(")")[-1].split()
    # After the closing ')' the 12th and 13th fields are utime and stime.
    return int(fields[11]) + int(fields[12])


def run(daemon, shell_holder, before, ignore, screen, collapsed_expected, hit):
    """Checks 1-4. Raises SystemExit via die() on any failure."""
    if abs(hit["w"] - collapsed_expected) > collapsed_expected * 0.25:
        die(f"collapsed width {hit['w']} px is not near the expected "
            f"{collapsed_expected:.0f} px -- wrong screen, wrong dpr, or the "
            "measured block is not the pill")
    if hit["luma"] > 60:
        die(f"pill is not dark: luma {hit['luma']:.1f}")
    ok(f"empty island draws a dark pill {hit['w']}x{hit['h']} px "
       f"(expected ~{collapsed_expected:.0f} wide)")

    env = daemon.env
    send(env, {"hook_event_name": "SessionStart", "session_id": "V1",
               "cwd": "/home/dev/blog", "source": "claude"})
    time.sleep(0.5)
    send(env, {"hook_event_name": "PermissionRequest", "session_id": "V2",
               "cwd": "/home/dev/api", "source": "claude", "tool_name": "Bash",
               "tool_input": {"command": "rm -rf /tmp/scratch"}})
    time.sleep(1.8)  # let the spring animation settle
    big = darkened(before, capture(), screen, ignore)
    if big is None:
        die("island disappeared once a session needed attention")
    grew_w = big["w"] / hit["w"]
    grew_h = big["h"] / hit["h"]
    if grew_w < MORPH_MIN_GROWTH or grew_h < MORPH_MIN_GROWTH:
        die(f"no morph: width x{grew_w:.2f} height x{grew_h:.2f}, "
            f"expected both >= {MORPH_MIN_GROWTH}")
    ok(f"permission request morphs the pill: "
       f"{hit['w']}x{hit['h']} -> {big['w']}x{big['h']} px")

    send(env, {"hook_event_name": "SessionEnd", "session_id": "V1", "source": "claude"})
    send(env, {"hook_event_name": "SessionEnd", "session_id": "V2", "source": "claude"})
    time.sleep(8.0)  # the celebration window is 6 s
    back = darkened(before, capture(), screen, ignore)
    if back is None:
        die("island vanished entirely; it should stay as an empty pill")
    if back["w"] > collapsed_expected * 1.35:
        die(f"pill stayed expanded: {back['w']} px wide")
    ok(f"island collapses back after sessions end: {back['w']}x{back['h']} px")

    shell = shell_holder[0]
    rss_kb = int(subprocess.run(["ps", "-o", "rss=", "-p", str(shell.pid)],
                                capture_output=True, text=True).stdout.split()[0])
    start = cpu_ticks(shell.pid)
    time.sleep(10.0)
    cpu_s = (cpu_ticks(shell.pid) - start) / os.sysconf("SC_CLK_TCK")
    print(f"      shell steady state: RSS {rss_kb / 1024:.1f} MB, "
          f"{cpu_s:.3f} CPU-s over 10 s")
    if cpu_s > 1.0:
        die(f"shell burned {cpu_s:.2f} CPU-s in 10 s -- too loud to ship")
    ok(f"shell is quiet: {cpu_s:.3f} CPU-s per 10 s, RSS {rss_kb / 1024:.1f} MB")


class DaemonWithEnv:
    def __init__(self, proc, env):
        self.proc = proc
        self.env = env


def main() -> int:
    argp = argparse.ArgumentParser()
    argp.add_argument("--screen", default=None,
                      help="screen name to host the island (default: the highest "
                           "scale factor, so geometry is measured at dpr > 1)")
    args = argp.parse_args()

    for binary in (DAEMON, BRIDGE, SHELL):
        if not binary.exists():
            die(f"{binary} missing. Build both:\n"
                "  cargo build --release\n"
                "  cmake -B shell/build -S shell -G Ninja && ninja -C shell/build")

    OUT.mkdir(parents=True, exist_ok=True)

    screens = list_screens()
    if args.screen:
        screen = next((s for s in screens if s.name == args.screen), None)
        if screen is None:
            die(f"screen {args.screen!r} not among {[s.name for s in screens]}")
    else:
        screen = max(screens, key=lambda s: s.dpr)
    print(f"      target screen: {screen.name} {screen.w}x{screen.h} "
          f"@dpr{screen.dpr} origin {screen.x},{screen.y} "
          f"-> capture columns {screen.col0}..{screen.col1}")

    collapsed_expected = COLLAPSED_W * screen.dpr
    print(f"      expected collapsed pill width: {collapsed_expected:.0f} px "
          f"({COLLAPSED_W} logical x dpr {screen.dpr})")

    work = tempfile.mkdtemp(prefix="codeisland-visual-")
    env = dict(os.environ)
    env["CODEISLAND_SOCKET_PATH"] = os.path.join(work, "hook.sock")
    env["CODEISLAND_VIEW_SOCKET"] = os.path.join(work, "view.sock")
    env["CODEISLAND_JOURNAL"] = os.path.join(work, "events.jsonl")
    env.setdefault("WAYLAND_DISPLAY", "wayland-0")
    env.setdefault("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
    env["QT_QPA_PLATFORM"] = "wayland"

    daemon_proc = subprocess.Popen([str(DAEMON)], env=env,
                                   stdout=open(OUT / "daemon.log", "w"),
                                   stderr=subprocess.STDOUT)
    daemon = DaemonWithEnv(daemon_proc, env)
    shell_holder = [None]
    failure = [None]

    try:
        for _ in range(100):
            if os.path.exists(env["CODEISLAND_VIEW_SOCKET"]):
                break
            time.sleep(0.05)
        else:
            die("daemon never created its view socket")

        first = capture()
        time.sleep(1.2)
        second = capture()
        top = screen.band_rows
        jitter = (np.abs(first[:top, screen.col0:screen.col1]
                         - second[:top, screen.col0:screen.col1]) > 8)
        ignore = np.zeros_like(first, dtype=bool)
        ignore[:top, screen.col0:screen.col1] = jitter
        print(f"      desktop self-motion in the band: {int(jitter.sum())} px "
              f"({100 * jitter.mean():.1f}%)")
        if jitter.mean() > 0.02:
            die("more than 2% of the top band moves on its own. Stop whatever is "
                "animating there (video, busy terminal) and re-run -- otherwise "
                "this test cannot tell the pill from other windows.")

        before = capture()
        shell_holder[0] = subprocess.Popen([str(SHELL), "--screen", screen.name],
                                           env=env,
                                           stdout=open(OUT / "shell.log", "w"),
                                           stderr=subprocess.STDOUT)
        time.sleep(3.0)
        if shell_holder[0].poll() is not None:
            die("shell exited immediately:\n" + (OUT / "shell.log").read_text())

        hit = darkened(before, capture(), screen, ignore)
        if hit is None:
            die("nothing got darker on the target screen -- the island is not "
                "painting there")
        print(f"      collapsed pill: {hit['w']}x{hit['h']} px at "
              f"x{hit['x0']}..{hit['x1']} y{hit['y0']}..{hit['y1']}, "
              f"luma {hit['luma']:.1f} over background {hit['under']:.1f}")

        run(daemon, shell_holder, before, ignore, screen, collapsed_expected, hit)
        print()
        print("VISUAL CHECK OK")
        return 0
    except SystemExit as exc:
        failure[0] = exc.code
        raise
    finally:
        # Unconditional: no check may leave an island or daemon behind on the
        # desktop, including the ones that fail halfway through.
        for proc in (shell_holder[0], daemon_proc):
            if proc is None:
                continue
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
        if failure[0] not in (None, 0):
            print("      (cleaned up shell and daemon despite the failure)")


if __name__ == "__main__":
    main()
