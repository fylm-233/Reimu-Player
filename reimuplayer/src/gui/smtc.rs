//! Windows SMTC（System Media Transport Controls / 系统媒体传输控件）。
//!
//! 作用：让 Windows 自带的媒体浮层（音量键弹出的那块、锁屏、蓝牙耳机上的
//! 播放键）显示当前曲目，并能反过来控制播放。它与 [`super::media_keys`] 是
//! **两件事**：
//!
//! - `media_keys`：全局热键（`RegisterHotKey`），只听键盘上的媒体键；
//! - `smtc`：向系统登记一个"媒体会话"，系统 UI 与蓝牙设备都从这里取信息。
//!
//! 用户在设置里看到的是同一行「媒体键\SMTC」，因为对用户来说它们就是同一件
//! 事——按一下播放键，播放器要动。所以开关一次同时管两者。
//!
//! 依赖：`smtc-player` 0.1（MIT）。它内部用 `MediaPlayer` 作为 WinRT 宿主，
//! 因此**不需要窗口句柄**——这点很关键：eframe 的 `CreationContext` 拿不到
//! winit 的 `Window`（字段是 `pub(crate)`），走 `ISystemMediaTransportControlsInterop
//! ::GetForWindow(hwnd)` 那条路在 eframe 里根本走不通。
//!
//! 为什么不用 `MediaPlayer` 自己播：我们把它的 `CommandManager` 关掉了，它只
//! 充当 SMTC 的注册载体，真正出声的还是 `rodio`。

/// SMTC 按钮事件（与 `smtc-player` 解耦一层，方便将来换实现）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmtcButton {
    /// 播放。
    Play,
    /// 暂停。
    Pause,
    /// 上一首。
    Previous,
    /// 下一首。
    Next,
    /// 停止。
    Stop,
}

#[cfg(windows)]
mod platform {
    use super::SmtcButton;
    use std::sync::mpsc::{channel, Receiver};

    /// SMTC 会话（持有即已向系统登记）。
    pub struct Smtc {
        /// WinRT 宿主。字段不读也要留着：drop 时它会注销媒体会话。
        _player: smtc_player::SmtcPlayer,
        /// 按钮事件通道（回调在 WinRT 线程里跑，只能靠 channel 回传）。
        rx: Receiver<SmtcButton>,
        /// 上次写进系统的标题，避免每帧重设元数据（WinRT 调用不便宜）。
        last: String,
        /// 上次写进系统的播放态（`None` = 还没写过）。
        last_playing: Option<bool>,
    }

    impl Smtc {
        /// 建立媒体会话并订阅按钮事件。
        ///
        /// 失败只可能是 WinRT 激活失败（极旧系统 / 组件被裁剪），调用方提示一次即可。
        pub fn start() -> Result<Self, String> {
            let player = smtc_player::SmtcPlayer::new().map_err(|e| e.to_string())?;
            let (tx, rx) = channel();
            player
                .on_event(move |event| {
                    let mapped = match event {
                        smtc_player::SmtcEvent::Play => SmtcButton::Play,
                        smtc_player::SmtcEvent::Pause => SmtcButton::Pause,
                        smtc_player::SmtcEvent::Previous => SmtcButton::Previous,
                        smtc_player::SmtcEvent::Next => SmtcButton::Next,
                        smtc_player::SmtcEvent::Stop => SmtcButton::Stop,
                    };
                    // 接收端在 UI 线程；发失败说明 app 已经退出，忽略即可
                    let _ = tx.send(mapped);
                })
                .map_err(|e| e.to_string())?;
            Ok(Self {
                _player: player,
                rx,
                last: String::new(),
                last_playing: None,
            })
        }

        /// 更新系统浮层上的曲目信息（标题/艺人/专辑）。
        ///
        /// 只比标题：同一首歌的三行元数据是一起变的，比三个字段没有额外收益。
        pub fn set_now_playing(&mut self, title: &str, artist: &str, album: &str) {
            if self.last == title {
                return;
            }
            self.last = title.to_string();
            // 系统浮层拿不到就只丢一行日志：这只是"锦上添花"的能力，
            // 不该因为它把播放搞坏
            if let Err(e) = self._player.set_media_info(title, artist, album, None) {
                tracing::debug!("SMTC 元数据写入失败: {e}");
            }
        }

        /// 同步播放/暂停状态（系统浮层据此显示 ▶ / ⏸ 并决定按钮语义）。
        ///
        /// 状态没变就什么都不做：这是每帧都会调的方法，`SetPlaybackStatus` 是
        /// 跨进程 WinRT 调用，60 次/秒的冗余调用会白白吃掉主线程时间。
        pub fn set_playing(&mut self, playing: bool) {
            if self.last_playing == Some(playing) {
                return;
            }
            self.last_playing = Some(playing);
            if let Err(e) = self._player.set_playback_status(playing) {
                tracing::debug!("SMTC 播放状态写入失败: {e}");
            }
        }

        /// 取走这一帧的按钮事件（非阻塞）。
        pub fn poll(&self) -> Vec<SmtcButton> {
            let mut out = Vec::new();
            while let Ok(btn) = self.rx.try_recv() {
                out.push(btn);
            }
            out
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::SmtcButton;

    /// 非 Windows 平台的空实现（本产品当前只发布 Windows 版）。
    pub struct Smtc {
        /// 占位。
        _never: core::marker::PhantomData<()>,
    }

    impl Smtc {
        /// 恒失败：非 Windows 没有 SMTC。
        pub fn start() -> Result<Self, String> {
            Err("SMTC is only available on Windows".to_string())
        }

        /// 空实现。
        pub fn set_now_playing(&mut self, _title: &str, _artist: &str, _album: &str) {}

        /// 空实现。
        pub fn set_playing(&mut self, _playing: bool) {}

        /// 恒空。
        pub fn poll(&self) -> Vec<SmtcButton> {
            Vec::new()
        }
    }
}

pub use platform::Smtc;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_variants_are_distinct() {
        // 与 media_keys / tray 同一类保护：相等变体会让 match 分支互相吞掉
        let all = [
            SmtcButton::Play,
            SmtcButton::Pause,
            SmtcButton::Previous,
            SmtcButton::Next,
            SmtcButton::Stop,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}
