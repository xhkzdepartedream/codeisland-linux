#include "daemon_client.h"

#include "sessions_model.h"

#include <QDebug>
#include <QLocalSocket>
#include <QJsonDocument>
#include <QJsonObject>
#include <QJsonParseError>

#include <unistd.h>

namespace {

QString resolveViewSocketPath()
{
    const QByteArray override = qgetenv("CODEISLAND_VIEW_SOCKET");
    if (!override.isEmpty()) {
        return QString::fromLocal8Bit(override);
    }
    QByteArray base = qgetenv("XDG_RUNTIME_DIR");
    if (base.isEmpty()) {
        base = "/tmp";
    }
    // The daemon writes to a uid-scoped path, so it must be the real uid: a
    // setuid invocation would otherwise look in the wrong place.
    const uid_t uid = getuid();
    return QString::fromLocal8Bit(base) + QStringLiteral("/codeisland-view-")
        + QString::number(static_cast<qulonglong>(uid)) + QStringLiteral(".sock");
}

constexpr int kRetryMs = 1500;

} // namespace

DaemonClient::DaemonClient(SessionsModel *model, QObject *parent)
    : QObject(parent)
    , m_model(model)
    , m_socket(new QLocalSocket(this))
{
    m_retry.setSingleShot(true);
    m_retry.setInterval(kRetryMs);
    connect(&m_retry, &QTimer::timeout, this, &DaemonClient::connectToDaemon);

    connect(m_socket, &QLocalSocket::connected, this, [this] {
        m_connected = true;
        emit connectedChanged();
    });

    connect(m_socket, &QLocalSocket::readyRead, this, &DaemonClient::consumeBuffer);

    // Any loss of the daemon empties the island. See the class comment.
    const auto dropped = [this] {
        m_connected = false;
        emit connectedChanged();
        m_model->clearAll();
        if (!m_stopping) {
            scheduleReconnect();
        }
    };
    connect(m_socket, &QLocalSocket::disconnected, this, dropped);
    connect(m_socket,
            QOverload<QLocalSocket::LocalSocketError>::of(&QLocalSocket::errorOccurred),
            this,
            [this, dropped](QLocalSocket::LocalSocketError error) {
                // A missing daemon is the normal state before it starts, so it is
                // not worth a warning on every retry.
                // ServerNotFoundError: the daemon has not been started yet.
                // ConnectionRefusedError: a stale socket file with nothing behind
                // it, i.e. the daemon died. Both are routine, so neither deserves a
                // warning once per retry.
                if (error == QLocalSocket::ServerNotFoundError
                    || error == QLocalSocket::ConnectionRefusedError
                    || error == QLocalSocket::PeerClosedError) {
                    dropped();
                } else {
                    qWarning() << "codeisland-shell: view socket error" << error;
                    dropped();
                }
            });
}

DaemonClient::~DaemonClient()
{
    stop();
}

void DaemonClient::start()
{
    m_stopping = false;
    connectToDaemon();
}

void DaemonClient::stop()
{
    m_stopping = true;
    m_retry.stop();
    if (m_socket->state() != QLocalSocket::UnconnectedState) {
        m_socket->abort();
    }
}

void DaemonClient::connectToDaemon()
{
    if (m_stopping) {
        return;
    }
    const QString path = resolveViewSocketPath();
    // QLocalSocket takes the trailing slash off absolute paths itself, so pass
    // the path exactly as the daemon bound it.
    m_buffer.clear();
    m_socket->abort();
    m_socket->connectToServer(path, QIODevice::ReadOnly);
}

void DaemonClient::consumeBuffer()
{
    m_buffer.append(m_socket->readAll());

    // The daemon frames updates as newline-delimited JSON. Only whole lines are
    // consumed; a partial tail stays buffered for the next readyRead, so a JSON
    // object split across packets is never parsed half.
    int newline = -1;
    while ((newline = m_buffer.indexOf('\n')) >= 0) {
        const QByteArray line = m_buffer.left(newline).trimmed();
        m_buffer.remove(0, newline + 1);
        if (line.isEmpty()) {
            continue;
        }

        QJsonParseError parseError;
        const QJsonDocument document = QJsonDocument::fromJson(line, &parseError);
        if (parseError.error != QJsonParseError::NoError || !document.isObject()) {
            qWarning() << "codeisland-shell: malformed update:" << parseError.errorString();
            continue;
        }

        const QJsonObject object = document.object();
        const QString type = object.value(QStringLiteral("type")).toString();
        if (type == QStringLiteral("snapshot")) {
            m_model->applySnapshot(object.value(QStringLiteral("sessions")).toVariant().toList());
        } else if (type == QStringLiteral("session_upsert")) {
            m_model->applyUpsert(object.value(QStringLiteral("session")).toVariant().toMap());
        } else if (type == QStringLiteral("session_remove")) {
            m_model->applyRemove(object.value(QStringLiteral("id")).toString());
        }
    }
}

void DaemonClient::scheduleReconnect()
{
    if (!m_stopping && !m_retry.isActive()) {
        m_retry.start();
    }
}
