# layer-shell 在 KDE 上的实测结论（阶段 1 的地基）

这组实验回答一个问题：**KWin 6.7.5 会不会在 layer-shell 表面后面做毛玻璃模糊？**
答案直接决定界面要不要自己画模糊，所以要测准，不能靠猜。

结论：**不会模糊，但要做对两件事，否则测出来的是假答案。** 我在这份实验里
两次得到假答案，都记在下面，免得以后重犯。

## 三条已验证的事实

1. **KWin 6.7.5 不在 layer-shell 表面后面做 compositor 模糊。**
   判据：胶囊区域内的亮度标准差正好是底下桌面的**一半**（普通窗口 8.7 vs 8.7，
   layer-shell 16.8 vs 16.9）。纯 alpha 叠加就是乘 `(1-alpha)=0.5`；真模糊会把
   细节摧毁得远多于此。所以**没有模糊，只有 alpha 混合**。

2. **必须显式申请 alpha 缓冲区，否则窗口根本没有透明通道。**
   `qmlscene6` 跑的窗口没有 alpha 通道：`#80ff0000`（50% 红）渲染成 `(255,0,0)`
   完全不透明。用 C++ 启动器在建窗前设
   `QSurfaceFormat::setDefaultFormat(fmt)` 且 `fmt.setAlphaBufferSize(8)`，
   实测 G 通道 16.3，透明假设预测 16.5，不透明黑底预测 0.0 → 透明成立。
   这条对**普通窗口和 layer-shell 窗口一样成立**，不是 layer-shell 的特殊限制。

3. **Qt 自己的混合是正确的。** 反证：在不透明的绿色窗口里放 50% 红矩形，
   实测 `(128, 127, 0)`，理论值 `(127.5, 127.5, 0)`。

## `org.kde.layershell` 的四个用法坑（都实测过）

| 坑 | 现象 | 正确做法 |
|---|---|---|
| 附加类型和 `QtQuick.Window` 的 `Window` **重名** | 不加限定就 import，根对象变成那个 `isCreatable: false` 的附加类型，报 `Type cannot be created in QML` | `import org.kde.layershell as LS` |
| 附加属性写法 | `Window.layer` 报 `Cannot assign to non-existent property "layer"` | 写成 `LS.Window.layer: LS.Window.LayerOverlay` |
| `margins` 是 `QMargins` | 写成 `QtMargins { ... }` 会报 `QtMargins is not a type` | **可以直接赋 JS 对象**：`LS.Window.margins: ({left:600, top:40, right:600, bottom:0})`。实测表面被精确移到逻辑坐标 x=600,y=40。（本文件此前记的"不能从 QML 赋值"是错的，只试了 `QtMargins {}` 一种写法就下了结论） |
| `qml6` 运行时不报 QML 加载错误 | 任何错误都只有一句 `Did not load any objects, exiting.` | 用 `qmlscene6`（它会打印 `file:line:col` 真实错误），或用 C++ 启动器 |

另外：`import QtQuick, QtQuick.Window` 这种逗号写法 QML **不接受**，必须一行一个 import。

## 为什么这个结论让阶段 1 变简单

原版 CodeIsland 的刘海是**纯黑**的（macOS 物理刘海本来就是黑的），它的 Windows
移植在自己的 README 里也写着 "designed around a pure-black, pixel-style Windows
island"。所以：

- **不需要模糊**就能对齐原版外观；
- 在 Wayland 上做不了 compositor 模糊这件事，**不构成障碍**；
- 只需要 alpha 通道来画**圆角**——而这已被事实 2 解决。

顺带说明：KDE 面板的毛玻璃不是 compositor 给的，Plasma 自己画。想给岛加模糊，
可行的路是 KWin 窗口规则（`~/.config/kwinrulesrc` 里按 window role 匹配并强制
模糊），但那是可选美化，不是必需，且会改动用户的桌面配置，所以**现在不动**。

## 两次假答案，以及它们错在哪

1. **第一次**：比较"有窗口"和"无窗口"两张截图来定位胶囊。结果变更区域是
   整块屏幕（6383×1793），撤掉窗口后仍有 605324 像素与基线不同——因为桌面在动
   （面板时钟在走、agent 自己的终端在输出）。差分法在有活动的桌面上不成立。

2. **第二次**：改用静态棋盘格 + 单张截图，得到 `比例 0.001` 并判成"有模糊"。
   错因是**窗口没有 alpha 通道**（上面事实 2），棋盘格根本没被叠在下面：
   实测"胶囊外"亮度均值 35.9，而真棋盘格应是 127；"胶囊内"是 127.0/std=0.0，
   即不透明的纯灰。量的是一块灰盒子压在黑底上，跟模糊无关。

**教训**：判据必须能自我否证。最终版本先验证"背景层真的画出来了"
（棋盘格 std 应≈127）、再验证"窗口真的透明"（G 通道对比透明/不透明两个预测值），
最后才读模糊比例。

## 复现方法

```bash
# 生成确定性棋盘格（seed 1234，4px 黑白格）
uv run --with pillow --python 3.12 -- python3 gen_noise.py

# 编译需要 alpha 缓冲区的启动器
g++ -fPIC -std=c++17 $(pkg-config --cflags Qt6Quick Qt6Gui Qt6Core) \
    -I$(qmake6 -query QT_INSTALL_HEADERS) \
    alpha-probe.cpp -o alpha-probe $(pkg-config --libs Qt6Quick Qt6Gui Qt6Core)

# 完整模糊实验（单张截图，不依赖 before/after 差分）
WAYLAND_DISPLAY=wayland-0 timeout 300 uv run --with pillow --with numpy \
  --python 3.12 -- python3 measure_blur.py
```

实验产物（截图与裁剪）默认落在 `/tmp/ci-blur2/measure/`。

## 文件

| 文件 | 作用 |
|---|---|
| `alpha-probe.cpp` | 申请 alpha 缓冲区的 Qt Quick 启动器；正式界面的启动器同源 |
| `gen_noise.py` | 生成 4px 黑白棋盘格（seed 1234，可复现） |
| `backdrop.qml` | 最底层的棋盘格带，用来提供高频细节 |
| `pill.qml` | 叠在上面的 50% 灰胶囊，洋红描边用于自定位 |
| `measure_blur.py` | 完整实验：单次截图、自定位、自验证、再读比例 |
| `translucent.qml` / `translucent-ls.qml` | 透明度对照实验（普通窗口 vs layer-shell） |
