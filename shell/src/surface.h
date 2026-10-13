#pragma once

// The island's window must be per-pixel transparent, otherwise rounded corners
// render as a black rectangle. Measured on this machine (KWin 6.7.5, Wayland):
// a window created by qmlscene6 painted #80ff0000 as (255,0,0) -- fully opaque --
// while the same colour inside an opaque window blended to (128,127,0), matching
// theory. So Qt's blending was fine and the *surface* had no alpha buffer.
// Requesting one before any window exists is the fix; see
// spike/layer-shell-blur-test/README.md.

#include <QSurfaceFormat>

inline void requestTranslucentSurface()
{
    QSurfaceFormat format = QSurfaceFormat::defaultFormat();
    format.setAlphaBufferSize(8);
    QSurfaceFormat::setDefaultFormat(format);
}
