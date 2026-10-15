#include "daemon_client.h"

#include "permission_model.h"
#include "sessions_model.h"

#include <QDebug>
#include <QLocalSocket>
#include <QJsonArray>
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

DaemonClient::DaemonClient(SessionsModel *model, PermissionModel *permissions,
                           QObject *parent)
    : QObject(parent)
    , m_model(model)
    , m_permissions(permissions)
    , m_socket(new QLocalSocket(this))
    // Same rule the daemon applies to CODEISLAND_NOTIFY_ON_STOP, read once: the
    // two must agree unless the human flips the bell, which is exactly what the
    // set_notify command is for.
    , m_notificationsOn([] {
        const QByteArray raw = qgetenv("CODEISLAND_NOTIFY_ON_STOP").toLower().trimmed();
        return !(raw == "0" || raw == "false" || raw == "off" || raw == "no");
    }())
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
        // A card the human can no longer reach must disappear: the daemon has
        // already released the parked hook, so clicking this button now would
        // decide nothing while looking like it decided something.
        m_permissions->clearAll();
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

void DaemonClient::decide(const QString &requestId, const QString &behavior)
{
    // Every exit from here is logged. An empty requestId used to return silently,
    // which made "the button does nothing" indistinguishable from "the click never
    // arrived" -- exactly the debugging dead end this hit.
    qInfo() << "codeisland-shell: decide() called" << requestId << behavior;

    if (requestId.isEmpty()) {
        qWarning() << "codeisland-shell: decide() dropped: empty request id"
                      " (the card lost its role data)";
        return;
    }
    QString normalised = behavior.toLower();
    if (normalised != QLatin1String("allow") && normalised != QLatin1String("deny")) {
        qWarning() << "codeisland-shell: refusing to send an unknown behaviour"
                   << behavior;
        return;
    }

    QJsonObject payload{
        {QStringLiteral("type"), QStringLiteral("permission_decision")},
        {QStringLiteral("request_id"), requestId},
        {QStringLiteral("behavior"), normalised},
    };
    writeDecision(payload);
}

void DaemonClient::answer(const QString &requestId, const QVariantList &answers)
{
    qInfo() << "codeisland-shell: answer() called" << requestId << "entries:" << answers.size();

    if (requestId.isEmpty()) {
        qWarning() << "codeisland-shell: answer() dropped: empty request id";
        return;
    }
    if (answers.isEmpty()) {
        // Sending an empty answer set would be refused by the daemon anyway; saying
        // nothing here keeps the log honest about which side dropped the click.
        qWarning() << "codeisland-shell: answer() dropped: no options selected";
        return;
    }

    // QJsonObject takes QJsonValue, not QVariant: a QVariantList here does not
    // convert, so the pairs are assembled as JSON from the start.
    QJsonArray mapped;
    for (const QVariant &entry : answers) {
        const QVariantMap map = entry.toMap();
        const QString question = map.value(QStringLiteral("question")).toString();
        const QString label = map.value(QStringLiteral("label")).toString();
        if (question.isEmpty() || label.isEmpty()) {
            qWarning() << "codeisland-shell: answer() dropped: incomplete pair" << map;
            return;
        }
        mapped.append(QJsonObject{
            {QStringLiteral("question"), question},
            {QStringLiteral("label"), label},
        });
    }

    // Answering a question *is* allowing the tool; the CLI only wants the rewritten
    // input back, which the daemon builds from these pairs.
    QJsonObject payload{
        {QStringLiteral("type"), QStringLiteral("permission_decision")},
        {QStringLiteral("request_id"), requestId},
        {QStringLiteral("behavior"), QStringLiteral("allow")},
        {QStringLiteral("answers"), mapped},
    };
    writeDecision(payload);
}

void DaemonClient::jump(const QString &sessionId)
{
    qInfo() << "codeisland-shell: jump() called" << sessionId;
    if (sessionId.isEmpty()) {
        qWarning() << "codeisland-shell: jump() dropped: empty session id";
        return;
    }
    if (m_socket->state() != QLocalSocket::ConnectedState) {
        qWarning() << "codeisland-shell: jump dropped, daemon not connected";
        return;
    }
    QJsonObject payload{
        {QStringLiteral("type"), QStringLiteral("jump")},
        {QStringLiteral("session_id"), sessionId},
    };
    QByteArray line = QJsonDocument(payload).toJson(QJsonDocument::Compact);
    line.append('\n');
    if (m_socket->write(line) < 0) {
        qWarning() << "codeisland-shell: failed to send jump" << m_socket->errorString();
    }
}

void DaemonClient::withdraw(const QString &requestId)
{
    qInfo() << "codeisland-shell: withdraw() called" << requestId;
    if (requestId.isEmpty()) {
        qWarning() << "codeisland-shell: withdraw() dropped: empty request id";
        return;
    }
    // Not a decision, so not through writeDecision's logging wording: that helper
    // exists so decide/answer cannot drift apart, and this carries no behavior at
    // all -- the payload is deliberately too small to smuggle an answer in.
    QJsonObject payload{
        {QStringLiteral("type"), QStringLiteral("withdraw")},
        {QStringLiteral("request_id"), requestId},
    };
    QByteArray line = QJsonDocument(payload).toJson(QJsonDocument::Compact);
    line.append('\n');
    if (m_socket->state() != QLocalSocket::ConnectedState) {
        // Nothing is parked on our behalf anymore: the daemon released it when the
        // connection dropped. Skipping then has nothing to skip.
        qWarning() << "codeisland-shell: withdraw dropped, daemon not connected";
        return;
    }
    if (m_socket->write(line) < 0) {
        qWarning() << "codeisland-shell: failed to send withdraw" << m_socket->errorString();
    }
}

void DaemonClient::writeDecision(const QJsonObject &payload)
{    QByteArray line = QJsonDocument(payload).toJson(QJsonDocument::Compact);
    line.append('\n');

    if (m_socket->state() != QLocalSocket::ConnectedState) {
        // The daemon already released this hook when our connection dropped, so
        // there is nothing to answer. Say so instead of pretending.
        qWarning() << "codeisland-shell: decision dropped, daemon not connected";
        return;
    }
    qInfo() << "codeisland-shell: sending decision" << line.trimmed();
    if (m_socket->write(line) < 0) {
        qWarning() << "codeisland-shell: failed to send decision" << m_socket->errorString();
    }
    m_socket->flush();
}

void DaemonClient::connectToDaemon()
{
    if (m_stopping) {
        return;
    }
    const QString path = resolveViewSocketPath();
    // ReadWrite, not ReadOnly. The island both receives updates and sends
    // decisions; opening read-only made every button click fail at write() with
    // "QIODevice::write (QLocalSocket): ReadOnly device" while looking from the
    // outside like the click had never arrived at all.
    m_buffer.clear();
    m_socket->abort();
    m_socket->connectToServer(path, QIODevice::ReadWrite);
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
        // One line per message. Without this a card that never appeared was
        // indistinguishable from a message that never arrived.
        qInfo() << "codeisland-shell: received" << type;
        if (type == QStringLiteral("snapshot")) {
            m_model->applySnapshot(object.value(QStringLiteral("sessions")).toVariant().toList());
        } else if (type == QStringLiteral("session_upsert")) {
            m_model->applyUpsert(object.value(QStringLiteral("session")).toVariant().toMap());
        } else if (type == QStringLiteral("session_remove")) {
            m_model->applyRemove(object.value(QStringLiteral("id")).toString());
        } else if (type == QStringLiteral("permission_requested")) {
            m_permissions->addRequest(
                object.value(QStringLiteral("request")).toVariant().toMap());
        } else if (type == QStringLiteral("permission_resolved")) {
            m_permissions->resolveRequest(
                object.value(QStringLiteral("request_id")).toString());
        }
    }
}

void DaemonClient::scheduleReconnect()
{
    if (!m_stopping && !m_retry.isActive()) {
        m_retry.start();
    }
}

void DaemonClient::setNotificationsOn(bool on)
{
    if (on == m_notificationsOn) {
        return;
    }
    m_notificationsOn = on;
    emit notificationsChanged();
    qInfo() << "codeisland-shell: setNotificationsOn() called" << on;

    QJsonObject payload{
        {QStringLiteral("type"), QStringLiteral("set_notify")},
        {QStringLiteral("on"), on},
    };
    QByteArray line = QJsonDocument(payload).toJson(QJsonDocument::Compact);
    line.append('\n');
    if (m_socket->state() != QLocalSocket::ConnectedState) {
        // The daemon keeps its own startup value until it can be told otherwise;
        // say so rather than pretending the bell flipped.
        qWarning() << "codeisland-shell: set_notify dropped, daemon not connected";
        return;
    }
    if (m_socket->write(line) < 0) {
        qWarning() << "codeisland-shell: failed to send set_notify" << m_socket->errorString();
    }
}

QVariantMap DaemonClient::diagnostics() const
{
    auto env = [](const char *name) {
        const QByteArray value = qgetenv(name);
        return value.isEmpty() ? QStringLiteral("（未设置，用默认）") : QString::fromLocal8Bit(value);
    };
    return {
        {QStringLiteral("viewSocket"), resolveViewSocketPath()},
        {QStringLiteral("qmlDir"), QStringLiteral(CODEISLAND_QML_DIR)},
        {QStringLiteral("notifyOnStop"), env("CODEISLAND_NOTIFY_ON_STOP")},
        {QStringLiteral("hookStaleMs"), env("CODEISLAND_HOOK_STALE_MS")},
        {QStringLiteral("jumpCommand"), env("CODEISLAND_JUMP_COMMAND")},
        {QStringLiteral("notificationsOn"), m_notificationsOn},
        {QStringLiteral("connected"), m_connected},
    };
}
