#pragma once

// Live session list, fed by the daemon's view socket and read by the QML cards.
//
// Why a model instead of a plain property: the island renders one row per agent
// session, and rows appear, change and disappear continuously. QAbstractListModel
// is the smallest thing that keeps QML correct without the QML side having to
// diff anything -- the daemon already sends whole sessions, last write wins.

#include <QAbstractListModel>
#include <QDateTime>
#include <QHash>
#include <QQmlEngine>
#include <QVariantList>

struct SessionRow {
    QString id;
    QString source;
    QString status;
    QString cwd;
    QString detail;
    int toolCalls = 0;
    qint64 lastEventMs = 0;
    qint64 startedMs = 0;
};

class SessionsModel final : public QAbstractListModel
{
    Q_OBJECT
    /// Highest-urgency status present, so the collapsed pill can pick one colour
    /// without QML looping over rows on every repaint.
    Q_PROPERTY(QString headlineStatus READ headlineStatus NOTIFY changed)
    /// Number of sessions currently needing a human.
    Q_PROPERTY(int needingAttention READ needingAttention NOTIFY changed)
    Q_PROPERTY(int count READ rowCount NOTIFY changed)

public:
    enum Roles {
        IdRole = Qt::UserRole + 1,
        SourceRole,
        StatusRole,
        CwdRole,
        ProjectRole,
        DetailRole,
        ToolCallsRole,
        LastEventMsRole,
        StartedMsRole,
    };

    explicit SessionsModel(QObject *parent = nullptr);

    int rowCount(const QModelIndex &parent = {}) const override;
    QVariant data(const QModelIndex &index, int role) const override;
    QHash<int, QByteArray> roleNames() const override;

    /// Apply one `session_upsert` payload.
    Q_INVOKABLE void applyUpsert(const QVariantMap &session);
    /// Apply one `session_remove` payload.
    Q_INVOKABLE void applyRemove(const QString &id);
    /// Replace the whole list from a `snapshot` payload.
    Q_INVOKABLE void applySnapshot(const QVariantList &sessions);
    /// Drop everything: used when the daemon connection is lost, because a stale
    /// island claiming a session is waiting is worse than an empty one.
    Q_INVOKABLE void clearAll();

    QString headlineStatus() const;
    int needingAttention() const;

    /// Last path component of the working directory: the card's title.
    static QString projectLabel(const QString &cwd, const QString &fallback);

signals:
    void changed();

private:
    // Urgency ranks, low first. Mirrors SessionStatus::urgency in Rust; the two
    // must stay in step or the island and the daemon disagree about ordering.
    static int urgency(const QString &status);
    void sortRows();

    QVector<SessionRow> m_rows;
    QHash<QString, int> m_indexById;
};

QML_DECLARE_TYPE(SessionsModel)
