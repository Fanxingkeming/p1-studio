//! 这台电脑能不能跑这个程序 —— 一条命令说清楚。
//!
//! 成品是一个单文件 exe，但"单文件"不等于"零依赖"：界面由微软的 WebView2
//! 运行时绘制，运行时由系统提供，不在包里。缺了它窗口根本起不来，而失败
//! 方式是闪一下就没了或者干脆什么都不发生，看不出原因。在别人的机器上
//! 排查这种事，第一分钟都花在猜上。
//!
//! 所以这里把"这台机器缺什么"逐项列出来，`--check` 打印它，建窗口之前也
//! 先看一眼。

use std::path::{Path, PathBuf};

use crate::protocol::VENDOR_ID;

/// WebView2 Evergreen 运行时的版本号，没装就是 `None`。
///
/// 三个位置都看：机器级（`Program Files (x86)` 和 `Program Files`），以及
/// 用户级（`LOCALAPPDATA` —— 没有管理员权限时 EdgeUpdate 装在这里）。
pub fn webview2_version() -> Option<String> {
    let mut best: Option<(u32, u32, u32, u32, String)> = None;
    for key in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
        let Ok(base) = std::env::var(key) else { continue };
        let app = Path::new(&base)
            .join("Microsoft")
            .join("EdgeWebView")
            .join("Application");
        let Ok(entries) = std::fs::read_dir(&app) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(v) = parse_version(&name) else {
                continue;
            };
            // 版本号命名的目录也可能是升级残留的空壳。真正的运行时一定带
            // 着 msedgewebview2.exe，认空壳会得出"装了却起不来"的结论。
            if !entry.path().join("msedgewebview2.exe").exists() {
                continue;
            }
            if best.as_ref().map_or(true, |b| v > (b.0, b.1, b.2, b.3)) {
                best = Some((v.0, v.1, v.2, v.3, name));
            }
        }
    }
    best.map(|(_, _, _, _, name)| name)
}

/// `156.0.4314.56` → 四个数字，用来挑出最高的那个版本。
fn parse_version(s: &str) -> Option<(u32, u32, u32, u32)> {
    let mut parts = s.split('.');
    let a = parts.next()?.parse().ok()?;
    let b = parts.next()?.parse().ok()?;
    let c = parts.next()?.parse().ok()?;
    let d = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    Some((a, b, c, d))
}

fn system_dir() -> PathBuf {
    match std::env::var("SystemRoot") {
        Ok(root) => Path::new(&root).join("System32"),
        Err(_) => PathBuf::from(r"C:\Windows\System32"),
    }
}

/// 通用 C 运行时的两个形态：Win10 以后是两个都有的，Win7/8.1 装了
/// KB2999226 之后才有 api-ms-win-crt-*。主程序链接的是后者。
fn ucrt_present() -> bool {
    let d = system_dir();
    d.join("ucrtbase.dll").exists() || d.join("api-ms-win-crt-runtime-l1-1-0.dll").exists()
}

fn vc_runtime_present() -> bool {
    let d = system_dir();
    d.join("msvcp140.dll").exists() && d.join("vcruntime140.dll").exists()
}

/// 建目录（不存在是正常的 —— 首次运行就是这样）再试着写进去，最后擦掉
/// 痕迹。只 stat 目录是不能说明能不能写的。
fn writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join("metaphyuni-hub-write-test.tmp");
    std::fs::write(&probe, b"x").is_ok() && std::fs::remove_file(&probe).is_ok()
}

/// 枚举本机上属于玄派的 USB 设备。没有设备不算错误 —— 这个检查的目的是
/// 区分"程序跑不了"和"鼠标没插"。
fn vendor_devices() -> Result<Vec<String>, String> {
    let api = hidapi::HidApi::new().map_err(|e| format!("hidapi 初始化失败：{e}"))?;
    let mut found: Vec<String> = api
        .device_list()
        .filter(|d| d.vendor_id() == VENDOR_ID)
        .map(|d| format!("pid 0x{:04x}", d.product_id()))
        .collect();
    found.sort();
    found.dedup();
    Ok(found)
}

/// 把固件升级用的那套 32 位组件释放出来，让辅助进程自己报能不能加载。
fn upgrade_engine() -> Result<String, String> {
    let dir = crate::stage_upgrade_env()?;
    let mut engines = 0usize;
    let code = crate::run_helper(&dir, "load", Path::new(""), None, |line| {
        if line.contains("\"ev\":\"loaded\"") {
            if let Some(v) = line.rsplit("\"engines\":").next() {
                engines = v.trim_end_matches(['}', '"']).parse().unwrap_or(0);
            }
        }
    })?;
    if code != 0 || engines == 0 {
        return Err("辅助进程自检未通过".to_string());
    }
    Ok(format!("加载正常（{engines} 个入口）"))
}

/// 一行一项的完整报告。`--check` 打印它。
pub fn report() -> String {
    let mut out = String::new();
    let mut required_ok = true;

    macro_rules! row {
        ($tag:expr, $label:expr, $detail:expr) => {{
            // 标签是中文，按字符数对齐会错开一格：一个汉字占两列。
            out.push_str(&format!(
                "  {} {} {}\n",
                $tag,
                pad_right($label, 16),
                $detail
            ));
        }};
    }

    out.push_str("Metaphyuni玄熊猫P1管理器 —— 运行环境自检\n");
    out.push_str(&format!(
        "程序架构：{}（32 位 Windows 无法运行本程序）\n",
        std::env::consts::ARCH
    ));
    out.push('\n');

    // ── 必需项 ──────────────────────────────────────────────────────────
    let arch = std::env::var("PROCESSOR_ARCHITECTURE").unwrap_or_else(|_| "未知".into());
    row!("[必需]", "操作系统架构", arch);

    let ucrt = ucrt_present();
    required_ok &= ucrt;
    row!(
        "[必需]",
        "通用 C 运行时",
        if ucrt {
            "已安装".to_string()
        } else {
            "缺失 —— Windows 7/8.1 需要安装 KB2999226".to_string()
        }
    );

    match webview2_version() {
        Some(v) => row!("[必需]", "WebView2 运行时", format!("{v}")),
        None => {
            required_ok = false;
            row!("[必需]", "WebView2 运行时", "缺失 —— 界面无法启动");
        }
    }

    let appdata = dirs_appdata();
    let w = writable(&appdata);
    required_ok &= w;
    row!(
        "[必需]",
        "设置目录可写",
        with_mark(w, appdata.display().to_string())
    );

    let temp = std::env::temp_dir();
    let w = writable(&temp);
    required_ok &= w;
    row!(
        "[必需]",
        "临时目录可写",
        with_mark(w, temp.display().to_string())
    );

    // ── 可选 / 设备 ─────────────────────────────────────────────────────
    out.push('\n');
    row!(
        "[可选]",
        "系统 VC++ 运行库",
        if vc_runtime_present() {
            "已安装".to_string()
        } else {
            "未装 —— 不影响，升级组件自带所需文件".to_string()
        }
    );

    match vendor_devices() {
        Ok(list) if list.is_empty() => row!("[设备]", "玄派接收器", "未发现（鼠标未接入或已休眠）"),
        Ok(list) => row!("[设备]", "玄派接收器", list.join("、")),
        Err(e) => row!("[设备]", "玄派接收器", e),
    }

    match upgrade_engine() {
        Ok(msg) => row!("[可选]", "固件升级引擎", msg),
        Err(e) => row!("[可选]", "固件升级引擎", format!("{e}（仅影响固件升级）")),
    }

    out.push('\n');
    if required_ok {
        out.push_str("结论：这台电脑可以运行本程序。\n");
    } else {
        out.push_str("结论：缺少必需组件，标记「缺失」的项需要先补上。\n");
        if webview2_version().is_none() {
            out.push_str("      WebView2 下载地址：https://go.microsoft.com/fwlink/p/?LinkId=2124703\n");
        }
    }
    out
}

fn with_mark(ok: bool, detail: String) -> String {
    if ok {
        detail
    } else {
        format!("不可写 —— {detail}")
    }
}

/// 终端里的显示宽度：中日韩字符占两列，`format!("{:<16}")` 按字符数算，
/// 中文标签会被推得参差不齐。
fn pad_right(s: &str, width: usize) -> String {
    let shown: usize = s
        .chars()
        .map(|c| if c as u32 > 0x2e80 { 2 } else { 1 })
        .sum();
    if shown >= width {
        return s.to_string();
    }
    format!("{s}{}", " ".repeat(width - shown))
}

/// 设置文件所在的目录，与 `save_settings` 用的是同一个。
fn dirs_appdata() -> PathBuf {
    std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("cn.metaphyuni.hub")
}
