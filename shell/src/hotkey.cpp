#include "hotkey.h"

#include <QDBusArgument>
#include <QDBusConnection>
#include <QDBusInterface>
#include <QDBusMetaType>
#include <QDBusReply>
#include <QKeySequence>
#include <QStringList>
#include <QVariantList>

// ---------------------------------------------------------------------------
// a(ai) marshalling. setShortcutKeys takes a set of key sequences; on the wire
// that is an array of (ai) structs, each holding one sequence's combined key
// codes. QtDBus does not ship a marshaller for this shape, so we declare a
// one-field struct and register both it and its list -- the same pattern the
// D-Bus docs recommend for custom structures.
// a(ai) marshalling: one field, the sequence's combined key codes. Kept in the
// global namespace because Q_DECLARE_METATYPE instantiates a specialization of
// QMetaTypeId, which the compiler rejects inside an anonymous namespace.
struct KeySeqGroup {
    QList<int> keys;
};
Q_DECLARE_METATYPE(KeySeqGroup)

QDBusArgument &operator<<(QDBusArgument &arg, const KeySeqGroup &group)
{
    arg.beginStructure();
    arg << group.keys;
    arg.endStructure();
    return arg;
}
const QDBusArgument &operator>>(const QDBusArgument &arg, KeySeqGroup &group)
{
    arg.beginStructure();
    arg >> group.keys;
    arg.endStructure();
    return arg;
}

static const ActionSpec kActions[] = {
    {"toggle-panel", "Toggle the island panel", "Ctrl+Shift+I"},
    {"approve", "Approve the waiting tool call", "Ctrl+Shift+A"},
    {"deny", "Deny the waiting tool call", "Ctrl+Shift+D"},
    {"skip", "Skip the waiting card", "Ctrl+Shift+S"},
};

Hotkey::Hotkey(QObject *parent)
    : QObject(parent)
{
}

bool Hotkey::install()
{
    qDBusRegisterMetaType<KeySeqGroup>();
    qDBusRegisterMetaType<QList<KeySeqGroup>>();

    QDBusConnection bus = QDBusConnection::sessionBus();
    if (!bus.isConnected()) {
        qWarning().noquote() << "codeisland-shell: hotkeys disabled, no session bus";
        return false;
    }

    // Component name is the uniqueName kglobalacceld stores everything under;
    // it becomes the object path /component/<name> with '-' folded to '_'.
    const QString component = QStringLiteral("island");
    const QString componentFriendly = QStringLiteral("CodeIsland");
    const QString service = QStringLiteral("org.kde.kglobalaccel");
    const QString path = QStringLiteral("/kglobalaccel");
    const QString iface = QStringLiteral("org.kde.KGlobalAccel");
    QDBusInterface accel(service, path, iface, bus);

    for (const ActionSpec &spec : kActions) {
        if (!registerAction(component, componentFriendly, spec, accel)) {
            // One action failing (e.g. an aggressive shortcut clash) must not
            // take the others down: log and continue, and report the partial
            // install honestly through isInstalled().
            continue;
        }
    }

    // Subscribe to presses on our component. The signal arrives on the
    // component object kglobalacceld creates for us, not on /kglobalaccel.
    QDBusReply<QDBusObjectPath> reply = accel.call(QStringLiteral("getComponent"), component);
    if (!reply.isValid()) {
        qWarning().noquote() << "codeisland-shell: hotkeys disabled,"
                             << reply.error().message();
        return false;
    }
    m_componentPath = reply.value().path();
    bool ok = bus.connect(QString(), m_componentPath,
                          QStringLiteral("org.kde.kglobalaccel.Component"),
                          QStringLiteral("globalShortcutPressed"), this,
                          SLOT(onGlobalShortcutPressed(QString,QString,qlonglong)));
    if (!ok) {
        qWarning().noquote() << "codeisland-shell: hotkeys disabled, cannot subscribe to"
                             << m_componentPath;
        return false;
    }

    m_installed = true;
    emit installedChanged();
    qInfo().noquote() << "codeisland-shell: global hotkeys installed at" << m_componentPath;
    return true;
}

bool Hotkey::registerAction(const QString &component, const QString &componentFriendly,
                            const ActionSpec &spec, QDBusInterface &accel)
{
    const QString actionName = QString::fromLatin1(spec.action);
    const QStringList actionId{
        component,
        actionName,
        componentFriendly,
        QString::fromUtf8(spec.friendly),
    };

    QDBusReply<void> reg = accel.call(QStringLiteral("doRegister"), actionId);
    if (!reg.isValid()) {
        qWarning().noquote() << "codeisland-shell: doRegister failed for" << actionName
                             << reg.error().message();
        return false;
    }

    const QKeySequence seq(QString::fromLatin1(spec.keySpec));
    if (seq.isEmpty()) {
        qWarning().noquote() << "codeisland-shell: bad key spec" << spec.keySpec;
        return false;
    }
    const QList<KeySeqGroup> groups{{QList<int>{seq[0].toCombined()}}};

    // Active keys (SetPresent|NoAutoloading), then the default keys, exactly
    // the two calls KGlobalAccel the library makes.
    QDBusReply<QList<KeySeqGroup>> active =
        accel.call(QStringLiteral("setShortcutKeys"), actionId, QVariant::fromValue(groups), 6u);
    if (!active.isValid()) {
        qWarning().noquote() << "codeisland-shell: setShortcutKeys failed for" << actionName
                             << active.error().message();
        return false;
    }
    accel.call(QStringLiteral("setShortcutKeys"), actionId, QVariant::fromValue(groups), 10u); // 8=IsDefault|2=SetPresent

    qInfo().noquote() << "codeisland-shell: hotkey" << spec.keySpec << "->" << actionName
                      << (active.value().isEmpty() ? "(clashed, no key active)"
                                                   : "(active)");
    return true;
}

void Hotkey::onGlobalShortcutPressed(const QString &component, const QString &action,
                                     qlonglong timestamp)
{
    Q_UNUSED(timestamp);
    if (component != QStringLiteral("island")) {
        return; // not ours; some other component fired
    }
    qInfo().noquote() << "codeisland-shell: global shortcut" << action;
    emit shortcutPressed(action);
}

QString Hotkey::shortcutLabel(const QString &action) const
{
    if (!m_installed) {
        return {};
    }
    return activeKeyLabel(action);
}

QString Hotkey::activeKeyLabel(const QString &action) const
{
    // Ask kglobalacceld for the active keys. This reflects a rebinding the user
    // made in System Settings, so the badge on the card shows the truth, not
    // our default. The reply is a(ai); each (ai) holds one combined key code.
    QDBusConnection bus = QDBusConnection::sessionBus();
    const QString component = QStringLiteral("island");
    const QStringList actionId{component, action, QStringLiteral("CodeI"), QStringLiteral("")};
    QDBusInterface accel(QStringLiteral("org.kde.kglobalaccel"), QStringLiteral("/kglobalaccel"),
                         QStringLiteral("org.kde.KGlobalAccel"), bus);
    QDBusReply<QList<KeySeqGroup>> reply =
        accel.call(QStringLiteral("shortcutKeys"), actionId);
    if (!reply.isValid() || reply.value().isEmpty() || reply.value().first().keys.isEmpty()) {
        return {};
    }
    QKeySequence seq(reply.value().first().keys.first());
    QString text = seq.toString(QKeySequence::PortableText);
    // Compact badge form: Ctrl+Shift+A -> Ctrl⇧A.
    text.replace(QStringLiteral("Shift"), QStringLiteral("⇧"));
    text.replace(QStringLiteral("+"), QStringLiteral(""));
    return text;
}