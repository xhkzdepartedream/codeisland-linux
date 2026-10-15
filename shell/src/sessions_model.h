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
    /// The conversation's own title, when the harness stated one (codex thread
    /// name, claude custom-title, or the first user prompt). The card leads with
    /// it and falls back to the project label when empty -- which is why it is
    /// kept as a plain string instead of an optional.
    QString title;
    QString status;
    QString cwd;
    QString detail;
    /// "hook" when the agent reported itself, "discovered" when we inferred the session from
    /// a transcript owned by a live process. The UI must not present the two identically:
    /// a discovered row can say active or quiet, and can never say "waiting for you".
    QString origin = QStringLiteral("hook");
    /// Checked-out git branch, read from `.git` by the daemon. Empty when there is no repo.
    QString branch;
    /// Token usage the harness stated about the session (`{input_tokens, output_tokens}`),
    /// empty when it never did. Feeds the bottom bar's aggregate, which is the only
    /// place it renders -- no per-row display, matching upstream's bar-not-card shape.
    QVariantMap usage;
    /// True when the status was stated by the harness (turn record / hook), false when
    /// the daemon fell back to a freshness guess. Only the latter is an inference.
    bool status_stated = false;
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
    /// The toolbar's AGENT view: same rows, grouped by tool instead of pure
    /// urgency. Urgency still rules inside each group.
    Q_PROPERTY(bool agentGrouped READ agentGrouped WRITE setAgentGrouped NOTIFY agentGroupedChanged)
    /// "codex 2 · pi 1" for the bottom bar: live sessions per tool.
    Q_PROPERTY(QString sourceSummary READ sourceSummary NOTIFY changed)
    /// "codex 输入 60.8K 输出 0.7K" for the bottom bar: token usage the harnesses
    /// actually stated, aggregated per tool. Empty when nothing stated any -- no
    /// estimated numbers ever appear here.
    Q_PROPERTY(QString usageSummary READ usageSummary NOTIFY changed)

public:
    enum Roles {
        IdRole = Qt::UserRole + 1,
        SourceRole,
        TitleRole,
        StatusRole,
        CwdRole,
        ProjectRole,
        DetailRole,
        OriginRole,
        BranchRole,
        StatedRole,
        ToolCallsRole,
        LastEventMsRole,
        StartedMsRole,
    };

    explicit SessionsModel(QObject *parent = nullptr);

    bool agentGrouped() const { return m_agentGrouped; }
    void setAgentGrouped(bool grouped);

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
    QString sourceSummary() const;
    QString usageSummary() const;

    /// Last path component of the working directory: the card's title.
    static QString projectLabel(const QString &cwd, const QString &fallback);

signals:
    void changed();
    void agentGroupedChanged();

private:
    // Urgency ranks, low first. Mirrors SessionStatus::urgency in Rust; the two
    // must stay in step or the island and the daemon disagree about ordering.
    static int urgency(const QString &status);
    // Fixed per-tool order for the AGENT view.
    static int agentRank(const QString &source);
    void sortRows();

    QVector<SessionRow> m_rows;
    QHash<QString, int> m_indexById;
    bool m_agentGrouped = false;
};

QML_DECLARE_TYPE(SessionsModel)
