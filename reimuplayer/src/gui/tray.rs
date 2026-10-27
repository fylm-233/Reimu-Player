//! 托盘图标 + 右键菜单（**默认关闭**）。
//!
//! 为什么默认关闭：托盘图标一旦出现就会常驻任务栏右下角，属于"装上了就撤不掉"
//! 的东西。用户没主动要之前不该擅自占用系统托盘；因此 [`crate::gui::prefs`]
//! 里的开关默认 `false`，设置面板里显式打开才创建。
//!
//! 依赖：`tray-icon` 0.26（MIT OR Apache-2.0，Tauri 官方维护，Windows 端走
//! `Shell_NotifyIconW`，不需要我们手写 `NOTIFYICONDATA`）。
//!
//! 三个实现约束（都是 Windows 上真实踩过的）：
//!
//! 1. **必须在主线程创建**。`tray-icon` 的 Windows 后端会建一个隐藏窗口收
//!    `WM_TRAYICON`，消息靠创建线程的消息泵派发；主线程就是 winit 的事件循环
//!    线程，天然在跑消息泵。放到别的线程会「图标能显示、右键菜单点不动」。
//! 2. **菜单事件走全局 channel**（`MenuEvent::receiver()`），不是回调。UI 每帧
//!    `poll()` 一次即可，不需要 `EventLoopProxy`，也不用把 winit 类型漏进 app。
//! 3. **图标与窗口 / 资源管理器同源**：读 `assets/icon/tray.png`（32px
//!    应用图标，含米色底板、透明底），不再单独用代码画一套。托盘 API 只收
//!    单张位图，32px 让 Windows 在 100–200% 缩放下都是缩小而非放大，
//!    几何形状最稳。

/// 托盘菜单回传的命令（UI 线程解释）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    /// 播放 / 暂停切换。
    PlayPause,
    /// 上一首。
    Prev,
    /// 下一首。
    Next,
    /// 收藏 / 取消收藏当前曲目。
    ToggleFavorite,
    /// 把主窗口叫回前台。
    Show,
    /// 退出程序。
    Quit,
}

/// 菜单项 id（与 [`TrayCommand`] 一一对应）。
pub mod ids {
    /// 播放 / 暂停。
    pub const PLAY_PAUSE: &str = "reimu-player.play_pause";
    /// 上一首。
    pub const PREV: &str = "reimu-player.prev";
    /// 下一首。
    pub const NEXT: &str = "reimu-player.next";
    /// 收藏。
    pub const FAVORITE: &str = "reimu-player.favorite";
    /// 显示主窗口。
    pub const SHOW: &str = "reimu-player.show";
    /// 退出。
    pub const QUIT: &str = "reimu-player.quit";
}

/// 菜单项 id → 命令（纯函数，可脱离托盘单测）。
///
/// 未知 id 返回 `None`：将来加项忘了映射时宁可静默丢弃，也不要误触发别的命令。
pub fn command_for_id(id: &str) -> Option<TrayCommand> {
    match id {
        ids::PLAY_PAUSE => Some(TrayCommand::PlayPause),
        ids::PREV => Some(TrayCommand::Prev),
        ids::NEXT => Some(TrayCommand::Next),
        ids::FAVORITE => Some(TrayCommand::ToggleFavorite),
        ids::SHOW => Some(TrayCommand::Show),
        ids::QUIT => Some(TrayCommand::Quit),
        _ => None,
    }
}

#[cfg(windows)]
mod platform {
    use super::{command_for_id, ids, TrayCommand};
    use crate::i18n::{pick, Lang};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

    /// 托盘图标（持有即存在；`Drop` 会移除图标）。
    pub struct Tray {
        /// 图标本体。字段不读也要留着——它一被 drop 图标就消失。
        _icon: TrayIcon,
        /// 播放/暂停那一项：文案随播放状态变（"播放" ↔ "暂停"）。
        play_item: MenuItem,
        /// 收藏那一项：文案随收藏状态变。
        fav_item: MenuItem,
        /// 当前语言（换语言时重建文案）。
        lang: Lang,
        /// 上次写入的播放态，避免每帧 `set_text` 触发重绘。
        playing: bool,
        /// 上次写入的收藏态。
        favorited: bool,
    }

    impl Tray {
        /// 创建托盘图标与菜单。
        ///
        /// 失败（极少：通常是 Explorer 未就绪）返回 `Err`，调用方只提示一次。
        pub fn new(lang: Lang, playing: bool, favorited: bool) -> Result<Self, String> {
            let menu = Menu::new();
            let play_item = MenuItem::with_id(
                ids::PLAY_PAUSE,
                pick(lang, "播放 / 暂停", "Play / Pause"),
                true,
                None,
            );
            let prev = MenuItem::with_id(ids::PREV, pick(lang, "上一首", "Previous"), true, None);
            let next = MenuItem::with_id(ids::NEXT, pick(lang, "下一首", "Next"), true, None);
            let fav_item =
                MenuItem::with_id(ids::FAVORITE, pick(lang, "收藏", "Favorite"), true, None);
            let show = MenuItem::with_id(
                ids::SHOW,
                pick(lang, "显示主窗口", "Show window"),
                true,
                None,
            );
            let quit = MenuItem::with_id(ids::QUIT, pick(lang, "退出", "Quit"), true, None);
            // 顺序 = 用户最常用的在上：切歌 → 收藏 → 找回窗口 → 退出
            let _ = menu.append_items(&[
                &play_item,
                &prev,
                &next,
                &fav_item,
                &tray_icon::menu::PredefinedMenuItem::separator(),
                &show,
                &quit,
            ]);

            let icon = tape_icon()?;
            let tray = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip(crate::BRAND)
                .with_icon(icon)
                .build()
                .map_err(|e| e.to_string())?;

            Ok(Self {
                _icon: tray,
                play_item,
                fav_item,
                lang,
                playing,
                favorited,
            })
        }

        /// 同步「播放/暂停」与「收藏」两项的文案（状态没变就什么都不做）。
        ///
        /// 托盘菜单是原生菜单，改文案要 `set_text` 重建，比 egui 里改字符串贵，
        /// 所以只在状态真的翻转时写一次。
        pub fn sync(&mut self, lang: Lang, playing: bool, favorited: bool) {
            if lang != self.lang {
                self.lang = lang;
                // 语言变了，两项都要重写
                self.playing = !playing;
                self.favorited = !favorited;
            }
            if playing != self.playing {
                self.playing = playing;
                self.play_item.set_text(pick(
                    lang,
                    if playing { "暂停" } else { "播放" },
                    if playing { "Pause" } else { "Play" },
                ));
            }
            if favorited != self.favorited {
                self.favorited = favorited;
                self.fav_item.set_text(pick(
                    lang,
                    if favorited { "取消收藏" } else { "收藏" },
                    if favorited { "Unfavorite" } else { "Favorite" },
                ));
            }
        }

        /// 取走这一帧的菜单命令（非阻塞）。
        pub fn poll(&self) -> Vec<TrayCommand> {
            let mut out = Vec::new();
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if let Some(cmd) = command_for_id(event.id.0.as_str()) {
                    out.push(cmd);
                }
            }
            out
        }
    }

    /// 托盘图标：`assets/icon/tray.png`（32×32 应用图标，透明底）。
    fn tape_icon() -> Result<Icon, String> {
        let rgba = image::load_from_memory(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../assets/icon/tray.png"
        )))
        .map_err(|e| format!("托盘图标解码失败: {e}"))?
        .into_rgba8();
        let (w, h) = rgba.dimensions();
        Icon::from_rgba(rgba.into_raw(), w, h).map_err(|e| e.to_string())
    }
}

#[cfg(not(windows))]
mod platform {
    use super::TrayCommand;
    use crate::i18n::Lang;

    /// 非 Windows 平台的空实现（本产品当前只发布 Windows 版）。
    pub struct Tray {
        /// 占位，保持字段非空以免 `Option<Tray>` 的 `Some` 语义失真。
        _never: core::marker::PhantomData<()>,
    }

    impl Tray {
        /// 恒失败：非 Windows 没有托盘后端。
        pub fn new(_lang: Lang, _playing: bool, _favorited: bool) -> Result<Self, String> {
            Err("tray icon is only implemented on Windows".to_string())
        }

        /// 空实现。
        pub fn sync(&mut self, _lang: Lang, _playing: bool, _favorited: bool) {}

        /// 恒空。
        pub fn poll(&self) -> Vec<TrayCommand> {
            Vec::new()
        }
    }
}

pub use platform::Tray;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_menu_id_maps_to_a_command() {
        for (id, want) in [
            (ids::PLAY_PAUSE, TrayCommand::PlayPause),
            (ids::PREV, TrayCommand::Prev),
            (ids::NEXT, TrayCommand::Next),
            (ids::FAVORITE, TrayCommand::ToggleFavorite),
            (ids::SHOW, TrayCommand::Show),
            (ids::QUIT, TrayCommand::Quit),
        ] {
            assert_eq!(command_for_id(id), Some(want), "id {id} 映射错了");
        }
    }

    #[test]
    fn unknown_ids_are_ignored() {
        // 未来加项忘了映射时宁可静默丢弃，也不能误触发别的命令
        assert_eq!(command_for_id("reimu-player.nope"), None);
        assert_eq!(command_for_id(""), None);
    }

    #[test]
    fn commands_are_distinct() {
        // 两个命令相等会让 match 分支互相吞掉（与 settings 动作同一类保护）
        let all = [
            TrayCommand::PlayPause,
            TrayCommand::Prev,
            TrayCommand::Next,
            TrayCommand::ToggleFavorite,
            TrayCommand::Show,
            TrayCommand::Quit,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn menu_ids_are_unique() {
        let all = [
            ids::PLAY_PAUSE,
            ids::PREV,
            ids::NEXT,
            ids::FAVORITE,
            ids::SHOW,
            ids::QUIT,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b, "菜单 id 撞了：{a}");
            }
        }
    }
}
