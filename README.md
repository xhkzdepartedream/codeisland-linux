# codeisland-linux

把 AI 编码 agent 的状态放进桌面顶部的悬浮胶囊里：一眼看清每个会话在干什么，直接批准工具调用、回答问题，不用切窗口。

这是 [wxtsky/CodeIsland](https://github.com/wxtsky/CodeIsland)（macOS，MIT）的 Linux/KDE 复刻。外观对齐原版，行为对齐原版，但桌面层是重写的——原来的界面是 SwiftUI + AppKit，画在 MacBook 的物理刘海里，在 Linux 上没有对应物。

## 当前进度

| 阶段 | 内容 | 状态 |
|---|---|---|
| 0 | 管道打通：hook → 桥接 → 常驻程序 → 事件日志 | **完成** |
| 1 | 界面：顶部居中悬浮胶囊 | 未开始 |
| 2 | 批准闭环：在胶囊上批准/拒绝工具调用 | 未开始 |
| 3 | 回答问题、跳转终端、桌面通知、hook 自动安装 | 未开始 |
| 4 | 多 CLI 铺开、SSH 远程、推送渠道 | 未开始 |

阶段 0 没有界面，所以常驻程序**故意不回答**权限请求——它保持沉默，让 agent 回落到自己的权限提示。见下面「安全契约」。

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
