#include "sessions_model.h"

#include <QDir>

SessionsModel::SessionsModel(QObject *parent)
    : QAbstractListModel(parent)
{
}

int SessionsModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : m_rows.size();
}

QVariant SessionsModel::data(const QModelIndex &index, int role) const
{
    if (!index.isValid() || index.row() < 0 || index.row() >= m_rows.size()) {
        return {};
    }
    const SessionRow &row = m_rows.at(index.row());
    switch (role) {
    case IdRole:
        return row.id;
    case SourceRole:
        return row.source;
    case StatusRole:
        return row.status;
    case CwdRole:
        return row.cwd;
    case ProjectRole:
        return projectLabel(row.cwd, row.source);
    case DetailRole:
        return row.detail;
    case OriginRole:
        return row.origin;
    case ToolCallsRole:
        return row.toolCalls;
    case LastEventMsRole:
        return row.lastEventMs;
    case StartedMsRole:
        return row.startedMs;
    }
    return {};
}

QHash<int, QByteArray> SessionsModel::roleNames() const
{
    return {
        {IdRole, "sessionId"},
        {SourceRole, "source"},
        {StatusRole, "status"},
        {CwdRole, "cwd"},
        {ProjectRole, "project"},
        {DetailRole, "detail"},
        {OriginRole, "origin"},
        {ToolCallsRole, "toolCalls"},
        {LastEventMsRole, "lastEventMs"},
        {StartedMsRole, "startedMs"},
    };
}

void SessionsModel::applyUpsert(const QVariantMap &session)
{
    const QString id = session.value(QStringLiteral("id")).toString();
    if (id.isEmpty()) {
        return;
    }

    SessionRow row;
    row.id = id;
    row.source = session.value(QStringLiteral("source")).toString();
    row.status = session.value(QStringLiteral("status")).toString();
    row.cwd = session.value(QStringLiteral("cwd")).toString();
    row.detail = session.value(QStringLiteral("detail")).toString();
    row.origin = session.value(QStringLiteral("origin")).toString();
    if (row.origin.isEmpty()) {
        // Frames written before discovery existed carry no origin; the authoritative
        // reading is the safe default.
        row.origin = QStringLiteral("hook");
    }
    row.toolCalls = session.value(QStringLiteral("tool_calls")).toInt();
    row.lastEventMs = static_cast<qint64>(session.value(QStringLiteral("last_event_ms")).toDouble());
    row.startedMs = static_cast<qint64>(session.value(QStringLiteral("started_ms")).toDouble());

    int existing = -1;
    for (int i = 0; i < m_rows.size(); ++i) {
        if (m_rows.at(i).id == id) {
            existing = i;
            break;
        }
    }

    if (existing >= 0) {
        m_rows[existing] = row;
        emit dataChanged(index(existing), index(existing));
    } else {
        beginInsertRows(QModelIndex(), m_rows.size(), m_rows.size());
        m_rows.append(row);
        endInsertRows();
    }

    sortRows();
    emit changed();
}

void SessionsModel::applyRemove(const QString &id)
{
    for (int i = 0; i < m_rows.size(); ++i) {
        if (m_rows.at(i).id == id) {
            beginRemoveRows(QModelIndex(), i, i);
            m_rows.removeAt(i);
            endRemoveRows();
            emit changed();
            return;
        }
    }
}

void SessionsModel::applySnapshot(const QVariantList &sessions)
{
    beginResetModel();
    m_rows.clear();
    for (const QVariant &variant : sessions) {
        const QVariantMap map = variant.toMap();
        SessionRow row;
        row.id = map.value(QStringLiteral("id")).toString();
        row.source = map.value(QStringLiteral("source")).toString();
        row.status = map.value(QStringLiteral("status")).toString();
        row.cwd = map.value(QStringLiteral("cwd")).toString();
        row.detail = map.value(QStringLiteral("detail")).toString();
        row.origin = map.value(QStringLiteral("origin")).toString();
        if (row.origin.isEmpty()) {
            row.origin = QStringLiteral("hook");
        }
        row.toolCalls = map.value(QStringLiteral("tool_calls")).toInt();
        row.lastEventMs = static_cast<qint64>(map.value(QStringLiteral("last_event_ms")).toDouble());
        row.startedMs = static_cast<qint64>(map.value(QStringLiteral("started_ms")).toDouble());
        if (!row.id.isEmpty()) {
            m_rows.append(row);
        }
    }
    endResetModel();
    sortRows();
    emit changed();
}

void SessionsModel::clearAll()
{
    if (m_rows.isEmpty()) {
        return;
    }
    beginResetModel();
    m_rows.clear();
    endResetModel();
    emit changed();
}

QString SessionsModel::headlineStatus() const
{
    QString best;
    int bestRank = 100;
    for (const SessionRow &row : m_rows) {
        const int rank = urgency(row.status);
        if (rank < bestRank) {
            bestRank = rank;
            best = row.status;
        }
    }
    return best;
}

int SessionsModel::needingAttention() const
{
    int total = 0;
    for (const SessionRow &row : m_rows) {
        if (row.status == QStringLiteral("waiting_approval")
            || row.status == QStringLiteral("waiting_input")) {
            ++total;
        }
    }
    return total;
}

int SessionsModel::urgency(const QString &status)
{
    // Order must match SessionStatus::urgency() in crates/codeisland-core/src/session.rs.
    if (status == QStringLiteral("waiting_approval")) return 0;
    if (status == QStringLiteral("waiting_input")) return 1;
    if (status == QStringLiteral("error")) return 2;
    if (status == QStringLiteral("working")) return 3;
    if (status == QStringLiteral("done")) return 4;
    return 5;
}

void SessionsModel::sortRows()
{
    std::stable_sort(m_rows.begin(), m_rows.end(), [](const SessionRow &a, const SessionRow &b) {
        const int ua = urgency(a.status);
        const int ub = urgency(b.status);
        if (ua != ub) {
            return ua < ub;
        }
        return a.lastEventMs > b.lastEventMs;
    });
}

QString SessionsModel::projectLabel(const QString &cwd, const QString &fallback)
{
    const QString trimmed = cwd.endsWith(QLatin1Char('/')) ? cwd.left(cwd.length() - 1) : cwd;
    const int slash = trimmed.lastIndexOf(QLatin1Char('/'));
    const QString last = slash >= 0 ? trimmed.mid(slash + 1) : trimmed;
    return last.isEmpty() ? fallback : last;
}
