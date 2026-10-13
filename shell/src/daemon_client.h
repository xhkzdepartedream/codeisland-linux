#pragma once

// Subscribes to the daemon's view socket and feeds SessionsModel.
//
// Deliberately read-only and deliberately not a decision-maker: with this
// connection down the island goes empty rather than keeping stale rows, because
// a card claiming "waiting for you" when nothing is waiting is worse than no
// card. The daemon answers permission requests with silence in that window, which
// hands the decision back to the CLI -- the safety contract, unchanged.

#include <QObject>
#include <QTimer>

class QLocalSocket;
class SessionsModel;

class DaemonClient final : public QObject
{
    Q_OBJECT
    Q_PROPERTY(bool connected READ isConnected NOTIFY connectedChanged)

public:
    explicit DaemonClient(SessionsModel *model, QObject *parent = nullptr);
    ~DaemonClient() override;

    bool isConnected() const { return m_connected; }

    /// Where to connect. Mirrors the daemon's resolution order:
    /// CODEISLAND_VIEW_SOCKET, then $XDG_RUNTIME_DIR/codeisland-view-<uid>.sock.
    Q_INVOKABLE void start();
    Q_INVOKABLE void stop();

signals:
    void connectedChanged();

private:
    void connectToDaemon();
    void consumeBuffer();
    void scheduleReconnect();

    SessionsModel *m_model;
    QLocalSocket *m_socket;
    QTimer m_retry;
    QByteArray m_buffer;
    bool m_connected = false;
    bool m_stopping = false;
};
