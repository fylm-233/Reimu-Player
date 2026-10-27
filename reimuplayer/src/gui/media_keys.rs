//! 全局媒体键（播放/暂停、上一首、下一首、停止）。
//!
//! 为什么不用 Tauri 插件那套：本项目是 winit/eframe，没有 Tauri 的插件层。
//! Windows 上最轻的做法是 `RegisterHotKey` + 独立线程消息循环：
//!
//! - 热键注册在线程内完成，消息也由该线程 `GetMessageW` 收取，
//!   不需要主窗口 HWND（`hwnd = NULL` 时 `WM_HOTKEY` 投递到线程队列）；
//! - 收到消息只做一件事：把 [`MediaCommand`] 通过 channel 发给 UI 线程，
//!   UI 线程每帧 `poll` 一次，转成**既有的** PlayerCore 命令。
//!
//! 为什么不用 `WH_KEYBOARD_LL` 钩子：它会截获全系统按键，权限与隐私面都更大；
//! `RegisterHotKey` 失败（被网易云客户端/音乐软件占用）时我们只记日志并如实
//! 告知用户，不偷偷降级成键盘钩子。
//!
//! 依赖：`windows-sys`（winit 已在依赖树里，特性合并后不新增编译单元）。

/// 媒体键动作（与播放器核心解耦，便于单测）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCommand {
    /// 播放 / 暂停切换。
    Toggle,
    /// 下一首。
    Next,
    /// 上一首。
    Prev,
    /// 停止。
    Stop,
}

/// 媒体键注册状态（设置面板如实展示）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKeyStatus {
    /// 线程还在注册中。
    Starting,
    /// 至少有一个键注册成功。
    Ready,
    /// 全部被占用/拒绝。
    Unavailable,
    /// 非 Windows 平台（本产品当前只发布 Windows 版）。
    Unsupported,
    /// 用户主动关闭。
    Disabled,
}

// Windows 虚拟键码（避免为了四个常量引入额外 feature 依赖）
pub const VK_MEDIA_NEXT_TRACK: u32 = 0xB0;
pub const VK_MEDIA_PREV_TRACK: u32 = 0xB1;
pub const VK_MEDIA_STOP: u32 = 0xB2;
pub const VK_MEDIA_PLAY_PAUSE: u32 = 0xB3;

/// 虚拟键码 → 动作（纯函数，可测）。
pub fn command_for_vk(vk: u32) -> Option<MediaCommand> {
    match vk {
        VK_MEDIA_PLAY_PAUSE => Some(MediaCommand::Toggle),
        VK_MEDIA_NEXT_TRACK => Some(MediaCommand::Next),
        VK_MEDIA_PREV_TRACK => Some(MediaCommand::Prev),
        VK_MEDIA_STOP => Some(MediaCommand::Stop),
        _ => None,
    }
}

/// 热键 id → 动作（`RegisterHotKey` 的 id 是我们自己分配的 1..=4）。
pub fn command_for_hotkey_id(id: i32) -> Option<MediaCommand> {
    match id {
        1 => Some(MediaCommand::Toggle),
        2 => Some(MediaCommand::Next),
        3 => Some(MediaCommand::Prev),
        4 => Some(MediaCommand::Stop),
        _ => None,
    }
}

/// 注册表：`(热键 id, 虚拟键码)`。
const HOTKEYS: [(i32, u32); 4] = [
    (1, VK_MEDIA_PLAY_PAUSE),
    (2, VK_MEDIA_NEXT_TRACK),
    (3, VK_MEDIA_PREV_TRACK),
    (4, VK_MEDIA_STOP),
];

/// 媒体键监听器。
pub struct MediaKeys {
    rx: std::sync::mpsc::Receiver<MediaCommand>,
    status: std::sync::Arc<std::sync::atomic::AtomicU8>,
    /// 监听线程的系统线程 id（0 = 尚未就绪）。
    ///
    /// 关闭媒体键时必须能把阻塞在 `GetMessageW` 里的线程叫醒——只有
    /// `PostThreadMessageW(tid, WM_QUIT)` 做得到（共享标志位唤不醒阻塞调用）。
    thread_id: std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// 监听线程是否还活着（线程退出前置 `false`）。只用于诊断与测试。
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl MediaKeys {
    /// 启动监听（非 Windows 平台返回一个恒为 `Unsupported` 的空壳）。
    pub fn start() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let status = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
        let thread_id = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        spawn_listener(tx, status.clone(), thread_id.clone(), alive.clone());
        Self {
            rx,
            status,
            thread_id,
            alive,
        }
    }

    /// 监听线程存活标志的句柄（在 `drop` 之前取出即可观察线程是否收尾）。
    pub fn alive_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.alive.clone()
    }

    /// 取走本帧收到的全部命令。
    pub fn poll(&self) -> Vec<MediaCommand> {
        let mut out = Vec::new();
        while let Ok(cmd) = self.rx.try_recv() {
            out.push(cmd);
        }
        out
    }

    /// 当前注册状态。
    pub fn status(&self) -> MediaKeyStatus {
        match self.status.load(std::sync::atomic::Ordering::SeqCst) {
            0 => MediaKeyStatus::Starting,
            1 => MediaKeyStatus::Ready,
            2 => MediaKeyStatus::Unavailable,
            3 => MediaKeyStatus::Unsupported,
            _ => MediaKeyStatus::Disabled,
        }
    }
}

/// 丢弃即停：给监听线程发 `WM_QUIT`，让它跳出消息循环并 `UnregisterHotKey`。
///
/// 没有这一步的话，关闭开关后热键**仍然被本进程占用**（线程还在跑），
/// 用户会以为"关了还在抢我的媒体键"。
impl Drop for MediaKeys {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        // 线程可能刚 spawn、还没写 tid：最多等 100ms（每 5ms 看一眼）
        let mut tid = self.thread_id.load(Ordering::SeqCst);
        let mut waited = 0u32;
        while tid == 0 && waited < 20 {
            std::thread::sleep(std::time::Duration::from_millis(5));
            tid = self.thread_id.load(Ordering::SeqCst);
            waited += 1;
        }
        if tid == 0 {
            // 线程没起来（注册阶段就失败退出）→ 没有消息泵要叫醒
            return;
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_QUIT};
            // SAFETY: 仅投递一条线程消息，无所有权语义；线程已退出时调用无害
            //（返回失败，我们也不关心）。
            unsafe {
                PostThreadMessageW(tid, WM_QUIT, 0, 0);
            }
        }
    }
}

#[cfg(windows)]
fn spawn_listener(
    tx: std::sync::mpsc::Sender<MediaCommand>,
    status: std::sync::Arc<std::sync::atomic::AtomicU8>,
    thread_id: std::sync::Arc<std::sync::atomic::AtomicU32>,
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;

    // 线程创建失败时要在外层写状态，所以先留一份句柄
    let status_for_err = status.clone();
    let alive_for_err = alive.clone();
    let spawned = std::thread::Builder::new()
        .name("reimu-player-media-keys".to_string())
        .spawn(move || {
            use windows_sys::Win32::System::Threading::GetCurrentThreadId;
            use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
                RegisterHotKey, UnregisterHotKey, MOD_NOREPEAT,
            };
            use windows_sys::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

            // 先报线程 id：`Drop` 靠它唤醒消息泵（失败也要能收尾）
            // SAFETY: 无参数、无副作用，仅读当前线程 id。
            let tid = unsafe { GetCurrentThreadId() };
            thread_id.store(tid, Ordering::SeqCst);

            let mut registered = 0usize;
            for (id, vk) in HOTKEYS {
                // SAFETY: hwnd = NULL → 热键消息投递到本线程队列；
                // 所有参数都是标量，无所有权语义。
                let ok = unsafe { RegisterHotKey(std::ptr::null_mut(), id, MOD_NOREPEAT, vk) };
                if ok != 0 {
                    registered += 1;
                } else {
                    tracing::warn!("媒体键注册失败 vk=0x{vk:02X}（可能被其他程序占用）");
                }
            }
            status.store(if registered > 0 { 1 } else { 2 }, Ordering::SeqCst);
            if registered == 0 {
                tracing::warn!("全部媒体键注册失败：媒体键功能不可用");
                alive.store(false, Ordering::SeqCst);
                return;
            }
            tracing::info!("媒体键已就绪（{registered}/4）");

            let mut msg: MSG = unsafe { std::mem::zeroed() };
            loop {
                // SAFETY: 标准消息泵；hwnd=NULL 表示取本线程的队列消息。
                let ret = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
                if ret <= 0 {
                    break;
                }
                if msg.message == WM_HOTKEY {
                    if let Some(cmd) = command_for_hotkey_id(msg.wParam as i32) {
                        if tx.send(cmd).is_err() {
                            break;
                        }
                    }
                }
            }
            for (id, _) in HOTKEYS {
                // SAFETY: 与注册时同一线程、同一 hwnd(NULL)、同一 id。
                unsafe {
                    UnregisterHotKey(std::ptr::null_mut(), id);
                }
            }
            alive.store(false, Ordering::SeqCst);
            tracing::info!("媒体键已注销");
        });
    if spawned.is_err() {
        status_for_err.store(2, std::sync::atomic::Ordering::SeqCst);
        // 线程没起来，别让 Drop 等一个永远不会出现的 tid
        alive_for_err.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(not(windows))]
fn spawn_listener(
    _tx: std::sync::mpsc::Sender<MediaCommand>,
    status: std::sync::Arc<std::sync::atomic::AtomicU8>,
    _thread_id: std::sync::Arc<std::sync::atomic::AtomicU32>,
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    // 非 Windows 平台：本产品不发布，如实标记为不支持（不伪造能力）。
    status.store(3, std::sync::atomic::Ordering::SeqCst);
    alive.store(false, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_keys_map_to_commands() {
        assert_eq!(command_for_vk(0xB3), Some(MediaCommand::Toggle));
        assert_eq!(command_for_vk(0xB0), Some(MediaCommand::Next));
        assert_eq!(command_for_vk(0xB1), Some(MediaCommand::Prev));
        assert_eq!(command_for_vk(0xB2), Some(MediaCommand::Stop));
        // 普通按键不触发任何媒体动作
        assert_eq!(command_for_vk(0x41), None);
        assert_eq!(command_for_vk(0), None);
    }

    #[test]
    fn hotkey_ids_are_bijective() {
        let ids: Vec<i32> = HOTKEYS.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4]);
        for (id, vk) in HOTKEYS {
            assert_eq!(
                command_for_hotkey_id(id),
                command_for_vk(vk),
                "热键 id 与虚拟键码必须指向同一动作"
            );
        }
        assert_eq!(command_for_hotkey_id(99), None);
    }

    #[test]
    fn status_starts_unknown_but_never_panics() {
        // 不启动线程也能安全读取状态（测试环境不注册真实热键）
        let (_tx, rx) = std::sync::mpsc::channel();
        let keys = MediaKeys {
            rx,
            status: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            thread_id: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
            alive: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        assert_eq!(keys.status(), MediaKeyStatus::Starting);
        assert!(keys.poll().is_empty());
    }

    /// 丢弃 `MediaKeys` 必须真正终止消息泵线程（否则热键仍被本进程占用）。
    ///
    /// 手动跑：`cargo test -p reimuplayer --features gui -- --ignored media_keys`。
    /// 不进默认套件的原因：它会占用真实系统媒体键，与并行跑的其它用例抢注册，
    /// 结果取决于环境（别的播放器是否开着），不适合当门禁。
    #[test]
    #[ignore = "占用系统媒体键，手动验证"]
    fn dropping_media_keys_stops_the_listener_thread() {
        let keys = MediaKeys::start();
        let alive = keys.alive_flag();
        // 等注册结果落定
        for _ in 0..40 {
            if keys.status() != MediaKeyStatus::Starting {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        if keys.status() != MediaKeyStatus::Ready {
            // 环境里已有程序占着媒体键 → 本用例无从验证，直接跳过
            return;
        }
        assert!(
            alive.load(std::sync::atomic::Ordering::SeqCst),
            "线程应还在跑"
        );
        drop(keys);
        // WM_QUIT 唤醒 GetMessageW → 线程退出 → alive 置 false
        let mut stopped = false;
        for _ in 0..40 {
            if !alive.load(std::sync::atomic::Ordering::SeqCst) {
                stopped = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(stopped, "丢弃监听器后消息泵线程应自行退出并注销热键");
    }
}
