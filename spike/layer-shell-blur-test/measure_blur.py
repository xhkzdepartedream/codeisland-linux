#!/usr/bin/env python3
"""Does KWin 6.7.5 blur what is behind a wlr-layer-shell surface?

This is the question that decides how the island UI must be drawn, so it is
settled with an experiment rather than an assumption.

Design
------
The first attempt diffed two screenshots of the live desktop and was void: the
panel clock and this agent's own terminal output both changed between captures,
so the "probe footprint" came back as the entire screen.

This version needs only ONE screenshot:

  backdrop.qml  an opaque, static 4px black/white checkerboard on
                LS.Window.LayerBackground  -> maximal high-frequency detail
  pill.qml      a 50% grey rounded rect with an opaque magenta border on
                LS.Window.LayerOverlay     -> sits on top of that checkerboard

The magenta border is detected from the capture, which gives the pill's exact
footprint without assuming any screen geometry, scale factor or panel offset.
Then the sharpness inside the pill is compared with the sharpness of the same
checkerboard just outside it.

Reading the number
------------------
Gradient energy is the mean absolute pixel-to-pixel difference: a proxy for how
much high-frequency detail survives.

  ratio = gradient(inside the pill) / gradient(same noise outside the pill)

  ~0.50  pure alpha compositing. Blending a constant grey scales every
         difference by exactly (1 - alpha) = 0.50 and smooths nothing.
         => KWin did NOT blur. Frosted glass must be done client-side.
  <<0.50 the 4px checkerboard has been destroyed while its surroundings stayed
         sharp. Only a compositor-side blur does that.
         => KWin DID blur.

Usage:
  uv run --with pillow --with numpy --python 3.12 -- python3 measure_blur.py
"""
import os
import subprocess
import sys
import time
from pathlib import Path

import numpy as np
from PIL import Image

HERE = Path(__file__).resolve().parent
NOISE = Path("/tmp/ci-blur2/noise.png")
BACKDROP = HERE / "backdrop.qml"
PILL = HERE / "pill.qml"
OUT = Path(os.environ.get("CODEISLAND_BLUR_OUT", "/tmp/ci-blur2/measure"))
PILL_ALPHA = 0.50      # #80808080: alpha byte 0x80 = 128/255


def ensure_noise() -> None:
    if NOISE.exists():
        return
    subprocess.run([sys.executable, str(HERE / "gen_noise.py")], check=True)
    if not NOISE.exists():
        sys.exit(f"gen_noise.py did not produce {NOISE}")


def launch(qml: Path, log: Path):
    env = dict(os.environ)
    env.setdefault("WAYLAND_DISPLAY", "wayland-0")
    env.setdefault("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
    env["QT_QPA_PLATFORM"] = "wayland"
    handle = open(log, "w")
    proc = subprocess.Popen(["qmlscene6", str(qml)], env=env,
                            stdout=handle, stderr=subprocess.STDOUT)
    return proc, handle


def wait_mapped(proc, log, seconds: float = 3.0) -> None:
    time.sleep(seconds)
    if proc.poll() is not None:
        text = log.read_text(errors="replace").strip()
        sys.exit(f"window died early (rc={proc.returncode}) for {log}:\n{text}")


def main() -> int:
    OUT.mkdir(parents=True, exist_ok=True)
    ensure_noise()

    print("[1/5] starting the static checkerboard backdrop")
    backdrop, blog = launch(BACKDROP, OUT / "backdrop.log")
    wait_mapped(backdrop, OUT / "backdrop.log")

    print("[2/5] starting the translucent pill above it")
    pill, plog = launch(PILL, OUT / "pill.log")
    wait_mapped(pill, OUT / "pill.log")

    print("[3/5] single silent capture")
    capture = OUT / "capture.png"
    subprocess.run(["spectacle", "-b", "-n", "-o", str(capture)],
                   check=True, capture_output=True, timeout=30)

    for proc in (pill, backdrop):
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
    for handle in (plog, blog):
        handle.close()

    print("[4/5] locating the pill from its magenta border")
    rgb = np.asarray(Image.open(capture).convert("RGB"), dtype=np.int16)
    r, g, b = rgb[..., 0], rgb[..., 1], rgb[..., 2]
    magenta = (r > 190) & (b > 190) & (g < 90)
    n_mag = int(magenta.sum())
    print(f"      magenta border pixels: {n_mag}")
    if n_mag < 2000:
        print("FAIL: the pill was never composited onto the screen. The blur")
        print("      question is UNANSWERED -- check the layer-shell setup first.")
        print(f"      capture kept at {capture}")
        return 1

    rows = np.where(magenta.any(axis=1))[0]
    cols = np.where(magenta.any(axis=0))[0]
    y0, y1 = int(rows[0]), int(rows[-1])
    x0, x1 = int(cols[0]), int(cols[-1])
    print(f"      pill footprint: x {x0}..{x1}  y {y0}..{y1}  ({x1-x0}x{y1-y0})")

    grey = np.asarray(Image.open(capture).convert("L"), dtype=np.float64)

    # Interior, eroded well past the border and the corner radius.
    erode = 30
    inside = np.zeros_like(grey, dtype=bool)
    inside[y0 + erode:y1 - erode, x0 + erode:x1 - erode] = True
    # Outside: the same band, but the magenta-free checkerboard either side of it.
    band = np.zeros_like(grey, dtype=bool)
    band[y0:y1 + 1, :] = True
    outside = band & ~inside
    # Drop a guard margin around the pill so border bleed cannot leak in.
    guard = np.zeros_like(grey, dtype=bool)
    guard[y0 - erode:y1 + erode + 1, x0 - erode:x1 + erode + 1] = True
    outside &= ~guard
    # Drop anything near the very edges of the frame, where panels live.
    edge = 40
    outside[:edge, :] = False
    outside[-edge:, :] = False
    outside[:, :edge] = False
    outside[:, -edge:] = False

    n_in, n_out = int(inside.sum()), int(outside.sum())
    print(f"      sampled: inside {n_in} px, outside {n_out} px")
    if n_in < 10000 or n_out < 20000:
        print("FAIL: not enough area to measure. Capture kept at "
              f"{capture}")
        return 1

    print("[5/5] gradient energy, inside versus outside")

    def gradient_over(mask: np.ndarray) -> float:
        vals = grey[mask]
        # Mean absolute difference between horizontally and vertically adjacent
        # pixels that are both inside the mask.
        h_sub = grey[:, :-1] - grey[:, 1:]
        v_sub = grey[:-1, :] - grey[1:, :]
        m_h = mask[:, :-1] & mask[:, 1:]
        m_v = mask[:-1, :] & mask[1:, :]
        eh = float(np.abs(h_sub[m_h]).mean()) if m_h.any() else float("nan")
        ev = float(np.abs(v_sub[m_v]).mean()) if m_v.any() else float("nan")
        return float(np.nanmean([eh, ev]))

    g_in = gradient_over(inside)
    g_out = gradient_over(outside)
    ratio = g_in / g_out if g_out else float("nan")
    predicts = 1.0 - PILL_ALPHA

    mean_in = float(grey[inside].mean())
    mean_out = float(grey[outside].mean())
    print(f"      gradient outside (raw checkerboard) : {g_out:.3f}")
    print(f"      gradient inside  (seen through pill): {g_in:.3f}")
    print(f"      mean brightness outside {mean_out:.1f}   inside {mean_in:.1f}")
    print(f"      RATIO = {ratio:.3f}")
    print(f"      pure alpha compositing predicts ~{predicts:.2f}")
    print(f"      compositor blur predicts far below that")

    # Crop the boundary so the verdict can also be checked by eye.
    pad = 80
    crop = rgb[max(0, y0 - pad):y1 + pad, max(0, x0 - pad):min(rgb.shape[1], x1 + pad)]
    Image.fromarray(crop.astype(np.uint8)).save(OUT / "crop-boundary.png")
    print(f"      evidence: {capture}  {OUT / 'crop-boundary.png'}")

    print()
    if ratio > predicts * 0.85:
        print("VERDICT: NO COMPOSITOR BLUR behind a layer-shell surface.")
        print("  The pill only alpha-blended the checkerboard; nothing was smoothed.")
        print("  Frosted glass therefore has to be produced client-side.")
    elif ratio < predicts * 0.55:
        print("VERDICT: KWin BLURS behind the layer-shell surface.")
        print("  The checkerboard lost far more detail than alpha alone can explain.")
    else:
        print("VERDICT: INCONCLUSIVE -- look at crop-boundary.png before deciding.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
