//! codeisland-hookctl -- install and re-check the hooks the island needs.
//!
//! A separate binary on purpose. The bridge is spawned once per event by the CLI,
//! so it must stay small, silent and fast; this tool is run by hand, writes into
//! the user's own configuration, and has to explain what it found.
//!
//! Two behaviours are non-negotiable, both learned the hard way elsewhere:
//!
//!   * **Read back what was written.** `install` re-reads the file from disk and
//!     verifies it, because a settings file that *looks* installed on the way out
//!     and is regenerated an hour later is exactly the failure this tool exists to
//!     catch.
//!   * **Keep a dated backup before changing anything.** The only reason a past
//!     regression -- Claude Code rewriting `settings.json` and dropping every
//!     externally installed hook -- was noticed at all was that an old copy
//!     survived to be diffed against.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs, process};

use codeisland_core::hooks_install;

/// Where the hooks go.
///
/// `project` is the default and the safe one. `user` (the global
/// `~/.claude/settings.json`) is opt-in precisely because Claude Code regenerates
/// that file on configuration changes and does not preserve foreign keys -- so an
/// install there needs this tool to be re-run, or `verify` to be run, after any
/// `/config` edit or upgrade.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Project,
    User,
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(command) = args.first().map(String::as_str) else {
        usage();
    };

    let mut scope = Scope::Project;
    let mut source = "claude".to_string();
    let mut bridge: Option<PathBuf> = None;

    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--scope" => {
                index += 1;
                scope = match args.get(index).map(String::as_str) {
                    Some("project") => Scope::Project,
                    Some("user") => Scope::User,
                    other => fail(&format!("未知的 --scope：{other:?}（可用：project, user）")),
                };
            }
            "--source" => {
                index += 1;
                source = args.get(index).cloned().unwrap_or_else(|| fail("--source 需要一个值"));
            }
            "--bridge" => {
                index += 1;
                bridge = Some(PathBuf::from(
                    args.get(index).cloned().unwrap_or_else(|| fail("--bridge 需要一个路径")),
                ));
            }
            "--help" | "-h" => usage(),
            other => fail(&format!("未知参数：{other}")),
        }
        index += 1;
    }

    let bridge = bridge.unwrap_or_else(default_bridge);
    let path = settings_path(scope, &source);

    if source == "pi" {
        match command {
            "install" => pi_install(scope),
            "verify" => pi_verify(scope),
            "path" => {
                println!("{}", settings_path(scope, &source).display());
                process::exit(0);
            }
            _ => usage(),
        }
    }

    match command {
        "install" => install(&path, &bridge, &source),
        "verify" => verify(&path, &bridge, &source),
        "path" => println!("{}", path.display()),
        other => fail(&format!("未知命令：{other}")),
    }
}

fn usage() -> ! {
    eprintln!(
        "用法: codeisland-hookctl <install|verify|path> [选项]\n\
         \n\
         \x20 install   把本项目需要的 hook 写进配置文件（claude {} 个 / codex {} 个；只补自己的，不碰别人的）\n\
         \x20 verify    检查 hook 是否还在、是否还指向当前桥接程序；有问题退出码 1\n\
         \x20 path      打印将要读写的配置文件路径\n\
         \n\
         配置文件随 --source 变：claude 用 .claude/settings.json，codex 用 .codex/hooks.json。\n\
         两者都有项目级，默认只动项目级；codex 另有一道信任门，install 会把操作步骤打出来。\n\
         \n\
         选项：\n\
         \x20 --scope project|user   项目级 ./.<cli>/…（默认）或全局 ~/.​<cli>/…\n\
         \x20 --bridge <路径>        桥接程序绝对路径（默认与本工具同目录）\n\
         \x20 --source <名字>        传给桥接程序的 --source，默认 claude\n\
         \n\
         --source pi 走的是另一条路：pi 没有 hook 配置文件，它加载 .pi/extensions/ 下的模块。\n\
         \x20 所以 install 是把扩展文件放进项目、verify 是逐字节比对；项目级文件受信任门控，\n\
         \x20 该目录第一次跑 pi 要带 --approve。全局注册请用 pi install，本工具不代写 ~/.pi。",
        hooks_install::HOOK_EVENTS.len(),
        hooks_install::CODEX_EVENTS.len()
    );
    process::exit(2)
}

fn fail(message: &str) -> ! {
    eprintln!("codeisland-hookctl: {message}");
    process::exit(2)
}

/// The bridge next to this executable.
///
/// They are built together and live in the same directory, so "wherever I am, the
/// bridge is beside me" is a better default than a path baked in at build time --
/// a copied `target/` directory would otherwise install hooks pointing at a binary
/// that no longer exists, which fails silently.
fn default_bridge() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("codeisland-bridge")))
        .unwrap_or_else(|| PathBuf::from("codeisland-bridge"))
}

/// Where the CLI actually reads its hooks from, per source and per scope.
///
/// Project scope exists for both CLIs, which is the point: this tool does not touch a
/// user's global CLI config unless asked. codex's project scope is measured, not assumed --
/// with `$CODEX_HOME/hooks.json` deleted, a lone `<project>/.codex/hooks.json` still
/// produced all six events at the daemon.
/// pi does not read a hook file: it loads a module from `.pi/extensions/`. Installing for
/// pi therefore means placing that module, and verifying means comparing bytes -- not
/// merging JSON. The directory itself is chosen in `settings_path`, so only the file name
/// is named twice here and it must stay that way.
const PI_FILE: &str = "codeisland.ts";

/// Where the shipped extension lives, in the order a user would expect it to be found.
///
/// Nothing is baked in at build time: `--bridge` already teaches this tool to prefer
/// "beside me" over "where the build machine put it", and the same reasoning applies here.
fn pi_extension_source() -> PathBuf {
    if let Ok(explicit) = env::var("CODEISLAND_PI_EXTENSION") {
        return PathBuf::from(explicit);
    }
    let exe = env::current_exe().ok();
    let candidates: Vec<PathBuf> = [
        exe.as_deref().and_then(|p| p.parent()).map(|d| d.join("pi-extension").join(PI_FILE)),
        exe.as_deref()
            .and_then(|p| p.parent())
            .and_then(|d| d.parent())
            .and_then(|d| d.parent())
            .map(|root| root.join("integrations/pi").join(PI_FILE)),
    ]
    .into_iter()
    .flatten()
    .collect();
    for candidate in &candidates {
        if candidate.exists() {
            return candidate.clone();
        }
    }
    candidates
        .into_iter()
        .next()
        .unwrap_or_else(|| PathBuf::from(PI_FILE))
}

fn pi_install(scope: Scope) -> ! {
    if matches!(scope, Scope::User) {
        // Global registration is pi's own job, and `pi remove` is the only clean way back
        // out of it (renaming a directory does nothing: the scanner ignores directory
        // names). Writing ~/.pi/agent/settings.json by hand would take that path away.
        fail(
            "pi 的全局注册不由本工具代写：请运行\n\
             \x20 pi install <仓库>/integrations/pi\n\
             \x20 卸载用 pi remove <同一个绝对路径>（相对路径不匹配，必须原样）\n\
             \x20 项目级则不需要 pi install，直接不带 --scope user 安装到 .pi/extensions/",
        );
    }

    let source = pi_extension_source();
    if !source.exists() {
        fail(&format!(
            "找不到要安装的扩展文件：{}\n（用 CODEISLAND_PI_EXTENSION 指定，或先 cargo build --release）",
            source.display()
        ));
    }
    let body = fs::read_to_string(&source).unwrap_or_else(|err| fail(&format!("读不了扩展文件：{err}")));

    let target = settings_path(scope, "pi");
    let existed = target.exists();
    let installed = fs::read_to_string(&target).ok();
    if hooks_install::extension_state(Some(&body), installed.as_deref()) == hooks_install::ExtensionState::Identical {
        println!("已是最新：{}（内容与发布版本一致，未改动）", target.display());
        process::exit(0);
    }

    if existed {
        let backup = backup_path(&target, &timestamp_utc());
        fs::copy(&target, &backup).unwrap_or_else(|err| fail(&format!("备份失败，放弃写入：{err}")));
        println!("已备份原文件 → {}", backup.display());
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).unwrap_or_else(|err| fail(&format!("建不了 {}：{err}", parent.display())));
    }
    fs::write(&target, &body).unwrap_or_else(|err| fail(&format!("写不了 {}：{err}", target.display())));

    // Read back from disk, the same rule as the JSON path: what we intended to write is not
    // evidence that it is there.
    let raw = fs::read_to_string(&target).unwrap_or_else(|err| fail(&format!("写入后读不回：{err}")));
    if raw != body {
        fail("写入后内容与源文件不一致，请检查磁盘");
    }
    println!("已安装扩展 → {}", target.display());
    eprintln!(
        "两条必须知道的生效条件：\n\
         \x20 1) 项目级 .pi/extensions/ 受**信任门控**：在该目录第一次跑 pi 要带 --approve（否则文件被忽略，\n\
         \x20    现象是「装了但一个事件都没有」，实测：不带 --approve 0 事件、带则 SessionStart 起一串）\n\
         \x20 2) 扩展只在启动时加载，当前会话不会立刻生效\n\
         \x20 全局注册请改用 pi install <仓库>/integrations/pi，卸载用 pi remove <同一路径>"
    );
    process::exit(0);
}

fn pi_verify(scope: Scope) -> ! {
    let source = pi_extension_source();
    let target = settings_path(scope, "pi");
    let body = fs::read_to_string(&source).ok();
    let installed = fs::read_to_string(&target).ok();
    match hooks_install::extension_state(body.as_deref(), installed.as_deref()) {
        hooks_install::ExtensionState::Identical => {
            println!("就位：{} 与发布版本一致", target.display());
            process::exit(0);
        }
        hooks_install::ExtensionState::Absent => {
            eprintln!("缺失：{} 不存在（跑 codeisland-hookctl install --source pi）", target.display());
            process::exit(1);
        }
        hooks_install::ExtensionState::Stale => {
            eprintln!(
                "不一致：{} 与 {} 不同（被手改过，或升级后没重装）",
                target.display(),
                source.display()
            );
            process::exit(1);
        }
    }
}

fn settings_path(scope: Scope, source: &str) -> PathBuf {
    let (dir, file) = match source {
        "codex" => (".codex", "hooks.json"),
        "pi" => (".pi/extensions", "codeisland.ts"),
        _ => (".claude", "settings.json"),
    };
    match scope {
        Scope::Project => env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(dir)
            .join(file),
        Scope::User => {
            let home = env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| fail("需要 HOME 才能定位 ~/.claude 或 ~/.codex"));
            home.join(dir).join(file)
        }
    }
}

/// `<原名>.bak-<utc>`, beside the original and keeping the original's name.
///
/// The backup name used to be hardcoded to `settings.json.bak-*`, which for codex wrote a
/// file called settings.json into `.codex/` -- a stray file with a misleading name in the
/// directory we are about to be told to trust.
fn backup_path(path: &PathBuf, stamp: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "hooks.json".to_string());
    path.with_file_name(format!("{name}.bak-{stamp}"))
}

/// codex gates hooks on a per-command trust hash that no CLI flag writes on the user's
/// behalf. Silence about it would look like a broken install.
fn trust_note(source: &str, existed_before: bool) {
    if source != "codex" {
        return;
    }
    let verb = if existed_before {
        "本次是往已有配置里写；只要 command 字符串变过，trusted_hash 就失效，提示会重新出现"
    } else {
        "第一次安装，必然要过一次信任门"
    };
    eprintln!(
        "codex 还有一道信任门（{verb}）：\n\
         \x20 1) 在该项目里运行 codex，会弹「Hooks need review」\n\
         \x20 2) 默认光标停在「1. Review hooks」，必须先按 ↓ 再回车，选「2. Trust all and continue」\n\
         \x20 3) 此后信任记在 ~/.codex/config.toml 的 [hooks.state.\"<文件>:<事件>:0:0\"] trusted_hash，不再提示\n\
         \x20 未信任之前，codex 不会执行这些 hook，本工具的 verify 也看不出差别——它只检查配置在不在。\n\
         \n\
         \x20 还有一道更早的门：第一次进入某个项目目录，codex 会问是否信任该文件夹。\n\
         \x20 那一句没答完，会话根本不建立，hook 一个事件都不会发。同一份配置实测：\n\
         \x20 已信任目录 5 个事件、新目录 0 个事件——所以别把这两件事混成一个 bug。"
    );
}

fn install(path: &PathBuf, bridge: &PathBuf, source: &str) -> ! {
    if !bridge.exists() {
        fail(&format!("桥接程序不存在：{}\n（先 cargo build --release，或用 --bridge 指定）", bridge.display()));
    }

    let existed = path.exists();
    let raw = if existed {
        fs::read_to_string(path).unwrap_or_else(|err| fail(&format!("读不了 {}：{err}", path.display())))
    } else {
        String::from("{}")
    };

    let mut settings = match hooks_install::parse(&raw) {
        Ok(settings) => settings,
        Err(err) => fail(&format!("{}：{err}", path.display())),
    };

    let report = hooks_install::install(&mut settings, bridge, source);

    if !report.changed() {
        println!(
            "已是最新：{}（{} 个 hook 全部就位，未改动文件）",
            path.display(),
            report.unchanged
        );
        if !report.skipped.is_empty() {
            eprintln!("注意：这些条目形状不认识，已原样跳过：{}", report.skipped.join(", "));
        }
        // Still verify: "no changes" and "correct" are different claims.
        trust_note(source, false);
        process::exit(finish_verify(path, bridge, source, true));
    }

    // Dated backup before the first write, and only before a real change.
    if existed {
        let backup = backup_path(path, &timestamp_utc());
        if let Err(err) = fs::copy(path, &backup) {
            fail(&format!("备份失败，放弃写入：{err}"));
        }
        println!("已备份原文件 → {}", backup.display());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap_or_else(|err| fail(&format!("建不了 {}：{err}", parent.display())));
    }
    let body = serde_json::to_string_pretty(&settings).unwrap_or_else(|err| fail(&format!("序列化失败：{err}")));
    fs::write(path, format!("{body}\n")).unwrap_or_else(|err| fail(&format!("写不了 {}：{err}", path.display())));

    println!(
        "已写入 {}：新增 {}，修复 {}，保持不动 {}",
        path.display(),
        report.added.len(),
        report.refreshed.len(),
        report.unchanged
    );
    if !report.skipped.is_empty() {
        eprintln!("注意：这些条目形状不认识，已原样跳过：{}", report.skipped.join(", "));
    }

    trust_note(source, existed);
    process::exit(finish_verify(path, bridge, source, false));
}

/// Re-read from disk and check. This is the whole point of `install`.
fn finish_verify(path: &PathBuf, bridge: &PathBuf, source: &str, quiet_ok: bool) -> i32 {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) => {
            eprintln!("复查失败：读不回 {}：{err}", path.display());
            return 1;
        }
    };
    let settings = match hooks_install::parse(&raw) {
        Ok(settings) => settings,
        Err(err) => {
            eprintln!("复查失败：写完的文件读回来不是合法配置：{err}");
            return 1;
        }
    };

    match verify_against(&settings, bridge, source) {
        0 => {
            if !quiet_ok {
                // 数字必须来自本次 source 的事件表。这里曾经硬编码 claude 的 7，于是给 codex 报了一句
                // "7 个 hook 都在"而文件里只有 6 个 —— 校验器报一个和磁盘不符的数，比不报更坏。
                println!(
                    "复查通过：{} 个 hook 都在，且指向 {}",
                    hooks_install::events_for(source).len(),
                    bridge.display()
                );
            }
            0
        }
        code => code,
    }
}

fn verify_against(settings: &serde_json::Value, bridge: &PathBuf, source: &str) -> i32 {
    let problems = hooks_install::verify(settings, bridge, source);
    if problems.is_empty() {
        return 0;
    }
    eprintln!("发现 {} 个问题：", problems.len());
    for problem in &problems {
        eprintln!("  ✗ {problem}");
    }
    eprintln!("修复：codeisland-hookctl install（同参数）");
    1
}

fn verify(path: &PathBuf, bridge: &PathBuf, source: &str) -> ! {
    if !path.exists() {
        eprintln!("{} 不存在 —— 也就是没有任何 hook", path.display());
        process::exit(1);
    }
    let raw = fs::read_to_string(path).unwrap_or_else(|err| fail(&format!("读不了 {}：{err}", path.display())));
    let settings = match hooks_install::parse(&raw) {
        Ok(settings) => settings,
        Err(err) => fail(&format!("{}：{err}", path.display())),
    };
    process::exit(verify_against(&settings, bridge, source))
}

/// `YYYYMMDD-HHMMSS` in UTC, without pulling in a date crate for one filename.
fn timestamp_utc() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rest) = (secs / 86_400, secs % 86_400);
    let (hour, minute, second) = (rest / 3600, (rest % 3600) / 60, rest % 60);

    // Days since 1970-01-01 -> civil date (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}{m:02}{d:02}-{hour:02}{minute:02}{second:02}")
}
