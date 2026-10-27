//! Reimu Player 产品入口（`--features gui` 编译）。
//!
//! System 6 / Y2K Tape Deck 风格音乐播放器。视觉参考 `code (3).html`，
//! 窗口边框/标题栏为应用自绘（非系统装饰）。
//!
//! 用法：
//! ```text
//! cargo run -p reimuplayer --features gui --bin reimu-player
//! ```
//!
//! 可选参数：
//! - `--search-timeout-ms <ms>` 覆盖搜索超时；
//! - `--library <dir>` 启动即扫描本地曲库目录。
//!
//! **release 构建不带控制台窗口**：`windows_subsystem = "windows"` 让双击
//! 启动时只有应用窗口，不会弹出黑色命令行框（用户明确要求——普通用户用不到
//! 命令行，看到它只会以为是程序出错）。debug 构建**保留**控制台，这样
//! `tracing` 日志仍能在终端里看，排查问题不受影响（日志写 stderr）。
//!
//! 图标分两路：exe 里的多尺寸 ICO 由 `build.rs` 写入（资源管理器 / 桌面 /
//! 快捷方式 / 开始菜单 / 任务栏读取），任务栏身份由 [`set_app_id`] 固定。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::time::Duration;

use reimuplayer::providers::ProviderTimeouts;

fn main() -> eframe::Result {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let mut timeouts = ProviderTimeouts::default();
    let mut library_dir: Option<String> = None;
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--search-timeout-ms" => {
                if let Some(v) = args.get(i + 1).and_then(|s| s.parse::<u64>().ok()) {
                    timeouts.search = Duration::from_millis(v);
                    i += 1;
                }
            }
            "--library" => {
                if let Some(v) = args.get(i + 1) {
                    library_dir = Some(v.clone());
                    i += 1;
                }
            }
            other => {
                eprintln!(
                    "未知参数: {}（支持 --search-timeout-ms <ms> / --library <dir>）",
                    other
                );
            }
        }
        i += 1;
    }

    set_app_id();
    reimuplayer::gui::run_reimu_player(timeouts, library_dir)
}

/// 任务栏身份：固定到任务栏的快捷方式、运行中的窗口、跳转列表按此归组。
///
/// 不设时 Windows 按 exe 路径推导，从不同目录启动可能出现两个任务栏按钮；
/// 设了之后 AUMID 是唯一身份，与图标资源里登记的路径无关。
#[cfg(windows)]
fn set_app_id() {
    use windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;

    let id: Vec<u16> = "ReimuPlayer.Desktop"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `id` 以 NUL 结尾，且在调用期间保持存活。
    let hr = unsafe { SetCurrentProcessExplicitAppUserModelID(id.as_ptr()) };
    if hr < 0 {
        tracing::warn!("设置 AppUserModelID 失败：HRESULT {hr:#010x}");
    }
}

#[cfg(not(windows))]
fn set_app_id() {}
