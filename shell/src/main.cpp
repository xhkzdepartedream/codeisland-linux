// codeisland-shell: the island itself.
//
// Owns no agent state. It connects to the daemon's view socket, renders whatever
// arrives, and goes empty if that connection drops. Permission decisions are not
// implemented yet, so this process cannot approve anything -- see the phase plan.

#include "daemon_client.h"
#include "hotkey.h"
#include "permission_model.h"
#include "sessions_model.h"
#include "surface.h"

#include <QCommandLineParser>
#include <QLockFile>
#include <QCoreApplication>
#include <QGuiApplication>
#include <QQmlApplicationEngine>
#include <QQmlContext>
#include <QQuickWindow>
#include <QScreen>
#include <QStandardPaths>
#include <QTimer>
#include <QDir>
#include <QUrl>

/// Put the island on a chosen screen.
///
/// This machine has two screens with *different* scale factors (eDP-1 at 2.0,
/// DP-1 at 1.0), so "the top centre" is not one well-defined place: the pill's
/// pixel size and its centre both depend on which screen hosts it. Choosing the
/// screen also makes the visual check deterministic, because it knows which
/// columns of the capture to look at.
static bool selectScreen(QQuickWindow *window, const QString &requested)
{
    const auto screens = QGuiApplication::screens();
    QScreen *target = nullptr;

    if (requested.isEmpty()) {
        target = QGuiApplication::primaryScreen();
    } else {
        bool isNumber = false;
        const int index = requested.toInt(&isNumber);
        if (isNumber && index >= 0 && index < screens.size()) {
            target = screens.at(index);
        } else {
            for (QScreen *screen : screens) {
                if (screen->name().compare(requested, Qt::CaseInsensitive) == 0) {
                    target = screen;
                    break;
                }
            }
        }
    }

    if (!target) {
        qWarning().noquote()
            << "codeisland-shell: unknown screen" << requested
            << "-- available:";
        for (const QScreen *screen : screens) {
            qWarning().noquote() << "  " << screen->name()
                                 << screen->geometry();
        }
        return false;
    }

    window->setScreen(target);
    // Position only, never size. Setting the whole screen geometry here turned a
    // 260x34 pill into a 1536x960 transparent surface that ate every click on the
    // laptop screen; QML sizes the window to the pill and centres it with margins.
    window->setX(target->geometry().x());
    window->setY(target->geometry().y());
    return true;
}

int main(int argc, char *argv[])
{
    requestTranslucentSurface();

    // Software rendering is the default here, and it is a measured choice rather
    // than a guess. On this machine (eDP-1 at dpr 2.0), five live sessions:
    //   GL backend        RSS 174.3 MB,  15.8% of a core while animating
    //   software backend  RSS  98.7 MB,  17.0% of a core while animating
    // So the cheap backend costs about one point of CPU and saves 75 MB. Qt
    // Quick's own floor is large either way -- a trivial one-rectangle window
    // measured 149.5 MB under GL -- so this is about the floor, not about my code.
    //
    // Override by exporting QT_QUICK_BACKEND=before launching, which is honoured
    // because qputenv only fills the slot when it is still empty.
    if (qEnvironmentVariableIsEmpty("QT_QUICK_BACKEND")) {
        qputenv("QT_QUICK_BACKEND", "software");
    }

    QGuiApplication application(argc, argv);
    QGuiApplication::setApplicationName(QStringLiteral("codeisland-shell"));
    QGuiApplication::setOrganizationName(QStringLiteral("codeisland-linux"));
    QGuiApplication::setApplicationVersion(QStringLiteral("0.1.0"));

    QCommandLineParser parser;
    parser.setApplicationDescription(
        QStringLiteral("Top-centre island showing live AI coding agent sessions."));
    parser.addHelpOption();
    parser.addVersionOption();
    const QCommandLineOption listOption(
        QStringLiteral("list-screens"),
        QStringLiteral("Print available screens as name/x/y/width/height/dpr and exit."));
    parser.addOption(listOption);
    const QCommandLineOption screenOption(
        {QStringLiteral("s"), QStringLiteral("screen")},
        QStringLiteral("Screen name or index to host the island (default: primary)."),
        QStringLiteral("screen"));
    parser.addOption(screenOption);
    parser.process(application);

    if (parser.isSet(listOption)) {
        for (const QScreen *screen : QGuiApplication::screens()) {
            const QRect g = screen->geometry();
            // One line per screen, machine-readable: the visual check targets a
            // screen by name rather than assuming where the island landed.
            printf("%s %d %d %d %d %.3f\n", screen->name().toLocal8Bit().constData(),
                   g.x(), g.y(), g.width(), g.height(), screen->devicePixelRatio());
        }
        return 0;
    }

    // One island per machine. Test harnesses used to leave several alive at
    // once, which made "click the button" meaningless -- the capsule on screen
    // was not necessarily the one under test, and each one covered the last.
    // The lock is held by a QFile-like object that releases it when the process
    // exits, including on a crash.
    QLockFile singleInstanceLock(
        QDir(QStandardPaths::writableLocation(QStandardPaths::TempLocation))
            .filePath(QStringLiteral("codeisland-shell.lock")));
    // Default stale time (30 s) is what we want: a shell that was killed keeps
    // its lock briefly so a racing harness cannot stack a second island on top,
    // but a machine that really did crash recovers on its own.
    if (!singleInstanceLock.tryLock(100)) {
        qint64 ownerPid = 0;
        QString hostname, appname;
        singleInstanceLock.getLockInfo(&ownerPid, &hostname, &appname);
        qWarning().noquote()
            << "codeisland-shell: another island is already running (pid"
            << ownerPid << "on" << hostname << ") -- refusing to start a second one";
        return 4;
    }

    QQmlApplicationEngine engine;

    SessionsModel model;
    PermissionModel permissions;
    DaemonClient client(&model, &permissions);
    Hotkey hotkey;
    engine.rootContext()->setContextProperty(QStringLiteral("sessions"), &model);
    engine.rootContext()->setContextProperty(QStringLiteral("permissions"), &permissions);
    engine.rootContext()->setContextProperty(QStringLiteral("daemon"), &client);
    engine.rootContext()->setContextProperty(QStringLiteral("hotkey"), &hotkey);

    const QUrl url = QUrl::fromLocalFile(
        QStringLiteral(CODEISLAND_QML_DIR "/main.qml"));
    engine.load(url);
    if (engine.rootObjects().isEmpty()) {
        return 2;
    }

    auto *window = qobject_cast<QQuickWindow *>(engine.rootObjects().first());
    if (window) {
        if (!selectScreen(window, parser.value(screenOption))) {
            return 3;
        }
        // Log where the surface actually landed. Coordinates for clicking the
        // buttons are derived from this, not guessed from a screenshot region --
        // guessing is what sent a run of clicks to the wrong screen.
        qInfo().noquote() << QStringLiteral("codeisland-shell: screen=%1 geometry=%2,%3 %4x%5 dpr=%6")
                                 .arg(window->screen() ? window->screen()->name() : QStringLiteral("?"))
                                 .arg(window->geometry().x())
                                 .arg(window->geometry().y())
                                 .arg(window->geometry().width())
                                 .arg(window->geometry().height())
                                 .arg(window->screen() ? window->screen()->devicePixelRatio() : 1.0);
    }

    client.start();

    // Global hotkeys: best-effort, never fatal. Without a KDE session (no
    // kglobalacceld on the bus) the island simply has no shortcuts.
    hotkey.install();

    // Re-report the geometry after the first layout pass, because the QML margins
    // binding only settles once the window has been configured by the compositor.
    QTimer::singleShot(700, window, [window] {
        qInfo().noquote() << QStringLiteral("codeisland-shell: after layout geometry=%1,%2 %3x%4")
                                 .arg(window->geometry().x())
                                 .arg(window->geometry().y())
                                 .arg(window->geometry().width())
                                 .arg(window->geometry().height());
    });

    return application.exec();
}
