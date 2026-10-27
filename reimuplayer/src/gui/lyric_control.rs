//! 桌面歌词的**共享控制状态**：锁定 / 背景不透明度 / 进度条样式。
//!
//! 为什么单独一个模块而不是塞进 [`super::lyric_window`]：这三项状态同时被
//! **三个地方**读写——
//!
//! ```text
//!   设置面板（主视口）──写──┐
//!   歌词窗（单视口）───────写─┼──▶ LyricControl（原子量）──读──▶ 歌词窗绘制
//!   prefs.json（落盘）──────┘
//! ```
//!
//! 歌词窗只有一个视口：锁定图标、拖动、右键菜单都在同一块画布上。控制状态
//! 与主窗设置面板共享同一份原子量，两边改的永远是同一份数据。
//!
//! ## 状态机（三条铁律，全部有测试钉死）
//!
//! 1. **锁定只影响交互，不影响外观**：`set_locked` 不改透明度，也不改进度条样式。
//! 2. **透明度只影响外观，不影响锁定**：调滑杆永远不改变锁定态。
//! 3. **解锁不清空透明度**：用户调好的值要留住，重新锁定时还是那个值。
//!
//! 另有一条与"开关"相关的规则在 [`super::app`] 里：**关闭桌面歌词会把锁定复位**
//! （见 `ReimuApp::apply_settings_action`）。否则「关掉再开」会得到一个刚打开
//! 就点不动的窗口，而用户此时根本不知道"锁定"这件事还留着。

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::i18n::{pick, Lang};

/// 背景不透明度的下限（百分比）。0 = 只剩文字，背景完全透出桌面。
pub const OPACITY_MIN: u8 = 0;

/// 背景不透明度的上限（百分比）。
pub const OPACITY_MAX: u8 = 100;

/// 默认背景不透明度（百分比）：约等于旧版硬编码的 `from_black_alpha(168)`。
pub const DEFAULT_OPACITY: u8 = 66;

/// 把任意整数夹到合法区间（纯函数，滑杆与配置文件共用）。
pub fn clamp_opacity(pct: i32) -> u8 {
    pct.clamp(OPACITY_MIN as i32, OPACITY_MAX as i32) as u8
}

/// 百分比 → 0..=255 的 alpha（纯函数）。
///
/// 用整数运算：`66% → 168`，与旧版硬编码值一致，视觉上不产生"升级后变淡"。
pub fn opacity_to_alpha(pct: u8) -> u8 {
    let pct = pct.min(OPACITY_MAX) as u32;
    ((pct * 255 + 50) / 100) as u8
}

/// 行内进度条的样式。
///
/// 默认 [`Self::Line`]（细白线）。[`Self::Acid`] 原先固定用酸绿 `#D7FF35`，
/// 现在改为取当前主题的强调色（`pal().acid`）——魔理沙档是星光金，界面文案
/// 因此不能再叫「绿条」，统一称「彩色」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProgressStyle {
    /// 细白线（默认）。
    #[default]
    Line,
    /// 不显示进度条。
    Off,
    /// 主题强调色条（旧版是酸绿；现在跟随主题）。
    Acid,
}

impl ProgressStyle {
    /// 全部样式（按钮循环顺序）。
    pub const ALL: [Self; 3] = [Self::Line, Self::Off, Self::Acid];

    /// 落盘/日志用的稳定代码（与界面语言无关）。
    pub fn code(self) -> &'static str {
        match self {
            Self::Line => "line",
            Self::Off => "off",
            Self::Acid => "acid",
        }
    }

    /// 代码 → 样式；未知代码返回 `None`（调用方回默认）。
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|s| s.code().eq_ignore_ascii_case(code))
    }

    /// 循环到下一个样式（按钮点一次换一个）。
    pub fn next(self) -> Self {
        match self {
            Self::Line => Self::Off,
            Self::Off => Self::Acid,
            Self::Acid => Self::Line,
        }
    }

    /// 按钮上的短标签（写**当前值**，不是动作）。
    pub fn label(self, lang: Lang) -> &'static str {
        match self {
            Self::Line => pick(lang, "白线", "LINE"),
            Self::Off => pick(lang, "关", "OFF"),
            Self::Acid => pick(lang, "彩色", "COLOR"),
        }
    }

    /// 悬停说明。
    pub fn hint(self, lang: Lang) -> &'static str {
        match self {
            Self::Line => pick(
                lang,
                "细白线进度（默认）",
                "Thin white progress line (default)",
            ),
            Self::Off => pick(lang, "不显示进度条", "No progress bar"),
            Self::Acid => pick(
                lang,
                "彩色进度条（跟随当前主题）",
                "Accent progress bar (follows the active theme)",
            ),
        }
    }
}

/// 三个入口共享的控制状态（全原子量：读侧每帧都要读，不能上锁）。
#[derive(Debug)]
pub struct LyricControl {
    locked: AtomicBool,
    opacity: AtomicU8,
    progress: AtomicU8,
}

impl Default for LyricControl {
    fn default() -> Self {
        Self::new()
    }
}

impl LyricControl {
    /// 未锁定 + 默认透明度 + 白线进度条。
    pub fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            opacity: AtomicU8::new(DEFAULT_OPACITY),
            progress: AtomicU8::new(ProgressStyle::default() as u8),
        }
    }

    /// 从落盘值恢复（`None` / 越界 → 默认值）。
    pub fn restore(opacity: u8, progress: ProgressStyle) -> Self {
        Self {
            locked: AtomicBool::new(false),
            opacity: AtomicU8::new(clamp_opacity(opacity as i32)),
            progress: AtomicU8::new(progress as u8),
        }
    }

    // ---- 锁定 ----

    pub fn locked(&self) -> bool {
        self.locked.load(Ordering::SeqCst)
    }

    /// 设置锁定态，返回**是否真的发生了变化**（调用方据此决定要不要提示）。
    pub fn set_locked(&self, on: bool) -> bool {
        self.locked.swap(on, Ordering::SeqCst) != on
    }

    /// 翻转锁定态，返回翻转后的值。
    pub fn toggle_lock(&self) -> bool {
        let next = !self.locked();
        self.set_locked(next);
        next
    }

    // ---- 背景不透明度 ----

    pub fn opacity(&self) -> u8 {
        clamp_opacity(self.opacity.load(Ordering::SeqCst) as i32)
    }

    /// 设置不透明度（自动夹取），返回夹取后的值。
    ///
    /// **不动锁定态**：调滑块不是"解锁"，也不是"上锁"。
    pub fn set_opacity(&self, pct: u8) -> u8 {
        let v = clamp_opacity(pct as i32);
        self.opacity.store(v, Ordering::SeqCst);
        v
    }

    /// 显示用的 alpha（0..=255）。
    pub fn backdrop_alpha(&self) -> u8 {
        opacity_to_alpha(self.opacity())
    }

    // ---- 进度条样式 ----

    pub fn progress(&self) -> ProgressStyle {
        // 原子量里存的是 `as u8`；越界（理论上不会）回默认，绝不 panic
        match self.progress.load(Ordering::SeqCst) {
            0 => ProgressStyle::Line,
            1 => ProgressStyle::Off,
            2 => ProgressStyle::Acid,
            _ => ProgressStyle::default(),
        }
    }

    pub fn set_progress(&self, style: ProgressStyle) {
        self.progress.store(style as u8, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_fresh_control_is_unlocked_with_the_legacy_look() {
        let c = LyricControl::new();
        assert!(!c.locked(), "默认必须未锁定：否则一开就点不动");
        assert_eq!(c.opacity(), DEFAULT_OPACITY);
        // 旧版硬编码 168/255 ≈ 66%，升级后视觉不能跳变
        assert_eq!(c.backdrop_alpha(), 168);
        assert_eq!(
            c.progress(),
            ProgressStyle::Line,
            "彩色进度条不再是默认样式"
        );
    }

    #[test]
    fn opacity_is_clamped_and_never_touches_the_lock() {
        let c = LyricControl::new();
        assert_eq!(c.set_opacity(255), OPACITY_MAX);
        assert_eq!(c.set_opacity(0), OPACITY_MIN);
        assert!(!c.locked(), "调透明度把锁定态改了");
        c.set_locked(true);
        assert_eq!(c.set_opacity(30), 30);
        assert!(c.locked(), "调透明度把锁定态改了");
    }

    #[test]
    fn unlocking_keeps_the_opacity_the_user_picked() {
        // 铁律 3：解锁不清空透明度——重新锁定时还是那个值
        let c = LyricControl::new();
        c.set_locked(true);
        c.set_opacity(20);
        c.set_locked(false);
        assert_eq!(c.opacity(), 20, "解锁把透明度重置了");
        c.set_locked(true);
        assert_eq!(c.opacity(), 20, "重新锁定后透明度丢了");
    }

    #[test]
    fn locking_never_changes_the_look() {
        // 铁律 1：锁定只影响"能不能点"
        let c = LyricControl::new();
        c.set_opacity(45);
        c.set_progress(ProgressStyle::Acid);
        c.set_locked(true);
        assert_eq!(c.opacity(), 45);
        assert_eq!(c.progress(), ProgressStyle::Acid);
    }

    #[test]
    fn set_locked_reports_only_real_transitions() {
        // 调用方靠返回值决定要不要弹提示：重复设同值不该再提示一次
        let c = LyricControl::new();
        assert!(c.set_locked(true), "false→true 应报告变化");
        assert!(!c.set_locked(true), "true→true 不该报告变化");
        assert!(c.set_locked(false));
        assert!(!c.set_locked(false));
    }

    #[test]
    fn toggle_flips_and_returns_the_new_value() {
        let c = LyricControl::new();
        assert!(c.toggle_lock());
        assert!(c.locked());
        assert!(!c.toggle_lock());
        assert!(!c.locked());
    }

    #[test]
    fn alpha_endpoints_are_exact() {
        assert_eq!(opacity_to_alpha(0), 0);
        assert_eq!(opacity_to_alpha(100), 255);
        assert_eq!(opacity_to_alpha(50), 128); // 四舍五入，不截断
        assert_eq!(opacity_to_alpha(200), 255, "越界值按上限处理，不得回绕");
    }

    #[test]
    fn progress_cycles_through_every_style_and_never_sticks() {
        // 按钮是"循环切换"，必须能从任意一档走遍三档回到原点
        let mut style = ProgressStyle::default();
        let mut seen = Vec::new();
        for _ in 0..3 {
            seen.push(style);
            style = style.next();
        }
        assert_eq!(style, ProgressStyle::default(), "三次之后应回到起点");
        let mut codes: Vec<&str> = seen.iter().map(|s| s.code()).collect();
        codes.sort_unstable();
        assert_eq!(codes, vec!["acid", "line", "off"], "三档必须各不相同");
    }

    #[test]
    fn progress_codes_round_trip_case_insensitively() {
        for style in ProgressStyle::ALL {
            assert_eq!(ProgressStyle::from_code(style.code()), Some(style));
            assert_eq!(
                ProgressStyle::from_code(&style.code().to_uppercase()),
                Some(style),
                "手改配置文件写成大写也不该回默认"
            );
        }
        assert_eq!(ProgressStyle::from_code("rainbow"), None);
    }

    #[test]
    fn restore_clamps_a_corrupted_opacity() {
        // 手改 prefs.json 写成 200 也不该让底板变成"不透明到看不见"
        let c = LyricControl::restore(200, ProgressStyle::Acid);
        assert_eq!(c.opacity(), OPACITY_MAX);
        assert_eq!(c.progress(), ProgressStyle::Acid);
        assert!(!c.locked(), "落盘的锁定态一律不恢复：开机必须能点");
    }

    #[test]
    fn shared_across_threads_like_the_viewport_does() {
        // 歌词窗回调与主窗设置面板是两个线程/回调，中间就是这么一个 Arc
        let c = Arc::new(LyricControl::new());
        let other = c.clone();
        let handle = std::thread::spawn(move || {
            other.set_locked(true);
            other.set_opacity(12);
        });
        handle.join().unwrap();
        assert!(c.locked());
        assert_eq!(c.opacity(), 12);
    }
}
