#pragma once

// Tool calls waiting for a yes/no, as the island shows them.
//
// Separate from SessionsModel on purpose. A session describes what an agent is
// doing; a permission describes one decision that must be made *now*, and the
// two have different lifetimes -- a session can keep existing after its request
// is resolved, and one session can hold several requests. Folding them into one
// row would force the card to guess which question it is answering.

#include <QAbstractListModel>
#include <QString>
#include <QVariantList>

class PermissionModel final : public QAbstractListModel
{
    Q_OBJECT
    Q_PROPERTY(int count READ rowCount NOTIFY changed)

public:
    enum Roles {
        RequestIdRole = Qt::UserRole + 1,
        SessionIdRole,
        SourceRole,
        ToolRole,
        SummaryRole,
        ProjectRole,
        QuestionsRole,
    };

    explicit PermissionModel(QObject *parent = nullptr);

    int rowCount(const QModelIndex &parent = {}) const override;
    QVariant data(const QModelIndex &index, int role) const override;
    QHash<int, QByteArray> roleNames() const override;

    /// Show a card. Called for each `permission_requested` the daemon sends.
    void addRequest(const QVariantMap &request);
    /// Hide one card, by request id.
    void resolveRequest(const QString &requestId);
    /// Hide everything: the connection to the daemon is gone, so nobody is
    /// waiting on this island any more.
    void clearAll();

    bool hasPending() const { return !m_rows.isEmpty(); }

signals:
    void changed();

private:
    struct Row {
        QString requestId;
        QString sessionId;
        QString source;
        QString tool;
        QString summary;
        QString project;
        /// Question cards only: [{question, header, options:[{label, description}]}].
        /// Empty for a plain approve/deny gate, which is how the QML tells the two
        /// apart -- the same field the daemon sends, not a second flag to keep in sync.
        QVariantList questions;
    };
    QVector<Row> m_rows;
};
