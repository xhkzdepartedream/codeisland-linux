#include "permission_model.h"

PermissionModel::PermissionModel(QObject *parent)
    : QAbstractListModel(parent)
{
}

int PermissionModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : m_rows.size();
}

QVariant PermissionModel::data(const QModelIndex &index, int role) const
{
    if (!index.isValid() || index.row() < 0 || index.row() >= m_rows.size()) {
        return {};
    }
    const Row &row = m_rows.at(index.row());
    switch (role) {
    case RequestIdRole:
        return row.requestId;
    case SessionIdRole:
        return row.sessionId;
    case SourceRole:
        return row.source;
    case ToolRole:
        return row.tool;
    case SummaryRole:
        return row.summary;
    case ProjectRole:
        return row.project;
    case QuestionsRole:
        return row.questions;
    }
    return {};
}

QHash<int, QByteArray> PermissionModel::roleNames() const
{
    return {
        {RequestIdRole, "requestId"},
        {SessionIdRole, "sessionId"},
        {SourceRole, "source"},
        {ToolRole, "tool"},
        {SummaryRole, "summary"},
        {ProjectRole, "project"},
        {QuestionsRole, "questions"},
    };
}

void PermissionModel::addRequest(const QVariantMap &request)
{
    const QString requestId = request.value(QStringLiteral("request_id")).toString();
    if (requestId.isEmpty()) {
        return;
    }
    for (int i = 0; i < m_rows.size(); ++i) {
        if (m_rows.at(i).requestId == requestId) {
            return; // already shown; the daemon retries must not stack cards
        }
    }

    Row row;
    row.requestId = requestId;
    row.sessionId = request.value(QStringLiteral("session_id")).toString();
    row.source = request.value(QStringLiteral("source")).toString();
    row.tool = request.value(QStringLiteral("tool_name")).toString();
    row.summary = request.value(QStringLiteral("summary")).toString();
    const QString cwd = request.value(QStringLiteral("cwd")).toString();
    const int slash = cwd.lastIndexOf(QLatin1Char('/'));
    row.project = slash >= 0 ? cwd.mid(slash + 1) : cwd;

    // Keep the daemon's field names verbatim (`question`, `header`, `options`).
    // A question the daemon could not answer never reaches this list: it refuses the
    // payload upstream and stays silent, so anything here is safe to answer.
    const QVariantList incoming = request.value(QStringLiteral("questions")).toList();
    for (const QVariant &entry : incoming) {
        const QVariantMap question = entry.toMap();
        QVariantList options;
        for (const QVariant &option : question.value(QStringLiteral("options")).toList()) {
            const QVariantMap map = option.toMap();
            options.append(QVariantMap{
                {QStringLiteral("label"), map.value(QStringLiteral("label"))},
                {QStringLiteral("description"), map.value(QStringLiteral("description"))},
            });
        }
        row.questions.append(QVariantMap{
            {QStringLiteral("question"), question.value(QStringLiteral("question"))},
            {QStringLiteral("header"), question.value(QStringLiteral("header"))},
            {QStringLiteral("options"), options},
        });
    }

    beginInsertRows(QModelIndex(), m_rows.size(), m_rows.size());
    m_rows.append(row);
    endInsertRows();
    emit changed();
}

void PermissionModel::resolveRequest(const QString &requestId)
{
    for (int i = 0; i < m_rows.size(); ++i) {
        if (m_rows.at(i).requestId == requestId) {
            beginRemoveRows(QModelIndex(), i, i);
            m_rows.removeAt(i);
            endRemoveRows();
            emit changed();
            return;
        }
    }
}

void PermissionModel::clearAll()
{
    if (m_rows.isEmpty()) {
        return;
    }
    beginResetModel();
    m_rows.clear();
    endResetModel();
    emit changed();
}

QVariantMap PermissionModel::at(int index) const
{
    if (index < 0 || index >= m_rows.size()) {
        return {};
    }
    const Row &row = m_rows.at(index);
    return {
        {QStringLiteral("requestId"), row.requestId},
        {QStringLiteral("sessionId"), row.sessionId},
        // The same signal the QML delegate uses to tell the two cards apart:
        // a card with questions is a question card, everything else is a plain
        // approve/deny gate.
        {QStringLiteral("isQuestion"), !row.questions.isEmpty()},
    };
}
