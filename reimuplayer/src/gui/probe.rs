//! 视觉校对探针（仅开发期，生产路径零开销）。
//!
//! 用途：自动化视觉回归——启动后等待 3 秒（等首帧布局/字体上传稳定），把指定页面的
//! GL framebuffer 直读导出成 PPM(P6)，随后进程自行退出，由宿主脚本回收。
//!
//! 触发方式（不设环境变量则整段逻辑不执行）：
//!
//! ```text
//! REIMU_PROBE_PAGE=player|search|rec|daily|playlist  reimu-player.exe
//! ```
//!
//! 刻意**不放进 `cargo test`**：它依赖真实 GL 上下文与窗口，headless CI 上必然失败；
//! 视觉回归由外部脚本 `tools/visual_regression.py`（浏览器离线渲染 HTML 基准）驱动。
//!
//! 注意：每进程只截一张。`request_screenshot` 连续调用会卡住帧循环（实测第 3 张后停滞）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use eframe::egui;

use super::app::Page;

/// 等待多久后截图（秒）。太短会拍到字体尚未上传/动画未收敛的首帧。
const SETTLE_SECS: f32 = 3.0;

/// 截图输出目录（相对仓库根；运行时工作目录约定为仓库根）。
const OUT_DIR: &str = "G:/AI_DEV/reimuplayer/outputs";

/// 稳定等待时长：可用 `REIMU_PROBE_SETTLE=<秒>` 覆盖。
///
/// 需要联网的页面（登录二维码、真实封面）在冷启动时 3 秒内拿不到响应，
/// 延长等待才能拍到"数据已就位"的画面。
fn settle_secs() -> f32 {
    std::env::var("REIMU_PROBE_SETTLE")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(SETTLE_SECS)
}

/// 读环境变量并映射为页面；非法值返回 `None`（等同于未启用探针）。
pub(crate) fn requested_page() -> Option<(&'static str, Page)> {
    let raw = std::env::var("REIMU_PROBE_PAGE").ok()?;
    match parse_page(&raw) {
        Some(hit) => Some(hit),
        None => {
            tracing::warn!("probe: 未知页面 `{raw}`，探针忽略");
            None
        }
    }
}

/// 页面名 → (规范化名, [`Page`])。纯函数，可测。
fn parse_page(raw: &str) -> Option<(&'static str, Page)> {
    match raw {
        "player" => Some(("player", Page::Player)),
        "search" => Some(("search", Page::Search)),
        "rec" => Some(("rec", Page::Rec)),
        "daily" => Some(("daily", Page::Daily)),
        "playlist" => Some(("playlist", Page::Playlist)),
        _ => None,
    }
}

/// `REIMU_PROBE_SETTINGS=1`：截屏前展开 Track Info 底部的设置区（视觉校对用）。
///
/// 设置区默认收起，探针不加这个开关就永远拍不到它。
pub(crate) fn wants_settings_open() -> bool {
    std::env::var("REIMU_PROBE_SETTINGS").ok().as_deref() == Some("1")
}

/// `REIMU_PROBE_FREE_NAV=1`：仍按 `REIMU_PROBE_PAGE` 抓帧，但**不把页面钉死**。
///
/// 默认每帧回钉是为了定格一个页面；但想拍"点开某首歌之后的播放态"就必须允许
/// 脚本在中途换页（列表 → 播放页），否则点击会被下一帧的钉页面立刻撤销。
pub(crate) fn free_nav() -> bool {
    std::env::var("REIMU_PROBE_FREE_NAV").ok().as_deref() == Some("1")
}

/// `REIMU_PROBE_LYRIC_WINDOW=1`：**仅调试构建**，启动即开启桌面歌词。
///
/// 用途：桌面歌词是**独立视口**，截图只能拍到主窗；没有这个钩子就无法确认
/// 悬浮窗是否真的创建、透明/置顶/穿透是否生效。发布构建里这段代码不存在。
#[cfg(debug_assertions)]
pub(crate) fn wants_lyric_window() -> bool {
    std::env::var("REIMU_PROBE_LYRIC_WINDOW").ok().as_deref() == Some("1")
}

/// `REIMU_PROBE_QUERY=<关键词>`：**仅调试构建**，启动即做一次本地检索。
///
/// 用途：本地命中的**逐字高亮**依赖 `LayoutJob` 分节，是纯视觉产物，单测只能
/// 验数据结构、验不了"底色是否落在对的那几个字上"。没有这个钩子，探针无法
/// 输入文字（不能模拟键盘输入）。
#[cfg(debug_assertions)]
pub(crate) fn forced_query() -> Option<String> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE
        .get_or_init(|| std::env::var("REIMU_PROBE_QUERY").ok())
        .clone()
}

/// `REIMU_PROBE_TITLE=<长标题>`：**仅调试构建**生效，强制替换 Track Info 标题。
///
/// 用途：校对走马灯滚动与「不越过 LOCAL 徽标」的裁剪边界——真实曲名长度不可控，
/// 没有一个固定的长标题就无法做视觉回归。发布构建里这段代码不存在。
#[cfg(debug_assertions)]
pub(crate) fn forced_title() -> Option<String> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE
        .get_or_init(|| std::env::var("REIMU_PROBE_TITLE").ok())
        .clone()
}

/// `REIMU_PROBE_TOAST=<文案>`：**仅调试构建**生效，每帧强制显示一条 toast。
///
/// 用途：校对提示条宽度/居中（真实 toast 只活 1.4 秒，截图窗口期太窄）。
#[cfg(debug_assertions)]
pub(crate) fn forced_toast() -> Option<String> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE
        .get_or_init(|| std::env::var("REIMU_PROBE_TOAST").ok())
        .clone()
}

/// `REIMU_PROBE_MENU=1`：**仅调试构建**，启动即在磁带架第一行弹一个右键菜单。
///
/// 用途：右键菜单画在 `Order::Foreground` 的 `Area` 里，是否被裁剪、描边/高亮
/// 是否与面板统一，只能看像素。探针没法模拟右键（`REIMU_PROBE_QUERY` 那条
/// 路子只覆盖文字输入），所以直接走"打开菜单"的公开路径。
#[cfg(debug_assertions)]
pub(crate) fn wants_context_menu() -> bool {
    std::env::var("REIMU_PROBE_MENU").ok().as_deref() == Some("1")
}

/// `REIMU_PROBE_CONFIRM=1`：**仅调试构建**，启动即弹出一个长文案确认框。
///
/// 用途：确认框的折行与纸面高度是纯像素产物，长歌单名曾直接冲出边框；
/// 探针无法模拟右键→删除的完整手势，只能直接走"打开确认框"的状态。
#[cfg(debug_assertions)]
pub(crate) fn wants_confirm_modal() -> bool {
    std::env::var("REIMU_PROBE_CONFIRM").ok().as_deref() == Some("1")
}

/// `REIMU_PROBE_ALBUM=<专辑 ID>`：**仅调试构建**，启动即打开该专辑页（磁带架复用）。
///
/// 用途：「专辑曲子」标题 + 副标题 + 「← 返回」入口都是像素级产物，且必须先
/// 有一次真实网络往返才有内容。没有这个钩子就只能靠手点，无法做视觉回归。
#[cfg(debug_assertions)]
pub(crate) fn forced_album() -> Option<String> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE
        .get_or_init(|| std::env::var("REIMU_PROBE_ALBUM").ok())
        .clone()
}

/// `REIMU_PROBE_PEEK=<rec|chart>:<歌单 ID>`：**仅调试构建**，启动即在对应页
/// 原地展开该歌单的详情。
///
/// 用途：详情页只在点击后出现，探针不能模拟鼠标——没有这个钩子就拍不到
/// 「页头 + 右上返回 + 曲目 spine」的像素结果。
///
/// 每日页**不在其中**：它进入即整列表展开（当天推荐的歌曲本身），
/// 没有"点开卡片才看到曲目"这一层，也就没有可探针的展开态。
#[cfg(debug_assertions)]
pub(crate) fn forced_peek() -> Option<(&'static str, String)> {
    static CACHE: OnceLock<Option<(&'static str, String)>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let raw = std::env::var("REIMU_PROBE_PEEK").ok()?;
            let (panel, id) = raw.split_once(':')?;
            let panel = match panel {
                "rec" => "rec",
                "chart" => "chart",
                _ => return None,
            };
            let id = id.trim();
            (!id.is_empty()).then(|| (panel, id.to_string()))
        })
        .clone()
}

/// `REIMU_PROBE_DAILY=loading|failed|empty`：**仅调试构建**，
/// 把每日推荐钉在指定状态上。
///
/// 用途：加载中 / 需要登录 / 空态这三张版式只能看像素，而真实状态不可构造
/// ——要么等网络超时，要么真把账号退出去。探针让它们在截图里可复现。
#[cfg(debug_assertions)]
pub(crate) fn daily_state_override() -> Option<&'static str> {
    match std::env::var("REIMU_PROBE_DAILY").ok()?.as_str() {
        "loading" => Some("loading"),
        "failed" => Some("failed"),
        "empty" => Some("empty"),
        _ => None,
    }
}

/// `REIMU_PROBE_LYRIC_SHOT=<PNG 路径>`：**仅调试构建**，把歌词窗的 framebuffer 写成 PNG。
///
/// 用途：歌词窗是独立视口，屏幕截图拍到的只有窗口底色（这台机器上连主窗口也是），
/// 圆角/透明/0% 无底板这些纯像素结论只能直读它自己的 framebuffer 得到。
#[cfg(debug_assertions)]
pub(crate) fn lyric_shot_path() -> Option<String> {
    std::env::var("REIMU_PROBE_LYRIC_SHOT").ok()
}

/// `REIMU_PROBE_LYRIC_LOCKED=1`：**仅调试构建**，启动即把桌面歌词设为锁定态。
///
/// 用途：锁定后控件窗会多出一条透明度滑杆（宽度从 92 变 232），并且展示窗右上角
/// 出现 `LOCKED` 角标——这两件事都是纯像素产物，只能截图校对。
#[cfg(debug_assertions)]
pub(crate) fn wants_lyric_locked() -> bool {
    std::env::var("REIMU_PROBE_LYRIC_LOCKED").ok().as_deref() == Some("1")
}

/// `REIMU_PROBE_LYRIC_OPACITY=<0..100>`：**仅调试构建**，强制桌面歌词背景不透明度。
///
/// 用途：底板 alpha 是 0..=255 的连续量，"30% 时白字还读不读得清"只能看像素。
#[cfg(debug_assertions)]
pub(crate) fn forced_lyric_opacity() -> Option<u8> {
    std::env::var("REIMU_PROBE_LYRIC_OPACITY")
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
        .map(crate::gui::lyric_control::clamp_opacity)
}

/// `REIMU_PROBE_LANG=zh|en`：强制界面语言。
///
/// 用途：中/英两态各截一张图校对文案——语言是用户偏好，正常路径读
/// `prefs.json`，探针不能去改用户的真实偏好文件（只覆盖当次运行，不落盘）。
///
/// 为什么**不像** `forced_title` / `forced_toast` 那样只留调试构建：语言是
/// 合法的用户偏好覆盖（等价于命令行 `--lang`），不伪造任何内容。它曾经只在调试
/// 构建生效，结果用 release exe 复核时该变量被**静默忽略**，界面语言其实来自
/// `prefs.json`，排查这类"语言怎么自己变了"的问题时代价很高。内容伪造类的覆盖
/// （标题、提示条）仍然只留调试构建。
pub(crate) fn forced_lang() -> Option<crate::i18n::Lang> {
    static CACHE: OnceLock<Option<crate::i18n::Lang>> = OnceLock::new();
    *CACHE.get_or_init(|| {
        std::env::var("REIMU_PROBE_LANG")
            .ok()
            .and_then(|v| crate::i18n::Lang::from_code(&v))
    })
}

/// 每帧驱动：稳定后截一张并退出；此前保持置顶 + 主动重绘防节流。
pub(crate) fn drive(ctx: &egui::Context, target: &'static str) {
    static SHOT_DONE: AtomicBool = AtomicBool::new(false);
    static START: OnceLock<Instant> = OnceLock::new();

    let elapsed = START.get_or_init(Instant::now).elapsed().as_secs_f32();

    if elapsed > settle_secs() && !SHOT_DONE.swap(true, Ordering::SeqCst) {
        capture(ctx, target);
    }

    // 防节流：置顶 + 每帧主动重绘（否则最小化/失焦时帧循环被挂起，永远到不了 3 秒）
    ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
        egui::WindowLevel::AlwaysOnTop,
    ));
    ctx.request_repaint();
}

/// 抓取当前帧并写盘，回调内直接退出进程。
fn capture(ctx: &egui::Context, target: &'static str) {
    let path = format!("{OUT_DIR}/probe_page_{target}.ppm");
    ctx.request_screenshot(move |img: std::sync::Arc<egui::ColorImage>| {
        let mut buf = format!("P6\n{} {}\n255\n", img.width(), img.height()).into_bytes();
        for px in &img.pixels {
            let [r, g, b, _] = px.to_array();
            buf.extend_from_slice(&[r, g, b]);
        }
        match std::fs::write(&path, &buf) {
            Ok(()) => tracing::info!("probe: 页面帧已写 {path}"),
            Err(e) => tracing::warn!("probe: 页面帧写入失败 {e}"),
        }
        // 截完即退，让宿主脚本回收
        std::process::exit(0);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_page_round_trips() {
        // 全覆盖：名字与枚举必须一一对应，改页面时漏改探针会在此失败
        assert_eq!(parse_page("player"), Some(("player", Page::Player)));
        assert_eq!(parse_page("search"), Some(("search", Page::Search)));
        assert_eq!(parse_page("rec"), Some(("rec", Page::Rec)));
        assert_eq!(parse_page("daily"), Some(("daily", Page::Daily)));
        assert_eq!(parse_page("playlist"), Some(("playlist", Page::Playlist)));
    }

    #[test]
    fn unknown_target_is_ignored_not_panicking() {
        assert_eq!(parse_page(""), None);
        assert_eq!(parse_page("Player"), None); // 大小写敏感，避免脚本静默出错
        assert_eq!(parse_page("player "), None);
    }
}
