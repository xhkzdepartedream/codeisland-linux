#pragma once

// Global hotkeys via KDE's kglobalacceld, over plain D-Bus.
//
// Upstream (macOS) uses Carbon's RegisterEventHotKey; on KDE the equivalent is
// org.kde.kglobalaccel. We do not link KF6 -- the D-Bus protocol is small and
// stable, and the shell is deliberately a Qt6-only application.
//
// Registration is two calls per action, exactly what KGlobalAccel the library
// does: doRegister(actionId) creates the component+action, then
// setShortcutKeys(actionId, keys, flags) makes the keys active. The user can
// then rebind them in KDE System Settings and our shortcuts() labels reflect
// the active keys on the next start; we never fight the stored configuration
// because a registered action with a saved shortcut is no longer "fresh".
//
// The daemon is untouched by this feature. approve/deny/skip are the same
// ViewCommands the buttons send -- a hotkey is a faster way to press the same
// button, never a new authority. A press lands in QML as `shortcutPressed`,
// which decides what (if anything) is on screen to act on.

#include <QObject>
#include <QString>

class QDBusInterface;

/// One global action: unique name, friendly name (shown in KDE System
/// Settings), and the default binding. Ctrl+Shift mirrors upstream's ⌘⇧ keys.
struct ActionSpec {
    const char *action;
    const char *friendly;
    const char *keySpec;
};

class Hotkey final : public QObject
{
    Q_OBJECT
    Q_PROPERTY(bool installed READ isInstalled NOTIFY installedChanged)

public:
    explicit Hotkey(QObject *parent = nullptr);

    /// Register every action with kglobalacceld. Returns false (with a logged
    /// reason) when the service is absent -- the island works unchanged without
    /// hotkeys, so a missing KDE session must never take the app down.
    bool install();

    bool isInstalled() const { return m_installed; }

    /// Human label of an action's bound key, for the badge on its card, e.g.
    /// "Ctrl+⇧A". Empty when the action is not installed.
    Q_INVOKABLE QString shortcutLabel(const QString &action) const;

signals:
    void shortcutPressed(const QString &action);
    void installedChanged();

private slots:
    void onGlobalShortcutPressed(const QString &component, const QString &action,
                                 qlonglong timestamp);

private:
    bool registerAction(const QString &component, const QString &componentFriendly,
                        const ActionSpec &spec, QDBusInterface &accel);
    QString activeKeyLabel(const QString &action) const;

    QString m_componentPath;
    bool m_installed = false;
};