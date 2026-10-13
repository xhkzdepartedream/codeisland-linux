// The island: a black pill floating at the top centre of the screen.
//
// Why a full-width transparent surface instead of a surface sized to the pill:
// LayerShellQt's `margins` is QMargins and has no QML value-type wrapper, so the
// pill cannot be offset to the centre by positioning its own surface. Verified in
// spike/layer-shell-blur-test/README.md. Spanning the top band and drawing the
// pill centred inside it is also what the morph needs -- the pill grows sideways.
//
// Why no frosted glass: measured on this machine, KWin 6.7.5 does not blur behind
// a layer-shell surface (brightness std under the pill was exactly half the
// backdrop's, which is pure alpha mixing). The original is pure black on macOS --
// the notch itself is black -- and its own Windows port says "designed around a
// pure-black, pixel-style Windows island". So black is the faithful look, not a
// compromise.

import QtQuick
import QtQuick.Window
import QtQuick.Layouts
import org.kde.layershell as LS

Window {
    id: island
    visible: true
    width: Screen.width
    height: 380
    title: "codeisland-island"
    color: "transparent"

    LS.Window.layer: LS.Window.LayerTop
    LS.Window.anchors: LS.Window.AnchorTop | LS.Window.AnchorLeft | LS.Window.AnchorRight
    LS.Window.exclusionZone: -1
    // Exclusive is required to type an answer later; nothing needs keyboard
    // focus today, so asking for it now would only steal it from the terminal.
    LS.Window.keyboardInteractivity: LS.Window.KeyboardInteractivityNone

    // ---- morph state -------------------------------------------------------

    // Upstream expands for anything that needs a human, and collapses back after
    // a quiet period. The pill must never grow on its own while merely working.
    readonly property bool needsAttention:
        sessions.needingAttention > 0
    property bool hovered: false
    property bool pinned: false
    // A finished session holds the pill open briefly so the result is seen.
    property bool celebrating: false

    readonly property bool expanded:
        needsAttention || hovered || pinned || celebrating

    // ---- timing ------------------------------------------------------------

    // One-second tick, and only while something is on screen. Polling the clock
    // with an empty island would burn CPU for no output.
    property real nowMs: Date.now()
    Timer {
        interval: 1000
        running: sessions.count > 0
        repeat: true
        onTriggered: island.nowMs = Date.now()
    }

    Timer {
        id: celebrateTimer
        interval: 6000
        onTriggered: island.celebrating = false
    }

    // One discrete on/off phase for a single header marker. Two repaints per
    // second, one item, no animation nodes.
    //
    // Cost history on this machine (eDP-1, software backend, five sessions
    // awaiting approval): per-dot 60 fps scale animations 15.25% of a core;
    // per-dot discrete blink with interpolation 6.00%; this single-marker version
    // is measured in scripts/README-resource-measurements.md.
    property bool attentionOn: true
    Timer {
        interval: 700
        running: sessions.needingAttention > 0
        repeat: true
        onTriggered: island.attentionOn = !island.attentionOn
    }

    // Any transition into `done` starts the celebration window.
    Connections {
        target: sessions
        function onChanged() {
            if (sessions.headlineStatus === "done" && sessions.count > 0) {
                island.celebrating = true
                celebrateTimer.restart()
            }
        }
    }

    // ---- geometry ----------------------------------------------------------

    readonly property real collapsedWidth: 260
    readonly property real collapsedHeight: 34
    readonly property real expandedWidth: Math.min(620, Screen.width - 40)
    readonly property int visibleCount: Math.min(sessions.count, 3)
    readonly property real expandedHeight:
        54 + visibleCount * 78 + (sessions.count > 3 ? 26 : 0)

    function humanize(ms) {
        const seconds = Math.max(0, Math.floor(ms / 1000))
        if (seconds < 60) return seconds + "s"
        const minutes = Math.floor(seconds / 60)
        if (minutes < 60) return minutes + "m"
        return Math.floor(minutes / 60) + "h"
    }

    // Status colour, mirroring the meaning upstream gives each state.
    function statusColor(status) {
        switch (status) {
        case "waiting_approval": return "#ff9f0a"   // amber: needs your yes/no
        case "waiting_input":    return "#5e9cff"   // blue: needs your words
        case "error":            return "#ff453a"   // red
        case "working":          return "#30d158"   // green: making progress
        case "done":             return "#63e6be"   // mint: finished, no action
        default:                 return "#8e8e93"   // grey: idle
        }
    }

    function statusText(status) {
        switch (status) {
        case "waiting_approval": return "需要批准"
        case "waiting_input":    return "等你回答"
        case "error":            return "出错了"
        case "working":          return "干活中"
        case "done":             return "已完成"
        default:                 return "空闲"
        }
    }

    // ---- the pill ----------------------------------------------------------

    Rectangle {
        id: pill
        anchors.horizontalCenter: parent.horizontalCenter
        anchors.top: parent.top
        anchors.topMargin: 6
        radius: expanded ? 22 : 17
        color: "#f2000000"
        border.width: 1
        border.color: "#1affffff"

        width: island.expanded ? island.expandedWidth : island.collapsedWidth
        height: island.expanded ? island.expandedHeight : island.collapsedHeight

        // The macOS original morphs rather than snapping. A spring with a slight
        // overshoot reads as the same gesture.
        Behavior on width {
            SpringAnimation { spring: 4; damping: 0.75; epsilon: 0.5 }
        }
        Behavior on height {
            SpringAnimation { spring: 4; damping: 0.75; epsilon: 0.5 }
        }
        Behavior on radius {
            NumberAnimation { duration: 180; easing.type: Easing.OutCubic }
        }

        MouseArea {
            id: mouse
            anchors.fill: parent
            hoverEnabled: true
            acceptedButtons: Qt.LeftButton
            onEntered: island.hovered = true
            onExited: island.hovered = false
            onClicked: island.pinned = !island.pinned
        }

        // ---- collapsed: one row, status dot, counts ----------------------

        RowLayout {
            anchors.fill: parent
            anchors.leftMargin: 14
            anchors.rightMargin: 14
            spacing: 8
            visible: !island.expanded
            opacity: island.expanded ? 0 : 1
            Behavior on opacity { NumberAnimation { duration: 120 } }

            Rectangle {
                Layout.preferredWidth: 10
                Layout.preferredHeight: 10
                Layout.alignment: Qt.AlignVCenter
                radius: 5
                color: sessions.count === 0
                       ? "#3a3a3c"
                       : island.statusColor(sessions.headlineStatus)
            }

            Text {
                Layout.fillWidth: true
                text: sessions.count === 0
                      ? "没有会话"
                      : sessions.count + " 个会话"
                      + (sessions.needingAttention > 0
                         ? " · " + sessions.needingAttention + " 个等你" : "")
                color: "#eaeaea"
                font.pixelSize: 13
                elide: Text.ElideRight
            }

            Text {
                visible: !daemon.connected
                text: "未连接"
                color: "#ff9f0a"
                font.pixelSize: 11
            }
        }

        // ---- expanded: cards ---------------------------------------------

        Column {
            anchors.fill: parent
            anchors.margins: 16
            spacing: 10
            visible: island.expanded
            opacity: island.expanded ? 1 : 0
            Behavior on opacity { NumberAnimation { duration: 140 } }

            Item {
                width: parent.width
                height: 22
                RowLayout {
                    anchors.left: parent.left
                    anchors.verticalCenter: parent.verticalCenter
                    spacing: 6
                    visible: sessions.needingAttention > 0
                    Rectangle {
                        Layout.preferredWidth: 9
                        Layout.preferredHeight: 9
                        Layout.alignment: Qt.AlignVCenter
                        radius: 4
                        color: "#ff9f0a"
                        opacity: island.attentionOn ? 1.0 : 0.25
                    }
                    Text {
                        text: sessions.needingAttention + " 个会话需要你"
                        color: "#ff9f0a"
                        font.pixelSize: 13
                        font.bold: true
                    }
                }
                Text {
                    anchors.left: parent.left
                    anchors.verticalCenter: parent.verticalCenter
                    visible: sessions.needingAttention === 0
                    text: sessions.count + " 个会话"
                    color: "#eaeaea"
                    font.pixelSize: 13
                    font.bold: true
                }
                Text {
                    anchors.right: parent.right
                    text: daemon.connected ? "" : "常驻程序未连接"
                    color: "#ff9f0a"
                    font.pixelSize: 11
                }
            }

            Repeater {
                model: sessions
                delegate: Rectangle {
                    required property int index
                    required property string sessionId
                    required property string source
                    required property string status
                    required property string project
                    required property string detail
                    required property real lastEventMs

                    width: parent ? parent.width : 0
                    height: 68
                    radius: 14
                    color: "#1c1c1e"
                    border.width: 1
                    readonly property bool blocked:
                        status === "waiting_approval" || status === "waiting_input"
                    border.color: blocked ? island.statusColor(status) : "#2c2c2e"

                    RowLayout {
                        anchors.fill: parent
                        anchors.leftMargin: 12
                        anchors.rightMargin: 12
                        spacing: 10

                        Rectangle {
                            Layout.preferredWidth: 12
                            Layout.preferredHeight: 12
                            Layout.alignment: Qt.AlignVCenter
                            radius: 6
                            color: island.statusColor(status)
                            // Static colour per card. Blinking *every* card is what
                            // cost CPU: five opacity interpolations repainted the
                            // whole surface 30+ times a second (measured 15.25% of a
                            // core, then 6.00% with a shared discrete phase). One
                            // marker in the header carries the "you are needed"
                            // signal instead -- see the attention timer below.
                            readonly property bool blocked:
                                status === "waiting_approval" || status === "waiting_input"
                        }

                        Column {
                            Layout.fillWidth: true
                            spacing: 3
                            Text {
                                width: parent.width
                                text: project
                                color: "#f2f2f2"
                                font.pixelSize: 14
                                font.bold: true
                                elide: Text.ElideRight
                            }
                            Text {
                                width: parent.width
                                text: detail && detail.length > 0
                                      ? detail : island.statusText(status)
                                color: "#98989d"
                                font.pixelSize: 12
                                elide: Text.ElideMiddle
                                maximumLineCount: 1
                            }
                        }

                        Column {
                            Layout.alignment: Qt.AlignVCenter
                            spacing: 2
                            Text {
                                text: source
                                color: "#636366"
                                font.pixelSize: 11
                            }
                            Text {
                                text: island.humanize(island.nowMs - lastEventMs)
                                color: status === "waiting_approval"
                                       || status === "waiting_input"
                                       ? island.statusColor(status) : "#636366"
                                font.pixelSize: 11
                                horizontalAlignment: Text.AlignRight
                            }
                        }
                    }
                }
            }

            Text {
                visible: sessions.count > 3
                width: parent.width
                horizontalAlignment: Text.AlignHCenter
                text: "+" + (sessions.count - 3) + " 个更多"
                color: "#636366"
                font.pixelSize: 11
            }
        }
    }
}
