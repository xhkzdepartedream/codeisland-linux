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

use codeisland_core::hooks_install::{self, HOOK_EVENTS};

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
    let path = settings_path(scope);

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
         \x20 install   把本项目的 {} 个 hook 写进 settings.json（只补自己的，不碰别人的）\n\
         \x20 verify    检查 hook 是否还在、是否还指向当前桥接程序；有问题退出码 1\n\
         \x20 path      打印将要读写的 settings.json 路径\n\
         \n\
         选项：\n\
         \x20 --scope project|user   项目级 ./.claude/settings.json（默认）或全局 ~/.claude/settings.json\n\
         \x20 --bridge <路径>        桥接程序绝对路径（默认与本工具同目录）\n\
         \x20 --source <名字>        传给桥接程序的 --source，默认 claude",
        HOOK_EVENTS.len()
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

fn settings_path(scope: Scope) -> PathBuf {
    match scope {
        Scope::Project => env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(".claude").join("settings.json"),
        Scope::User => {
            let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| fail("需要 HOME 才能定位 ~/.claude"));
            home.join(".claude").join("settings.json")
        }
    }
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
        process::exit(finish_verify(path, bridge, source, true));
    }

    // Dated backup before the first write, and only before a real change.
    if existed {
        let backup = path.with_file_name(format!("settings.json.bak-{}", timestamp_utc()));
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
                println!("复查通过：{} 个 hook 都在，且指向 {}", HOOK_EVENTS.len(), bridge.display());
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
