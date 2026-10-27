//! GUI 测试程序（阶段 F）——Reimu Player 五页窗口 + 在线播放/歌词验证载体。
//!
//! 职责：在线搜索、歌曲详情、歌单读取、**在线音频播放与歌词时间轴**的界面验证——
//! 加载/缓冲状态展示（[`crate::LoadState`] / [`player_core::PlayerCore`]）、
//! 请求超时控制（[`crate::providers::ProviderTimeouts`]）、取消请求、
//! 资源释放（临时文件回收）与友好错误提示（模态对话框，规范 §3.9 / 集成规范 8.2）。
//!
//! 模块地图：
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`theme`] | 设计令牌的 Rust 常量表（GUI 唯一写颜色的地方） |
//! | [`layout`] | 布局纯函数（fr 列比 / 居中行 / art 高度 / 像素吸附），零 IO 可单测 |
//! | [`halftone`] | 封面印刷网纹离屏算法（纯字节层，可完整单测） |
//! | [`bridge`] | UI 线程 ↔ Provider 异步桥（网络全在工作线程，可取消） |
//! | [`player_core`] | 播放编排核心（PlaySession + PlaybackController + 歌词时间轴） |
//! | [`widgets`] | 组件：spine / 波形 / 歌词面板 / EQ / 结果行 / 热榜行 / 舱体 / Toast |
//! | [`app`] | Reimu Player 五页装配（Player / Search / Rec / Hot / Playlist）与交互流程 |
//!
//! 视觉权威 `code (3).html`（SONORA — System 6 / Y2K Tape Deck，1100px 基准）；
//! 可执行视觉规范见 `docs/WINDOW_UI_DESIGN.md`。

pub mod app;
pub mod bridge;
pub mod click_through;
pub mod clipboard;
pub mod context_menu;
pub mod daily_clock;
pub mod deck_seq;
pub mod desktop_lyric;
pub mod favorites;
pub mod halftone;
pub mod history;
pub mod icons;
pub mod layout;
pub mod library;
pub mod local_playlist;
pub mod login;
pub mod lyric_cache;
pub mod lyric_control;
pub mod lyric_window;
pub mod media_keys;
pub mod perf_log;
pub mod persist;
pub mod player_core;
pub mod prefs;
pub mod probe;
pub mod qr;
pub mod recommend;
pub mod session_store;
pub mod settings;
pub mod smtc;
pub mod theme;
pub mod tray;
pub mod widgets;
pub mod window;

use std::sync::Arc;

use crate::music::provider::DynMusicProvider;
use crate::music::DynAudioFetcher;
use crate::providers::{HttpAudioFetcher, NeteaseProvider, ProviderTimeouts};

/// GUI 测试程序入口。
///
/// `timeouts` 为 Provider 请求超时配置（请求超时控制）；
/// `factory` 决定桥背后的具体 Provider 实现（可替换性入口：
/// 测试注入 Mock，生产注入 [`NeteaseProvider`]）。音频下载使用
/// [`HttpAudioFetcher`]（15s 空闲超时 + Range 断点续传 / 10s 连接超时）。
pub fn run(
    title: &str,
    timeouts: ProviderTimeouts,
    factory: Arc<dyn Fn(ProviderTimeouts) -> Arc<dyn DynMusicProvider> + Send + Sync>,
) -> eframe::Result {
    let fetcher: Arc<dyn DynAudioFetcher> = match HttpAudioFetcher::new() {
        Ok(f) => Arc::new(f),
        Err(e) => {
            eprintln!("音频下载器构造失败: {}", e);
            std::process::exit(1);
        }
    };
    run_with_fetcher(title, timeouts, factory, fetcher)
}

/// 入口（自定义下载器，测试注入 Stub 用）。
pub fn run_with_fetcher(
    title: &str,
    timeouts: ProviderTimeouts,
    factory: Arc<dyn Fn(ProviderTimeouts) -> Arc<dyn DynMusicProvider> + Send + Sync>,
    fetcher: Arc<dyn DynAudioFetcher>,
) -> eframe::Result {
    run_with_library(title, timeouts, factory, fetcher, None)
}

/// 入口（含启动曲库目录）：`library_dir` 给定时启动即扫描本地曲库。
pub fn run_with_library(
    title: &str,
    timeouts: ProviderTimeouts,
    factory: Arc<dyn Fn(ProviderTimeouts) -> Arc<dyn DynMusicProvider> + Send + Sync>,
    fetcher: Arc<dyn DynAudioFetcher>,
    library_dir: Option<String>,
) -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_decorations(false) // 自绘边框/标题栏（gui::window）
            .with_inner_size([1100.0, 860.0]) // 设计稿 1100px 基准（GUI_ALIGNMENT_PLAN §5.5）
            .with_min_inner_size([900.0, 620.0]) // 最小窗口 900×620（§6.1）
            // 空 IconData：eframe 会当成「不设图标」，否则回落到 egui 默认 logo；
            // 真正的窗口图标由 set_window_icon 从 PE 多尺寸资源里取。
            .with_icon(eframe::egui::IconData::default())
            .with_title(title),
        // ⛔ 必须关掉 egui-winit 的剪贴板快捷键接管。
        //
        // 它默认开着时，`egui_winit::State::on_keyboard_input` 遇到 Ctrl+C/X/V
        // 会走 `is_paste_command` 分支并**直接 `return`**：
        //   - 我们没开 eframe 的 `clipboard` feature → `self.clipboard` 是空实现，
        //     `get()` 永远 `None` → 连 `Event::Paste` 都发不出来；
        //   - 而那个 `return` 同时**吞掉了 `Event::Key`**，于是
        //     `gui::app::inject_paste` 里 `key_pressed(Key::V)` 永远为假。
        // 结果就是 Ctrl+V 完全没反应（右键菜单的粘贴反而能用——它不走按键）。
        //
        // 置 false 之后 egui-winit 把 `Event::Key { key: V, ctrl }` 正常发出来，
        // `inject_paste` 才能接管。代价是 Ctrl+C/X 不再产生 `Event::Copy`/`Cut`
        // ——但那条路本来也要靠 `clipboard` feature 才有意义（现在是空实现，
        // 复制本来就无效），所以这里没有任何净损失。
        clipboard_shortcuts: false,
        ..Default::default()
    };
    eframe::run_native(
        title,
        options,
        Box::new(move |cc| {
            #[cfg(windows)]
            set_window_icon(cc);
            Ok(Box::new(app::ReimuApp::new(
                cc,
                timeouts,
                factory,
                fetcher,
                library_dir,
            )))
        }),
    )
}

/// 窗口图标（任务栏 / Alt+Tab / 标题栏）：按系统请求的尺寸从 exe 内嵌的多尺寸
/// 图标里取原图，而不是把单张位图缩放到 20/40。
///
/// eframe 只会拿一张 `IconData` 做 Lanczos 缩放后写入 `WM_SETICON`；
/// 这里让每个 DPI 档都命中资源里手绘的那一档，与资源管理器走同一条来源。
#[cfg(windows)]
fn set_window_icon(cc: &eframe::CreationContext<'_>) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, LoadImageW, SendMessageW, ICON_BIG, ICON_SMALL, IMAGE_ICON,
        LR_DEFAULTCOLOR, SM_CXICON, SM_CXSMICON, WM_SETICON,
    };

    let Ok(handle) = cc.window_handle() else {
        tracing::warn!("拿不到窗口句柄，跳过窗口图标设置");
        return;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return;
    };
    let hwnd = win32.hwnd.get() as *mut core::ffi::c_void;

    // 图标组 ID 由 build.rs 里 winresource 的 set_icon 固定为 1。
    const ICON_RESOURCE_ID: usize = 1;

    // SAFETY: hwnd 来自 eframe 已创建的窗口；LoadImageW 返回的 HICON 交给
    // WM_SETICON 后由窗口持有，窗口存活期间不销毁（进程退出时整体回收）。
    unsafe {
        let module = GetModuleHandleW(std::ptr::null());
        for (metric, slot) in [(SM_CXICON, ICON_BIG), (SM_CXSMICON, ICON_SMALL)] {
            let size = GetSystemMetrics(metric);
            let icon = LoadImageW(
                module,
                ICON_RESOURCE_ID as *const u16,
                IMAGE_ICON,
                size,
                size,
                LR_DEFAULTCOLOR,
            );
            if icon.is_null() {
                tracing::warn!(size, "窗口图标加载失败");
                continue;
            }
            SendMessageW(hwnd, WM_SETICON, slot as usize, icon as isize);
        }
    }
}

/// 默认入口：匿名网易云 Provider + 规范默认超时。
pub fn run_default() -> eframe::Result {
    run(
        "REIMU PLAYER AUDIO PLAYER — ONLINE PLAYBACK TEST",
        ProviderTimeouts::default(),
        Arc::new(|timeouts| {
            Arc::new(NeteaseProvider::new().with_timeouts(timeouts)) as Arc<dyn DynMusicProvider>
        }),
    )
}

/// Reimu Player 产品入口（`src/bin/reimu-player.rs` 调用）。
///
/// 与 [`run_default`] 的区别：产品标题、自绘窗口边框模式（阶段 G），
/// 以及可选的启动曲库目录（`--library`，启动即扫描，见 [`library`]）。
pub fn run_reimu_player(timeouts: ProviderTimeouts, library_dir: Option<String>) -> eframe::Result {
    // 改名（旧品牌 → Reimu Player）带来的配置目录变更必须在**读会话之前**迁移，
    // 否则老用户看到的是"没登录过"的空界面。迁移只复制缺失文件，不删旧目录。
    let migrated = library::migrate_legacy_config_dir();
    if migrated > 0 {
        tracing::info!("配置迁移：从旧目录复制了 {migrated} 个文件");
    }
    let fetcher: Arc<dyn DynAudioFetcher> = match HttpAudioFetcher::new() {
        Ok(f) => Arc::new(f),
        Err(e) => {
            eprintln!("音频下载器构造失败: {}", e);
            std::process::exit(1);
        }
    };
    // 启动即恢复登录会话：从安全存储解密 Cookie 喂给 Provider；是否有效由
    // 后续 `login_status` 判定（失效则回匿名并清除存储）。
    let restored_cookie = crate::gui::session_store::load().map(|s| s.cookie);
    run_with_library(
        crate::BRAND,
        timeouts,
        Arc::new(move |timeouts| {
            let provider = match restored_cookie.clone() {
                Some(cookie) => NeteaseProvider::new_with_cookie(cookie),
                None => NeteaseProvider::new(),
            };
            Arc::new(provider.with_timeouts(timeouts)) as Arc<dyn DynMusicProvider>
        }),
        fetcher,
        library_dir,
    )
}
