# codeisland-linux

把 AI 编码 agent 的状态放进桌面顶部的悬浮胶囊里：一眼看清每个会话在干什么，直接批准工具调用、回答问题，不用切窗口。

这是 [wxtsky/CodeIsland](https://github.com/wxtsky/CodeIsland)（macOS，MIT）的 Linux/KDE 复刻。外观对齐原版，行为对齐原版，但桌面层是重写的——原来的界面是 SwiftUI + AppKit，画在 MacBook 的物理刘海里，在 Linux 上没有对应物。

## 当前进度

| 阶段 | 内容 | 状态 |
|---|---|---|
| 0 | 管道打通：hook → 桥接 → 常驻程序 → 事件日志 | **完成**（真实 claude/haiku 四组用例通过） |
| 1 | 界面：顶部居中悬浮胶囊 | **完成**（`scripts/visual-check.py` 四项像素级断言通过） |
| 2 | 批准闭环：在胶囊上批准/拒绝工具调用 | **完成**（2026-10-13） |
| 3 | 回答问题、跳转终端、桌面通知、hook 自动安装 | 进行中：**hook 自动安装 + 复查完成**（2026-10-13，`codeisland-hookctl`）；**提问卡完成**（2026-10-14，单选问题；契约见 `docs/协议.md`） |
| 4 | 多 CLI 铺开、SSH 远程、推送渠道 | **codex、pi 已接入**（pi 走扩展适配器 `integrations/pi/codeisland.ts`，三例实测：批准生成文件、驳回不生成且无 `PostToolUse`、岛不在场时由 pi 自己决定）；进行中：**codex 已可安装**（2026-10-14：`codeisland-hookctl --source codex` 写项目级 `.codex/hooks.json`，审批闭环实测通过，信任可一次授予并持久化；codex 另有文件夹信任这道前置门，见 `docs/协议.md`） |

阶段 0 没有界面，所以常驻程序**故意不回答**权限请求——它保持沉默，让 agent 回落到自己的权限提示。见下面「安全契约」。

提问卡只回答**已实测确认的形状**：`AskUserQuestion` 以 `PermissionRequest` 到达，回答 = `allow` + `updatedInput`（原 `questions` 保留，加 `answers` 与 `annotations`）。`multiSelect`、文本/数字类问题在上游载荷里尚未观测到，所以 daemon 直接**不 park**——agent 回落到终端自己的提问，而不是被胶囊猜一个答案。验收两条：`cargo test`（含"点错形状必须被拒"的不变量）与真 pty 会话——脚本化 viewer 选 `Blue`，journal 的 `PostToolUse` 里出现 `answers: {"…": "Blue"}`，而提交指令后**没有再按过任何键**。

阶段 2 当初留的那个「前提」是权限回复的报文格式必须和上游一致，已逐字节对照上游源码与已安装的 `claude` 二进制确认（见 `docs/协议.md`）。闭环的验收方式两条：`scripts/permission-probe.py` 跑六项行为断言（含「撤回是静默而不是 deny」「过期点击不生效」），以及截图量像素确认卡片与按钮真的画出来（窗口高度现在从内容自己推，不再手算）。

### 装 hook（阶段 3）

```bash
# 项目级（默认，安全）：写 ./.claude/settings.json
cd 你的项目 && ~/apps/codeisland-linux/target/release/codeisland-hookctl install

# 复查：hook 还在不在、还指向当前桥接程序吗
codeisland-hookctl verify            # 有问题退出码 1，并说明是哪一项

# 全局（~/.claude/settings.json）要显式指定：Claude Code 改配置时会整体重生成
# 这个文件、不保留外部注入的键，所以装完得靠 verify 复查
codeisland-hookctl install --scope user
```

`install` 只补自己的条目：无关键（`model`、`autoMemoryEnabled`、`permissions`…）和用户自己
在同一个事件上的 hook 一律原样保留；改动前留一份带 UTC 时间戳的备份；写完**读回来复查**。

### 桌面通知（阶段 3）

通知走桌面自己的服务（freedesktop `org.freedesktop.Notifications`，本机由 plasmashell 拥有），实现方式是调用 `notify-send` —— 不引入 D-Bus 客户端依赖，也不自己实现协议。措辞分两种：卡片已经显示时是「`<项目>` 需要你 / `<摘要>`」，**没有界面连接时**额外注明「（界面未连接，请在终端里回答）」，因为那种情况下终端是唯一还能回答的地方。

四条硬约束，都带测试：

- **绝不为通知卡住 agent**：发送在独立线程上完成，钩子线程只往一个容量 4 的队列里塞，塞满就丢（迟到的「需要你」比不发更坏）
- **卡死的通知进程会被杀掉**（3 秒）：这台机器上出过一次「通知服务缺席」的事故，症状不是报错而是 `notify-send` 挂 25~60 秒（见 vault `facts/本机/修复/QQ闪退缺通知applet`）
- **同一会话 5 秒内只通知一次**，不同会话互不影响
- 通知**从不改变安全契约**：它只是提醒，agent 等到的仍然是「人给了答案」或者「静默」

通知**自动 5 秒过期**（`--expire-time 5000`），不会堆在角落里等人关掉。环境里没有 `DBUS_SESSION_BUS_ADDRESS` 时（纯 SSH 会话常见），会填上本机用户总线 `unix:path=/run/user/<uid>/bus` 再发。

两个配置项，都是环境变量（项目的配置面清一色如此，不另起配置文件）：

| 变量 | 默认 | 作用 |
|---|---|---|
| `CODEISLAND_NOTIFY_ON_STOP` | 开 | 一轮结束（`Stop`）是否也通知。写成 `0`/`false`/`off`/`no` 才静音；**空值或半填不算静音**，因为那通常是 drop-in 写漏了 |
| `CODEISLAND_NOTIFY_PROGRAM` | `notify-send` | 换掉发送程序，测试用它数通知条数 |

systemd 下改法：`systemctl --user edit codeisland-daemon` 加一行
`Environment=CODEISLAND_NOTIFY_ON_STOP=0`。没有通知服务时发送失败只在日志里留一行，
**胶囊与批准流程完全不受影响**——通知是提醒，能解开 agent 的只有卡片或终端里的回答。

## 怎么跑

```bash
cargo build --release

# 终端 A：常驻程序
./target/release/codeisland-daemon

# 终端 B：模拟一个 hook 事件，观察常驻程序输出
echo '{"hook_event_name":"SessionStart","session_id":"demo"}' \
  | ./target/release/codeisland-bridge --source claude
```

常驻程序会打印一行处理结果，并把事件追加到 `~/.local/state/codeisland/events.jsonl`。

端到端冒烟测试（会真实调用一次 `claude`，消耗额度）：

```bash
scripts/smoke-claude.sh
```

它把 hook 写进**一次性的项目级** `.claude/settings.json`，不动你的全局 `~/.claude/settings.json`。

## 安全契约

这条契约是整套设计的地基，改任何代码都不能破坏它：

> 桥接程序夹在 agent 和它自己的权限提示之间。**只要出任何问题**——常驻程序没跑、连接被拒、超时、输入是坏的、程序 panic——桥接程序必须**退出码 0 且不向 stdout 写任何东西**。然后 agent 就回落到它自己的权限提示。
>
> **绝不自动批准，绝不卡住 agent。**

违反后果不对称：自动批准是安全漏洞，卡住是可用性事故，两者都比"没有这个工具"严重得多。所以超时都是有界的，且桥接程序装了 panic hook，panic 时也走 exit 0。

## 两条链路

```
AI CLI ──触发 hook──▶ codeisland-bridge ──unix socket──▶ codeisland-daemon ──▶ 界面
                        (短命进程)        /tmp/codeisland-<uid>.sock        (阶段 1+)
                            ▲                                                    │
                            └────────── 权限决策原路返回（标准输出）◀─────────────┘
```

报文格式见 [`docs/协议.md`](docs/协议.md)。

## 目录

| 路径 | 作用 |
|---|---|
| `crates/codeisland-core/` | 平台无关层：事件名归一化、线协议、socket 路径 |
| `crates/codeisland-bridge/` | 每次 hook 事件启动一次的短命进程 |
| `crates/codeisland-daemon/` | 常驻：监听 socket、维护会话状态、写事件日志 |
| `shell/` | 顶部胶囊界面（阶段 1 起） |
| `packaging/systemd/user/` | 常驻程序的 systemd 用户服务 |
| `scripts/` | 冒烟测试等运维脚本 |
| `docs/` | 协议、与上游的差异、资源占用实测 |
| `agents/` | 各 CLI 的 hook 配置模板 |

## 与上游的关系

复用上游的**线协议、事件名映射表、hook 语义**；重写**桌面层、进程模型、配置写入器**。逐项对照见 [`docs/与上游的差异.md`](docs/与上游的差异.md)。

上游仓库里已经有一份 Windows 移植（`windows/` 目录，C#/.NET 8 + WPF），遵循的正是同一条原则。

## 许可证

MIT，与上游一致。
