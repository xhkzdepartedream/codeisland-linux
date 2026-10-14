#!/usr/bin/env python3
"""Regression probe for the pi install path: deterministic, no model call, no pi needed.

Why this exists next to permission-probe.py: the pi integration is a file copy, not a JSON
merge, and every failure mode we actually hit today was invisible from the inside --
an installed extension that pi ignores (trust gate), a hand-edited copy that verify called
healthy, a backup written under the wrong name. All of those are checkable without running
a model, so they should be checked by a script and not by whoever remembers.

The extension loading itself -- the part that needs a real pi and a real turn -- is covered
by the manual recipe printed at the end.
"""

import os
import subprocess
import sys
import tempfile

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CTL = os.path.join(REPO, "target", "release", "codeisland-hookctl")
SRC = os.path.join(REPO, "integrations", "pi", "codeisland.ts")

failures = []


def check(label, ok, detail=""):
    print(f"  {'PASS' if ok else 'FAIL'}  {label}{(' — ' + detail) if detail else ''}")
    if not ok:
        failures.append(label)


def ctl(args, cwd):
    return subprocess.run([CTL] + args, cwd=cwd, capture_output=True, text=True)


if not os.path.exists(CTL):
    print(f"需要 {CTL}（先 cargo build --release）")
    sys.exit(2)

body = open(SRC, encoding="utf-8").read()
check("shipped extension exists", bool(body), f"{len(body)} chars")
check("package.json declares it", os.path.exists(os.path.join(REPO, "integrations/pi/package.json")))

with tempfile.TemporaryDirectory(prefix="ci-pi-") as proj:
    target = os.path.join(proj, ".pi", "extensions", "codeisland.ts")

    r = ctl(["path", "--source", "pi"], proj)
    check("path points at .pi/extensions/codeisland.ts",
          r.stdout.strip().endswith(os.path.join(".pi", "extensions", "codeisland.ts")),
          r.stdout.strip().replace(proj, "<proj>"))

    r = ctl(["verify", "--source", "pi"], proj)
    check("verify fails before install", r.returncode == 1, r.stderr.strip()[:80])

    r = ctl(["install", "--source", "pi"], proj)
    installed = open(target, encoding="utf-8").read() if os.path.exists(target) else ""
    check("install places a byte-identical copy", installed == body,
          f"{len(installed)} bytes")
    check("install names the --approve gate", "approve" in (r.stdout + r.stderr),
          "the trust gate is the one thing users cannot guess")
    check("install names the startup-only rule", "启动时" in (r.stdout + r.stderr))

    r = ctl(["verify", "--source", "pi"], proj)
    check("verify passes after install", r.returncode == 0, r.stdout.strip()[:70])

    r2 = ctl(["install", "--source", "pi"], proj)
    check("second install is a no-op", "已是最新" in r2.stdout, r2.stdout.strip()[:60])
    backups = [f for f in os.listdir(os.path.dirname(target)) if "bak" in f]
    check("no-op wrote no backup", not backups, str(backups))

    with open(target, "a", encoding="utf-8") as fh:
        fh.write("\n// hand edit\n")
    r = ctl(["verify", "--source", "pi"], proj)
    check("verify catches a hand edit", r.returncode == 1, r.stderr.strip()[:80])

    r = ctl(["install", "--source", "pi"], proj)
    check("install repairs the drift", open(target, encoding="utf-8").read() == body)
    backups = [f for f in os.listdir(os.path.dirname(target)) if f.startswith("codeisland.ts.bak-")]
    check("repair kept a backup named after the real file", len(backups) == 1, str(backups))

    r = ctl(["install", "--source", "pi", "--scope", "user"], proj)
    check("--scope user refuses to write ~/.pi", r.returncode == 2 and "pi install" in (r.stdout + r.stderr),
          r.stdout.strip()[:70] or r.stderr.strip()[:70])

    # claude and codex must still resolve their own files: the pi branch must not swallow them
    for source, suffix in (("claude", os.path.join(".claude", "settings.json")),
                           ("codex", os.path.join(".codex", "hooks.json"))):
        r = ctl(["path", "--source", source], proj)
        check(f"{source} still resolves its own file", r.stdout.strip().endswith(suffix),
              r.stdout.strip().replace(proj, "<proj>"))

print("\n  真加载部分需要 pi 与一次真实对话，仓库里没有静默替代：")
print("    mkdir -p .pi/extensions && cp integrations/pi/codeisland.ts .pi/extensions/")
print("    pi --approve --print '用 bash 工具运行：touch PI-PROBE-CHECK'")
print("    同时在常驻程序日志里应看到 SessionStart 与 PermissionRequest；缺 --approve 则一个都不会有")

if failures:
    print(f"\nPI INSTALL PROBE FAILED: {len(failures)} -> {failures}")
    sys.exit(1)
print("\nPI INSTALL PROBE OK")
