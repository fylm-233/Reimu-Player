//! 桌面歌词**接口预留**（需求④）：只定义契约与分发，不实现任何窗口。
//!
//! 为什么先做接口而不是直接做窗口：桌面歌词是"独立置顶透明窗口 + 跨屏拖动 +
//! 字体/颜色/锁定设置"的一整套子系统，它的生命周期与主窗口不同（主窗口关掉
//! 它可能还要活、或者要跟着关）。先把它压成一个**单向数据流**的契约：
//!
//! ```text
//! PlayerCore（播放时钟 + 歌词行）
//!         │  每帧一个 LyricFrame（借用，零分配）
//!         ▼
//!   DesktopLyricHub ──(门控/去重)──▶ Box<dyn DesktopLyricSink>
//! ```
//!
//! 将来实现桌面歌词窗口时，只需写一个 `impl DesktopLyricSink`
//! 并 [`DesktopLyricHub::attach`] 进来；[`crate::gui::app`] 一行都不用改。
//!
//! 现阶段 `sink` 恒为 `None`，设置面板会如实显示 `HOOK: NONE`——
//! **不假装已经实现**（规范：不伪造能力）。

/// 一帧桌面歌词快照（借用式，避免每帧为字符串分配）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LyricFrame<'a> {
    /// 曲名（无曲目时为空串）。
    pub title: &'a str,
    /// 艺人（多位已合并；未知为空串）。
    pub artist: &'a str,
    /// 当前行文本；纯音乐 / 无歌词 / 尚未到第一行为 `None`。
    pub line: Option<&'a str>,
    /// 当前行的辅助文本（译文或罗马音，按设置里的两个开关决定，见
    /// [`crate::music::lyrics::Lyrics::aux_text_for`]）。
    ///
    /// 渲染约定是**上行原文、下行辅助**：`aux` 画在 `line` 下面一行。
    /// 两个开关都关时为 `None`，此时渲染结果与加这个字段之前完全一致。
    ///
    /// 早先这里还有一个 `next`（下一行预告），阶段 22 按需求移除了：
    /// 桌面歌词只该显示"正在唱的这句"，预告下一句反而抢注意力。
    pub aux: Option<&'a str>,
    /// 当前行内进度 `0.0..=1.0`（逐字染色用；行未知时为 0）。
    pub line_progress: f32,
    /// 是否正在播放（暂停时实现方应停止逐字推进但保留文本）。
    pub playing: bool,
}

/// 桌面歌词渲染端（未来实现）。
///
/// 约定：
/// - `present` 每帧可能被调用，实现方**不要**在这里做阻塞 I/O；
/// - `shutdown` 在摘除/退出时调用一次，用于释放窗口资源。
pub trait DesktopLyricSink: Send {
    /// 呈现一帧。
    fn present(&mut self, frame: &LyricFrame<'_>);

    /// **每帧**调用一次，与 `present` 的去重无关。
    ///
    /// 为什么需要它（踩过）：eframe 的 deferred viewport 必须在**父视口的每个
    /// pass** 里被 `show_viewport_deferred` 提到一次，否则 egui 在
    /// `context.rs` 的 `is_our_child && !viewport.used` 分支里把它当"本 pass
    /// 未使用"直接销毁。而 [`DesktopLyricHub::update`] 在"暂停且文案没变"时
    /// 会去重不推送，于是暂停那一刻窗口就消失了。去重只该省内容，不该省声明。
    fn tick(&mut self) {}

    /// 开关变化通知（默认空实现）。
    ///
    /// 真实窗口实现需要它来**关闭视口**：`present` 只在启用时被调用，
    /// 关掉开关后不会再有任何回调，没有这条通知窗口会一直留在桌面上。
    fn set_enabled(&mut self, _enabled: bool) {}

    /// 界面语言变化通知（默认空实现）。
    ///
    /// 悬浮窗自带控件条（锁定 / 透明度），那上面的文案要跟随界面语言；
    /// 而 [`LyricFrame`] 里装的是**歌词内容**，与语言无关，所以单独一条通知。
    fn set_lang(&mut self, _lang: crate::i18n::Lang) {}

    /// 释放资源（默认空实现）。
    fn shutdown(&mut self) {}
}

/// 分发中心：门控 + 去重。
///
/// 去重规则（见 [`DesktopLyricHub::update`]）：文案与播放态都没变**且已暂停**时
/// 不再推送。播放中每帧都推——逐字进度本来就在变，去重没有意义。
#[derive(Default)]
pub struct DesktopLyricHub {
    enabled: bool,
    sink: Option<Box<dyn DesktopLyricSink>>,
    /// 上一次推送的去重键（`title` / `line` / `aux` / `playing`）。
    ///
    /// `aux` 必须在键里：否则暂停时打开「歌词翻译」开关，文案变了但去重键
    /// 没变，桌面歌词会一直停在旧的一行上，直到下次换行才更新。
    last: Option<(String, Option<String>, Option<String>, bool)>,
}

impl DesktopLyricHub {
    /// 新建（默认关闭、无 sink）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 开关（设置面板驱动）。
    ///
    /// 关闭时清空去重键，重新打开后第一帧必定推送——否则「关→开」后
    /// 若正好停在原来的歌词行上，实现方会一直收不到内容。
    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled != enabled {
            self.enabled = enabled;
            self.last = None;
            if let Some(sink) = self.sink.as_mut() {
                sink.set_enabled(enabled);
            }
        }
    }

    /// 当前是否启用。
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 是否已挂接实现（面板显示 `HOOK: READY` / `HOOK: NONE`）。
    pub fn has_sink(&self) -> bool {
        self.sink.is_some()
    }

    /// 挂接实现。已有实现会被先 `shutdown` 再替换。
    pub fn attach(&mut self, sink: Box<dyn DesktopLyricSink>) {
        self.detach();
        let mut sink = sink;
        // 新挂接方需要知道当前开关状态（关着的话不该开窗）
        sink.set_enabled(self.enabled);
        self.sink = Some(sink);
        // 新挂接方需要立刻拿到当前状态，不能等下一次歌词切换
        self.last = None;
    }

    /// 摘除实现（并让它清理资源）。
    pub fn detach(&mut self) {
        if let Some(mut sink) = self.sink.take() {
            sink.shutdown();
        }
        self.last = None;
    }

    /// 推送一帧。
    ///
    /// 返回 `true` 表示本帧确实分发给了实现方（测试与诊断用）。
    pub fn update(&mut self, frame: &LyricFrame<'_>) -> bool {
        if !self.enabled || self.sink.is_none() {
            // 门控关闭期间不保留去重键：重新打开要能立即推
            self.last = None;
            return false;
        }
        let key = (
            frame.title.to_string(),
            frame.line.map(str::to_string),
            frame.aux.map(str::to_string),
            frame.playing,
        );
        let unchanged = self.last.as_ref() == Some(&key);
        if unchanged && !frame.playing {
            return false;
        }
        if let Some(sink) = self.sink.as_mut() {
            sink.present(frame);
        }
        self.last = Some(key);
        true
    }

    /// 界面语言变化（转发给渲染端）。
    pub fn set_lang(&mut self, lang: crate::i18n::Lang) {
        if let Some(sink) = self.sink.as_mut() {
            sink.set_lang(lang);
        }
    }

    /// 每帧心跳（见 [`DesktopLyricSink::tick`]）。
    ///
    /// 必须在父视口**每一帧**调用：它负责让渲染端重新声明自己的视口，
    /// 否则暂停时窗口会被 egui 当成未使用的子视口销毁。
    pub fn tick(&mut self) {
        if !self.enabled {
            return;
        }
        if let Some(sink) = self.sink.as_mut() {
            sink.tick();
        }
    }
}

impl Drop for DesktopLyricHub {
    fn drop(&mut self) {
        // 进程退出也要给实现方一次清理机会（透明置顶窗口不清理会留残影）
        self.detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// 记录推送次数与最后一次文本的测试实现。
    struct Recorder {
        presents: Arc<AtomicUsize>,
        shutdowns: Arc<AtomicUsize>,
        last_line: Arc<std::sync::Mutex<Option<String>>>,
        enabled_calls: Arc<std::sync::Mutex<Vec<bool>>>,
        ticks: Arc<AtomicUsize>,
    }

    impl DesktopLyricSink for Recorder {
        fn present(&mut self, frame: &LyricFrame<'_>) {
            self.presents.fetch_add(1, Ordering::SeqCst);
            *self.last_line.lock().unwrap() = frame.line.map(str::to_string);
        }

        fn tick(&mut self) {
            self.ticks.fetch_add(1, Ordering::SeqCst);
        }

        fn set_enabled(&mut self, enabled: bool) {
            self.enabled_calls.lock().unwrap().push(enabled);
        }

        fn shutdown(&mut self) {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn recorder() -> (Box<Recorder>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let presents = Arc::new(AtomicUsize::new(0));
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let sink = Box::new(Recorder {
            presents: presents.clone(),
            shutdowns: shutdowns.clone(),
            last_line: Arc::new(std::sync::Mutex::new(None)),
            enabled_calls: Arc::new(std::sync::Mutex::new(Vec::new())),
            ticks: Arc::new(AtomicUsize::new(0)),
        });
        (sink, presents, shutdowns)
    }

    fn frame<'a>(line: Option<&'a str>, playing: bool) -> LyricFrame<'a> {
        LyricFrame {
            title: "T",
            artist: "A",
            line,
            aux: None,
            line_progress: 0.5,
            playing,
        }
    }

    #[test]
    fn disabled_hub_does_not_push() {
        let mut hub = DesktopLyricHub::new();
        let (sink, presents, _) = recorder();
        hub.attach(sink);
        // 默认关闭
        assert!(!hub.enabled());
        assert!(!hub.update(&frame(Some("第一行"), true)));
        assert_eq!(presents.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn paused_duplicate_frame_is_deduplicated() {
        let mut hub = DesktopLyricHub::new();
        hub.set_enabled(true);
        let (sink, presents, _) = recorder();
        hub.attach(sink);
        assert!(hub.update(&frame(Some("同一行"), false)));
        assert!(!hub.update(&frame(Some("同一行"), false)));
        assert_eq!(presents.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn aux_change_repushes_even_when_paused() {
        let mut hub = DesktopLyricHub::new();
        hub.set_enabled(true);
        let (sink, presents, _) = recorder();
        hub.attach(sink);
        assert!(hub.update(&frame(Some("同一行"), false)));
        // 暂停态下打开「歌词翻译」：原文没变，但辅助行变了，必须重新推
        let mut with_aux = frame(Some("同一行"), false);
        with_aux.aux = Some("译文");
        assert!(hub.update(&with_aux));
        assert_eq!(presents.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn playing_frames_are_pushed_every_tick() {
        let mut hub = DesktopLyricHub::new();
        hub.set_enabled(true);
        let (sink, presents, _) = recorder();
        hub.attach(sink);
        // 播放中：逐字进度在变，同一行也必须继续推
        assert!(hub.update(&frame(Some("同一行"), true)));
        assert!(hub.update(&frame(Some("同一行"), true)));
        assert_eq!(presents.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn toggling_off_then_on_repushes_current_line() {
        let mut hub = DesktopLyricHub::new();
        hub.set_enabled(true);
        let (sink, presents, _) = recorder();
        hub.attach(sink);
        assert!(hub.update(&frame(Some("同一行"), false)));
        hub.set_enabled(false);
        assert!(!hub.update(&frame(Some("同一行"), false)));
        hub.set_enabled(true);
        // 去重键在关闭时被清空，重开后必须重新推一次
        assert!(hub.update(&frame(Some("同一行"), false)));
        assert_eq!(presents.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn detach_shuts_down_sink() {
        let mut hub = DesktopLyricHub::new();
        let (sink, _, shutdowns) = recorder();
        hub.attach(sink);
        assert!(hub.has_sink());
        hub.detach();
        assert!(!hub.has_sink());
        assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn toggling_notifies_sink_so_window_can_close() {
        // present 只在启用时调用；关掉开关必须有一条通知，否则悬浮窗永远不关
        let enabled_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Box::new(Recorder {
            presents: Arc::new(AtomicUsize::new(0)),
            shutdowns: Arc::new(AtomicUsize::new(0)),
            last_line: Arc::new(std::sync::Mutex::new(None)),
            enabled_calls: enabled_calls.clone(),
            ticks: Arc::new(AtomicUsize::new(0)),
        });
        let mut hub = DesktopLyricHub::new();
        hub.attach(sink);
        hub.set_enabled(true);
        hub.set_enabled(false);
        assert_eq!(
            *enabled_calls.lock().unwrap(),
            vec![false, true, false],
            "挂接时同步一次当前状态，之后每次开关都要通知"
        );
        // 重复设同一值不重复通知
        hub.set_enabled(false);
        assert_eq!(enabled_calls.lock().unwrap().len(), 3);
    }

    #[test]
    fn tick_reaches_the_sink_even_when_present_is_deduplicated() {
        // 事故：暂停时去重生效 → present 不再调用 → 渲染端没机会重新声明视口 →
        // egui 把 deferred viewport 当"本 pass 未使用"销毁 → 桌面歌词窗口消失。
        // tick 必须独立于去重，每帧都到。
        let mut hub = DesktopLyricHub::new();
        hub.set_enabled(true);
        let presents = Arc::new(AtomicUsize::new(0));
        let ticks = Arc::new(AtomicUsize::new(0));
        hub.attach(Box::new(Recorder {
            presents: presents.clone(),
            shutdowns: Arc::new(AtomicUsize::new(0)),
            last_line: Arc::new(std::sync::Mutex::new(None)),
            enabled_calls: Arc::new(std::sync::Mutex::new(Vec::new())),
            ticks: ticks.clone(),
        }));

        assert!(hub.update(&frame(Some("同一行"), false)));
        assert!(
            !hub.update(&frame(Some("同一行"), false)),
            "暂停+同文案应去重"
        );
        hub.tick();
        hub.tick();
        assert_eq!(presents.load(Ordering::SeqCst), 1, "内容只推一次");
        assert_eq!(
            ticks.load(Ordering::SeqCst),
            2,
            "心跳每帧都要到，不能被去重吞掉"
        );

        // 关掉开关后心跳也要停（渲染端已经收到 set_enabled(false)，视口已关）
        hub.set_enabled(false);
        hub.tick();
        assert_eq!(ticks.load(Ordering::SeqCst), 2, "关闭后不该再心跳");
    }
}
