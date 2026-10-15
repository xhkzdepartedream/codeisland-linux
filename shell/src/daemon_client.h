#pragma once

// Subscribes to the daemon's view socket and feeds SessionsModel.
//
// Deliberately read-only and deliberately not a decision-maker: with this
// connection down the island goes empty rather than keeping stale rows, because
// a card claiming "waiting for you" when nothing is waiting is worse than no
// card. The daemon answers permission requests with silence in that window, which
// hands the decision back to the CLI -- the safety contract, unchanged.

#include <QJsonObject>
#include <QObject>
#include <QTimer>
#include <QVariant>

class QLocalSocket;
class PermissionModel;
class SessionsModel;

class DaemonClient final : public QObject
{
    Q_OBJECT
    Q_PROPERTY(bool connected READ isConnected NOTIFY connectedChanged)

public:
    explicit DaemonClient(SessionsModel *model, PermissionModel *permissions,
                          QObject *parent = nullptr);
    ~DaemonClient() override;

    bool isConnected() const { return m_connected; }

    /// Where to connect. Mirrors the daemon's resolution order:
    /// CODEISLAND_VIEW_SOCKET, then $XDG_RUNTIME_DIR/codeisland-view-<uid>.sock.
    Q_INVOKABLE void start();
    Q_INVOKABLE void stop();

    /// Hand a human's decision back to the daemon.
    ///
    /// `behavior` is "allow" or "deny". Anything else is dropped rather than
    /// guessed: a malformed click must not silently become an approval.
    Q_INVOKABLE void decide(const QString &requestId, const QString &behavior);

    /// Answer a question card. `answers` is a list of {question, label} maps, one
    /// per question, each label taken from that question's own options.
    ///
    /// A question cannot be answered with `decide()`: approving `AskUserQuestion`
    /// without an answer would run the tool with its questions still open, and the
    /// daemon refuses that combination rather than passing it on.
    Q_INVOKABLE void answer(const QString &requestId, const QVariantList &answers);

    /// Bring the terminal this session runs in to the front.
    ///
    /// A view action only: it does not answer anything, and the daemon keeps the
    /// hook parked. If the terminal cannot be found the daemon logs why and the
    /// card simply stays where it is.
    Q_INVOKABLE void jump(const QString &sessionId);

    /// Approve *and* record a session-scoped "always allow this tool" rule.
    ///
    /// Sends `session_rule: true` with an allow decision. The daemon only honours
    /// it for Claude Code -- the one source whose reply envelope is verified to
    /// carry the rule; any other source degrades to a one-time allow, and the
    /// button is only shown for claude cards anyway.
    Q_INVOKABLE void decideAlways(const QString &requestId);

    /// Skip a card: the human will answer in the terminal instead.
    ///
    /// Neither allow nor deny -- the daemon releases the hook to silence so the
    /// CLI falls back to its own prompt. The one command that can always be sent:
    /// it promises nothing on the human's behalf.
    Q_INVOKABLE void withdraw(const QString &requestId);

    /// The toolbar bell state: whether the daemon notifies when a turn finishes.
    /// Initial value mirrors the daemon's reading of CODEISLAND_NOTIFY_ON_STOP;
    /// flipping it sends `set_notify`, which changes only what the human is told.
    Q_PROPERTY(bool notificationsOn READ notificationsOn NOTIFY notificationsChanged)
    bool notificationsOn() const { return m_notificationsOn; }
    Q_INVOKABLE void setNotificationsOn(bool on);

    /// What the settings sheet shows: effective sockets and configuration. Read-only
    /// -- the whole project is configured by environment variables (F26), and a
    /// display of the live truth beats a settings UI that lies about what it can
    /// change.
    Q_INVOKABLE QVariantMap diagnostics() const;

signals:
    void connectedChanged();
    void notificationsChanged();

private:
    void connectToDaemon();
    void consumeBuffer();
    void scheduleReconnect();
    /// Serialise and send one decision. Both `decide()` and `answer()` go through
    /// here so the socket-state check and the logging cannot drift apart.
    void writeDecision(const QJsonObject &payload);

    SessionsModel *m_model;
    PermissionModel *m_permissions;
    QLocalSocket *m_socket;
    QTimer m_retry;
    QByteArray m_buffer;
    bool m_connected = false;
    bool m_stopping = false;
    bool m_notificationsOn = true;
};
