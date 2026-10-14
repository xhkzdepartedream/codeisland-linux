// The island: a black pill floating at the top centre of the screen.
//
// The window IS the pill.
//
// Earlier notes here claimed `margins` cannot be set from QML. That was wrong:
// `QtMargins {}` is not a type, but a plain JS object is accepted, and measured
// on this machine `margins: ({left:600, top:40, right:600, bottom:0})` moved the
// surface to exactly logical x=600, y=40. So the surface is the size of the pill
// and centred by symmetric left/right margins.
//
// This matters beyond tidiness. When the surface spanned the screen, an anchored
// full-width window plus a C++ setGeometry() covering the whole screen meant a
// 1536x960 transparent layer sat on top of the laptop screen and swallowed every
// click on it. A pill-sized surface only takes input where the pill is.
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
    title: "codeisland-island"
    color: "transparent"

    // The window is the pill, so its bounding box shrinks to nothing when there
    // is nothing to show -- the collapsed strip is 260x34, not the whole screen.
    width: island.targetWidth
    height: island.targetHeight

    // Centred by symmetric margins: with Top|Left|Right anchored, KWin computes
    // the surface width as screenWidth - left - right, so equal left/right values
    // both centre it and fix its width.
    readonly property real horizontalMargin:
        Math.max(0, (Screen.width - width) / 2)

    LS.Window.layer: LS.Window.LayerTop
    LS.Window.anchors: LS.Window.AnchorTop | LS.Window.AnchorLeft | LS.Window.AnchorRight
    LS.Window.margins: ({
        left: island.horizontalMargin,
        top: 6,
        right: island.horizontalMargin,
        bottom: 0
    })
    LS.Window.exclusionZone: -1
    // Exclusive is required to type an answer later; nothing needs keyboard
    // focus today, so asking for it now would only steal it from the terminal.
    LS.Window.keyboardInteractivity: LS.Window.KeyboardInteractivityNone

    // ---- morph state -------------------------------------------------------

    // Upstream expands for anything that needs a human, and collapses back after
    // a quiet period. The pill must never grow on its own while merely working.
    readonly property bool needsAttention:
        sessions.needingAttention > 0 || permissions.count > 0
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
    readonly property int visiblePermissions: Math.min(permissions.count, 2)

    // The expanded content's own padding, shared with the Column that draws it so
    // the two cannot drift apart.
    readonly property real contentMargin: 12

    // Exactly as tall as the content it is showing, plus the padding around it.
    //
    // This used to be a hand-tuned sum of card heights, and it silently disagreed
    // with the layout the moment a delegate changed: with one permission card and
    // one session it asked for 210 while the content needed 228, so the surface
    // sliced the bottom border off the last card. Deriving it from the content's
    // implicitHeight makes the window follow the cards instead of the other way
    // round. Positioners leave invisible children out of that number, so the
    // "+N more" row and the cards that are not shown cost nothing.
    readonly property real expandedHeight:
        island.contentMargin * 2 + expandedContent.implicitHeight

    readonly property real targetWidth:
        expanded ? expandedWidth : collapsedWidth
    readonly property real targetHeight:
        expanded ? expandedHeight : collapsedHeight

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

    // The spring animation now lives on the window, because the window is the
    // pill: its bounding box and its paint are the same thing.
    Behavior on width {
        SpringAnimation { spring: 4; damping: 0.75; epsilon: 0.5 }
    }
    Behavior on height {
        SpringAnimation { spring: 4; damping: 0.75; epsilon: 0.5 }
    }

    Rectangle {
        id: pill
        anchors.fill: parent
        radius: island.expanded ? 22 : 17
        color: "#f2000000"
        border.width: 1
        border.color: "#1affffff"

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
            id: expandedContent
            anchors.fill: parent
            anchors.margins: island.contentMargin
            spacing: 8
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

            // ---- waiting on a yes/no: the only thing here that unblocks an agent

            Repeater {
                model: permissions
delegate: Rectangle {
                    id: cardRoot
                    required property int index
                    required property string requestId
                    required property string sessionId
                    required property string source
                    required property string tool
                    required property string summary
                    required property string project
                    // A question card carries its options here; an ordinary approval
                    // leaves it empty. That is the only signal separating the two --
                    // the same field the daemon sends, not a second flag to keep in
                    // sync.
                    required property var questions

                    // Selections keyed by the question text, because that exact string
                    // is the key the CLI wants back inside `answers`. Reassigned rather
                    // than mutated so QML's bindings notice the change.
                    property var picks: ({})
                    readonly property int asked: questions ? questions.length : 0
                    readonly property int picked: {
                        let n = 0
                        for (let i = 0; i < asked; ++i) {
                            if (picks[questions[i].question] !== undefined) { ++n }
                        }
                        return n
                    }
                    readonly property bool isQuestion: asked > 0
                    readonly property bool ready: isQuestion && picked === asked

                    width: parent ? parent.width : 0
                    // Question cards are as tall as their content. The approval card
                    // keeps the fixed height the window's content-derived math already
                    // accounts for.
                    height: isQuestion ? cardBody.implicitHeight + 24 : 98
                    // At most two cards ask at once; a third would only make the
                    // island taller than it is useful. The agent is still parked on
                    // its own socket either way, so nothing is silently dropped --
                    // the extra cards simply wait for one of these to clear.
                    visible: index < island.visiblePermissions
                    radius: 14
                    color: isQuestion ? "#08210f" : "#231a06"
                    border.width: 1
                    border.color: isQuestion ? "#30d158" : "#ff9f0a"

                    Column {
                        id: cardBody
                        anchors.left: parent.left
                        anchors.right: parent.right
                        anchors.top: parent.top
                        anchors.margins: 12
                        spacing: 8

                        RowLayout {
                            width: parent.width
                            spacing: 8

                            Text {
                                Layout.fillWidth: true
                                text: (project.length > 0 ? project + " · " : "")
                                      + (cardRoot.isQuestion
                                         ? source + " 在等你回答"
                                         : source + " 想执行 " + tool)
                                color: cardRoot.isQuestion ? "#7bf3a0" : "#ffd60a"
                                font.pixelSize: 13
                                font.bold: true
                                elide: Text.ElideRight
                            }

                            // 跳回终端. A view action, not an answer: the daemon keeps
                            // the hook parked, so coming here to look never decides for
                            // the agent, and the card stays up afterwards -- returning
                            // to the question is the point of the trip.
                            Text {
                                Layout.alignment: Qt.AlignVCenter
                                text: "↩ 跳回终端"
                                color: jumpArea.containsMouse ? "#ffffff" : "#8e8e93"
                                font.pixelSize: 11
                                font.underline: jumpArea.containsMouse
                                MouseArea {
                                    id: jumpArea
                                    anchors.fill: parent
                                    anchors.margins: -4
                                    hoverEnabled: true
                                    cursorShape: Qt.PointingHandCursor
                                    onClicked: {
                                        console.log("jump clicked, sessionId=" + sessionId)
                                        daemon.jump(sessionId)
                                    }
                                }
                            }
                        }

                        // The one-line summary describes a tool call. On a question
                        // card it would only print "AskUserQuestion" where the question
                        // belongs, so it is dropped rather than shrunk.
                        Text {
                            width: parent.width
                            // Column skips an invisible child, so hiding is enough.
                            // A manual `height: visible ? implicitHeight : 0` here made
                            // a binding loop with `maximumLineCount`, whose implicit
                            // height depends on the height it was feeding.
                            visible: !cardRoot.isQuestion
                            text: summary
                            color: "#eaeaea"
                            font.pixelSize: 12
                            elide: Text.ElideMiddle
                            maximumLineCount: 1
                        }

                        Repeater {
                            model: cardRoot.isQuestion ? cardRoot.questions : []
                            delegate: Column {
                                id: questionBlock
                                required property var modelData
                                width: cardBody.width
                                spacing: 6

                                readonly property string question: modelData.question
                                readonly property string header: modelData.header || ""

                                Text {
                                    width: parent.width
                                    text: (questionBlock.header.length > 0
                                           ? questionBlock.header + " · " : "")
                                          + questionBlock.question
                                    color: "#eaeaea"
                                    font.pixelSize: 12
                                    wrapMode: Text.WordWrap
                                }

                                Repeater {
                                    model: modelData.options
                                    delegate: Rectangle {
                                        id: optionRow
                                        required property var modelData
                                        readonly property bool chosen:
                                            cardRoot.picks[questionBlock.question] === modelData.label

                                        width: cardBody.width
                                        height: 24
                                        radius: 8
                                        color: chosen ? "#30d158"
                                               : (optionArea.containsMouse ? "#1e2b22" : "#141a16")
                                        border.width: 1
                                        border.color: chosen ? "#30d158" : "#3a4a40"

                                        Row {
                                            anchors.left: parent.left
                                            anchors.leftMargin: 8
                                            anchors.verticalCenter: parent.verticalCenter
                                            spacing: 6

                                            // The radio dot. There is no Other here:
                                            // the CLI appends one itself, and a second
                                            // one would be a lie about what the agent
                                            // offered.
                                            Text {
                                                text: optionRow.chosen ? "●" : "○"
                                                color: optionRow.chosen ? "#04150a" : "#8e8e93"
                                                font.pixelSize: 11
                                            }
                                            Text {
                                                text: modelData.label
                                                      + (modelData.description
                                                         ? " — " + modelData.description : "")
                                                color: optionRow.chosen ? "#04150a" : "#eaeaea"
                                                font.pixelSize: 11
                                                elide: Text.ElideRight
                                                width: Math.max(40, cardBody.width - 70)
                                            }
                                        }

                                        MouseArea {
                                            id: optionArea
                                            anchors.fill: parent
                                            hoverEnabled: true
                                            cursorShape: Qt.PointingHandCursor
                                            onClicked: {
                                                let next = Object.assign({}, cardRoot.picks)
                                                next[questionBlock.question] = modelData.label
                                                cardRoot.picks = next
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // 提交回答. Only a complete answer set leaves this card: an
                        // agent question with one answer missing is not an answer, and
                        // the daemon refuses it rather than letting the tool run with
                        // questions still open.
                        RowLayout {
                            width: parent.width
                            spacing: 8
                            visible: cardRoot.isQuestion

                            Text {
                                Layout.fillWidth: true
                                text: cardRoot.picked + " / " + cardRoot.asked
                                      + (cardRoot.ready ? " 已选，可提交" : " 已选")
                                color: cardRoot.ready ? "#7bf3a0" : "#8e8e93"
                                font.pixelSize: 11
                            }

                            Rectangle {
                                Layout.preferredWidth: 104
                                Layout.preferredHeight: 28
                                radius: 8
                                color: !cardRoot.ready ? "#1c1c1e"
                                       : (submitArea.containsMouse ? "#3d8b52" : "#30d158")
                                Text {
                                    anchors.centerIn: parent
                                    text: "提交回答"
                                    color: cardRoot.ready ? "#04150a" : "#636366"
                                    font.pixelSize: 12
                                    font.bold: cardRoot.ready
                                }
                                MouseArea {
                                    id: submitArea
                                    anchors.fill: parent
                                    enabled: cardRoot.ready
                                    hoverEnabled: true
                                    cursorShape: Qt.PointingHandCursor
                                    onClicked: {
                                        let out = []
                                        for (let i = 0; i < cardRoot.asked; ++i) {
                                            const q = cardRoot.questions[i].question
                                            out.push({ question: q, label: cardRoot.picks[q] })
                                        }
                                        console.log("question submitted, requestId=" + requestId)
                                        daemon.answer(requestId, out)
                                    }
                                }
                            }
                        }

                        RowLayout {
                            width: parent.width
                            spacing: 8
                            // Column skips an invisible child, so hiding is enough.
                            // A manual `height: visible ? implicitHeight : 0` here made
                            // a binding loop with `maximumLineCount`, whose implicit
                            // height depends on the height it was feeding.
                            visible: !cardRoot.isQuestion
                            Item { Layout.fillWidth: true }

                            // 拒绝 / 批准. These are the two outcomes the daemon can
                            // hand back; anything else is silence, and that path is
                            // taken by the clock or a vanished island, never a button.
                            Rectangle {
                                Layout.preferredWidth: 74
                                Layout.preferredHeight: 28
                                radius: 8
                                color: rejectArea.containsMouse ? "#3a2f22" : "#2c2c2e"
                                border.width: 1
                                border.color: "#48484a"
                                Text {
                                    anchors.centerIn: parent
                                    text: "拒绝"
                                    color: "#eaeaea"
                                    font.pixelSize: 12
                                }
                                MouseArea {
                                    id: rejectArea
                                    anchors.fill: parent
                                    hoverEnabled: true
                                    cursorShape: Qt.PointingHandCursor
                                    onClicked: {
                                        console.log("reject clicked, requestId=" + requestId)
                                        daemon.decide(requestId, "deny")
                                    }
                                }
                            }

                            Rectangle {
                                Layout.preferredWidth: 74
                                Layout.preferredHeight: 28
                                Layout.alignment: Qt.AlignVCenter
                                radius: 8
                                color: allowArea.containsMouse ? "#3d8b52" : "#30d158"
                                Text {
                                    anchors.centerIn: parent
                                    text: "批准"
                                    color: "#04150a"
                                    font.pixelSize: 12
                                    font.bold: true
                                }
                                MouseArea {
                                    id: allowArea
                                    anchors.fill: parent
                                    hoverEnabled: true
                                    cursorShape: Qt.PointingHandCursor
                                    onClicked: {
                                        console.log("allow clicked, requestId=" + requestId)
                                        daemon.decide(requestId, "allow")
                                    }
                                }
                            }
                        }
                    }
                }
            }

            Repeater {
                model: sessions
                delegate: Rectangle {
                    required property int index
                    required property string sessionId
                    required property string source
                    required property string origin
                    required property string status
                    required property string project
                    required property string detail
                    required property real lastEventMs

                    // The list is capped so a long tail of sessions cannot push the
                    // island past the screen; the "+N more" row below counts what is
                    // hidden here. Positioners leave invisible children out of both
                    // the layout and implicitHeight, which is what the window height
                    // is now derived from -- so hidden cards cost no space at all.
                    visible: index < island.visibleCount

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
                                // A discovered row is a statement about a file's mtime, not
                                // about what an agent is waiting for -- say so on the row
                                // rather than letting an inference look like a report.
                                text: origin === "discovered" ? source + " ·推断" : source
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
