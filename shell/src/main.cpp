// codeisland-shell: the island itself.
//
// Owns no agent state. It connects to the daemon's view socket, renders whatever
// arrives, and goes empty if that connection drops. Permission decisions are not
// implemented yet, so this process cannot approve anything -- see the phase plan.

#include "daemon_client.h"
#include "sessions_model.h"
#include "surface.h"

#include <QCommandLineParser>
#include <QCoreApplication>
#include <QGuiApplication>
#include <QQmlApplicationEngine>
#include <QQmlContext>
#include <QQuickWindow>
#include <QScreen>
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
    // Move first, then let QML size it: an anchored layer-shell surface is placed
    // by its screen, so the origin has to be that screen's origin.
    window->setGeometry(target->geometry());
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

    QQmlApplicationEngine engine;

    SessionsModel model;
    DaemonClient client(&model);
    engine.rootContext()->setContextProperty(QStringLiteral("sessions"), &model);
    engine.rootContext()->setContextProperty(QStringLiteral("daemon"), &client);

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
    }

    client.start();
    return application.exec();
}
