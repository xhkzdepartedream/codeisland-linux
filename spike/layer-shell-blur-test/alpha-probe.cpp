// Minimal Qt Quick launcher that actually gives the window an alpha buffer.
//
// Why this file exists: qmlscene6 produced windows with NO per-pixel
// transparency. Measured evidence, on this machine (KWin 6.7.5, Wayland):
//   - a Rectangle with colour #80ff0000 inside an OPAQUE window rendered as
//     (128, 127, 0) against the green background, matching the (127.5, 127.5, 0)
//     that blending predicts. So Qt's blending is correct.
//   - the same colour in a window with colour "transparent" rendered as
//     (255, 0, 0) over an opaque black fill, where letting the desktop through
//     predicts G ~ 24.7 and ~ 10.9; measured G was 0.0. So the surface carried
//     no alpha channel at all.
// Requesting alphaBufferSize(8) on the default surface format before the first
// window is created is the documented way to fix that, and it is also what the
// real shell will need, so the shell is built this way from the start rather
// than discovered late.
//
// Usage: alpha-probe <file.qml>

#include <QFileInfo>
#include <QGuiApplication>
#include <QQmlApplicationEngine>
#include <QQmlContext>
#include <QSurfaceFormat>
#include <QUrl>

int main(int argc, char *argv[])
{
    QSurfaceFormat format = QSurfaceFormat::defaultFormat();
    format.setAlphaBufferSize(8);
    QSurfaceFormat::setDefaultFormat(format);

    QGuiApplication application(argc, argv);
    QGuiApplication::setApplicationName(QStringLiteral("codeisland-blur-probe"));

    QQmlApplicationEngine engine;
    if (argc > 1) {
        const QFileInfo info(QString::fromLocal8Bit(argv[1]));
        engine.rootContext()->setContextProperty(QStringLiteral("probePath"),
                                                 info.absoluteFilePath());
        engine.load(QUrl::fromLocalFile(info.absoluteFilePath()));
    } else {
        engine.load(QUrl(QStringLiteral("qrc:///main.qml")));
    }

    if (engine.rootObjects().isEmpty()) {
        return 2;
    }
    return application.exec();
}
