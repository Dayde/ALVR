import QtQuick
import QtQuick.Layouts
import org.kde.plasma.plasmoid
import org.kde.plasma.components as PlasmaComponents
import org.kde.plasma.plasma5support as P5Support
import org.kde.kirigami as Kirigami

PlasmoidItem {
    id: root

    readonly property string unit: "alvr-client.service"
    readonly property string watcher: Qt.resolvedUrl("../code/watch-unit.py").toString().replace("file://", "")
    // active, activating, deactivating, inactive, failed, ...
    property string unitState: "unknown"
    // state the running watcher was started with
    property string watchedState: "unknown"
    readonly property bool running: unitState === "active" || unitState === "activating" || unitState === "reloading"
    readonly property bool busy: unitState === "activating" || unitState === "deactivating"
    readonly property color stateColor: unitState === "active" ? Kirigami.Theme.positiveTextColor
        : busy ? Kirigami.Theme.neutralTextColor
        : unitState === "failed" ? Kirigami.Theme.negativeTextColor
        : "transparent"

    function watchCmd(known) {
        return "python3 '" + watcher + "' " + unit + " " + known
    }

    // Event-driven: the watcher sleeps until systemd reports a state change, then we re-arm it
    function watch() {
        watchedState = unitState
        executable.exec(watchCmd(watchedState))
    }

    function toggle() {
        // --no-block: starting waits on Monado/the headset, don't hang until it's up
        executable.exec("systemctl --user --no-block " + (running ? "stop " : "start ") + unit)
    }

    preferredRepresentation: compactRepresentation
    toolTipMainText: "ALVR client"
    toolTipSubText: unitState + "\nClick to " + (running ? "stop" : "start")

    P5Support.DataSource {
        id: executable
        engine: "executable"
        connectedSources: []
        onNewData: (sourceName, data) => {
            disconnectSource(sourceName)
            if (sourceName !== root.watchCmd(root.watchedState)) {
                return
            }
            const s = data["stdout"].trim()
            if (s) {
                root.unitState = s
                root.watch()
            } else {
                retry.start()
            }
        }
        function exec(cmd) {
            connectSource(cmd)
        }
    }

    // only used if the watcher itself fails
    Timer {
        id: retry
        interval: 30000
        onTriggered: root.watch()
    }

    Component.onCompleted: watch()

    compactRepresentation: MouseArea {
        id: compact
        hoverEnabled: true
        onClicked: root.toggle()

        Kirigami.Icon {
            anchors.fill: parent
            source: Qt.resolvedUrl("../images/vr-headset.svg")
            isMask: true
            color: Kirigami.Theme.textColor
            active: compact.containsMouse
            opacity: root.running ? 1 : 0.4
        }

        Rectangle {
            width: Math.round(parent.width / 3)
            height: width
            radius: width / 2
            anchors.right: parent.right
            anchors.bottom: parent.bottom
            color: root.stateColor
            visible: color.a > 0
        }
    }

    fullRepresentation: RowLayout {
        Kirigami.Icon {
            source: Qt.resolvedUrl("../images/vr-headset.svg")
            isMask: true
            color: Kirigami.Theme.textColor
            Layout.preferredWidth: Kirigami.Units.iconSizes.medium
            Layout.preferredHeight: Kirigami.Units.iconSizes.medium
        }
        PlasmaComponents.Switch {
            text: "ALVR client (" + root.unitState + ")"
            checked: root.running
            onToggled: root.toggle()
        }
    }
}
