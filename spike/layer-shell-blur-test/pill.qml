// The pill under test: a translucent rounded rectangle with an opaque magenta
// border, floating above the noise backdrop.
//
// Why magenta: the border lets the harness locate the pill's exact footprint from
// the screenshot itself, with no assumed screen geometry and no scaling maths --
// #ff00ff cannot occur anywhere else in this setup.
//
// Why 50% grey (#80808080): blending that over the noise attenuates contrast by
// exactly the alpha factor and nothing else, so pure compositing predicts an
// inside/outside gradient-energy ratio of ~0.50. Compositor blur drives it far
// lower. The two hypotheses are numerically separated.

import QtQuick
import QtQuick.Window
import org.kde.layershell as LS

Window {
    id: root
    visible: true
    width: Screen.width
    height: 520
    title: "codeisland-blur-pill"
    color: "#00000000"

    LS.Window.layer: LS.Window.LayerOverlay
    LS.Window.anchors: LS.Window.AnchorTop | LS.Window.AnchorLeft | LS.Window.AnchorRight
    LS.Window.exclusionZone: -1
    LS.Window.keyboardInteractivity: LS.Window.KeyboardInteractivityNone

    Rectangle {
        anchors.horizontalCenter: parent.horizontalCenter
        anchors.verticalCenter: parent.verticalCenter
        width: 620
        height: 300
        radius: 40
        color: "#80808080"
        border.width: 4
        border.color: "#ffff00ff"
    }

    Timer {
        interval: 12000
        running: true
        repeat: false
        onTriggered: Qt.exit(0)
    }
}
