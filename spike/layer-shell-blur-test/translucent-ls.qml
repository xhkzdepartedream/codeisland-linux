import QtQuick
import QtQuick.Window
import org.kde.layershell as LS
Window {
    visible: true; width: 700; height: 360; color: "transparent"; title: "translucent-ls"
    LS.Window.layer: LS.Window.LayerOverlay
    LS.Window.keyboardInteractivity: LS.Window.KeyboardInteractivityNone
    Rectangle { anchors.fill: parent; anchors.margins: 40; radius: 40
        color: "#80ff0000"; border.width: 5; border.color: "#ffff00ff" }
    Timer { interval: 9000; running: true; repeat: false; onTriggered: Qt.exit(0) }
}
