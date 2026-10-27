//! 磁带机械动作时间线（`code (3).html` openTray / ejectTape / insertTape /
//! closeTray / boot 的 JS 状态机移植）。
//!
//! 纯逻辑、无 UI 依赖：阶段推进输出 [`DeckFrame`]（舱盖开度 / 装带进度 /
//! 落座轻震 / 合舱咔哒偏移 / DECK 状态文案），[`super::app`] 只消费帧数据。
//!
//! 时间线（与 HTML `wait()` 时序一致）：
//!
//! ```text
//! boot:        TRAY OPEN(300) → INSERTING(500) → 落座(170) → CLOSING(360) → 咔哒(180)
//! load(换带):  TRAY OPEN(300) → EJECTING(300) → INSERTING(500) → 落座(170) → CLOSING(360) → 咔哒(180)
//! load(首次):  同 boot
//! ```

/// 落座轻震峰值（px，HTML `@keyframes seat` 42% 处 2.5px）。
pub const KICK_SEAT: f32 = 2.5;

/// 合舱咔哒峰值（px，HTML `@keyframes clunk` 32% 处 2.5px / 64% 处 -1px）。
pub const KICK_CLUNK_MAX: f32 = 2.5;
pub const KICK_CLUNK_MIN: f32 = -1.0;

// ---------------------------------------------------------------------------
// 关键帧插值（纯函数）
// ---------------------------------------------------------------------------

/// 折线关键帧插值：`pts` 为 (u, y) 且 u 递增，u∈[0,1]。
fn piecewise(u: f32, pts: &[(f32, f32)]) -> f32 {
    let u = u.clamp(0.0, 1.0);
    for w in pts.windows(2) {
        let (x0, y0) = w[0];
        let (x1, y1) = w[1];
        if u <= x1 {
            let f = if x1 > x0 { (u - x0) / (x1 - x0) } else { 1.0 };
            return y0 + (y1 - y0) * f;
        }
    }
    pts[pts.len() - 1].1
}

/// 落座轻震（HTML `@keyframes seat`：0%→0，42%→2.5px，100%→0）。
pub fn seat_kick(u: f32) -> f32 {
    piecewise(u, &[(0.0, 0.0), (0.42, KICK_SEAT), (1.0, 0.0)])
}

/// 合舱咔哒（HTML `@keyframes clunk`：0%→0，32%→2.5px，64%→-1px，100%→0）。
pub fn clunk_kick(u: f32) -> f32 {
    piecewise(
        u,
        &[
            (0.0, 0.0),
            (0.32, KICK_CLUNK_MAX),
            (0.64, KICK_CLUNK_MIN),
            (1.0, 0.0),
        ],
    )
}

// ---------------------------------------------------------------------------
// 阶段状态机
// ---------------------------------------------------------------------------

/// 机械动作阶段。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeckStage {
    /// 静止（保持当前舱盖/卡带位置）。
    Idle,
    /// 开舱（舱盖 -66°，300ms）。
    Open,
    /// 退带（卡带退出，300ms，仅换带时）。
    Eject,
    /// 装带（卡带入舱 500ms，HTML entering→transition 落位）。
    Enter,
    /// 落座轻震（170ms，HTML seat keyframes）。
    Seat,
    /// 合舱（360ms）。
    Close,
    /// 咔哒（180ms，HTML clunk keyframes）。
    Clunk,
}

impl DeckStage {
    /// 阶段时长（秒，与 HTML 毫秒时序一致）。
    pub fn duration(self) -> f32 {
        match self {
            DeckStage::Idle => 0.0,
            DeckStage::Open => 0.30,
            DeckStage::Eject => 0.30,
            DeckStage::Enter => 0.50,
            DeckStage::Seat => 0.17,
            DeckStage::Close => 0.36,
            DeckStage::Clunk => 0.18,
        }
    }

    /// DECK 状态文案（HTML `deckStatus`；Idle 无覆盖）。
    pub fn status(self) -> Option<&'static str> {
        match self {
            DeckStage::Idle => None,
            DeckStage::Open => Some("DECK 01 / TRAY OPEN"),
            DeckStage::Eject => Some("DECK 01 / EJECTING"),
            DeckStage::Enter | DeckStage::Seat => Some("DECK 01 / INSERTING"),
            DeckStage::Close | DeckStage::Clunk => Some("DECK 01 / CLOSING"),
        }
    }
}

/// 一帧机械输出（UI 消费）。
#[derive(Clone, Copy, Debug, Default)]
pub struct DeckFrame {
    /// 舱盖开度 0（合）..=1（开）。
    pub open: f32,
    /// 装带进度 0（舱外）..=1（落座）。
    pub insert: f32,
    /// 落座轻震 y 偏移（px，作用于卡带）。
    pub seat_dy: f32,
    /// 合舱咔哒 y 偏移（px，作用于舱体 well）。
    pub well_dy: f32,
    /// 阶段文案覆盖（Idle 时 None → 用内核 deck_status）。
    pub status: Option<&'static str>,
    /// 本轮时间线恰好结束的帧（boot 用 toast）。
    pub just_finished: bool,
}

/// 机械时间线状态机。
#[derive(Clone, Debug)]
pub struct DeckSeq {
    stage: DeckStage,
    /// 阶段内已过时间（秒）。
    t: f32,
    /// 本轮是否 boot（结束时 toast PLAYBACK READY）。
    boot_mode: bool,
    /// 启动开舱前是否已有磁带（决定 Eject 阶段）。
    had_tape: bool,
    open: f32,
    insert: f32,
}

impl Default for DeckSeq {
    fn default() -> Self {
        Self {
            stage: DeckStage::Idle,
            t: 0.0,
            boot_mode: false,
            had_tape: false,
            open: 0.0,
            insert: 0.0,
        }
    }
}

impl DeckSeq {
    /// 开机装带（HTML `boot()`：开舱→装带→落座→合舱→咔哒，结束 toast）。
    pub fn start_boot(&mut self) {
        self.begin(false, true);
    }

    /// 装载一盘新带（HTML `loadTape(withEject)`）。
    pub fn start_load(&mut self, had_tape: bool) {
        self.begin(had_tape, false);
    }

    fn begin(&mut self, had_tape: bool, boot: bool) {
        self.stage = DeckStage::Open;
        self.t = 0.0;
        self.boot_mode = boot;
        self.had_tape = had_tape;
    }

    /// 当前阶段（测试/调试用）。
    pub fn stage(&self) -> DeckStage {
        self.stage
    }

    /// 当前阶段文案覆盖。
    pub fn status(&self) -> Option<&'static str> {
        self.stage.status()
    }

    /// 是否正在机械动作中。
    pub fn busy(&self) -> bool {
        self.stage != DeckStage::Idle
    }

    /// 推进一帧，输出机械位置。
    pub fn tick(&mut self, dt: f32) -> DeckFrame {
        if self.stage == DeckStage::Idle {
            return DeckFrame {
                open: self.open,
                insert: self.insert,
                ..DeckFrame::default()
            };
        }
        self.t += dt.max(0.0);
        let dur = self.stage.duration();
        let u = if dur > 0.0 {
            (self.t / dur).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let mut frame = DeckFrame {
            open: self.open,
            insert: self.insert,
            status: self.stage.status(),
            ..DeckFrame::default()
        };
        match self.stage {
            DeckStage::Idle => {}
            DeckStage::Open => {
                self.open = u;
                frame.open = self.open;
            }
            DeckStage::Eject => {
                self.insert = 1.0 - u;
                frame.insert = self.insert;
            }
            DeckStage::Enter => {
                self.insert = u;
                frame.insert = self.insert;
            }
            DeckStage::Seat => {
                frame.seat_dy = seat_kick(u);
            }
            DeckStage::Close => {
                self.open = 1.0 - u;
                frame.open = self.open;
            }
            DeckStage::Clunk => {
                frame.well_dy = clunk_kick(u);
            }
        }
        if u >= 1.0 {
            let finished = self.stage;
            self.stage = self.next_after(finished);
            self.t = 0.0;
            if self.stage == DeckStage::Idle {
                frame.just_finished = true;
            }
        }
        frame
    }

    /// 阶段推进：Open → [Eject] → Enter → Seat → Close → Clunk → Idle。
    fn next_after(&self, stage: DeckStage) -> DeckStage {
        match stage {
            DeckStage::Open => {
                if self.had_tape {
                    DeckStage::Eject
                } else {
                    DeckStage::Enter
                }
            }
            DeckStage::Eject => DeckStage::Enter,
            DeckStage::Enter => DeckStage::Seat,
            DeckStage::Seat => DeckStage::Close,
            DeckStage::Close => DeckStage::Clunk,
            DeckStage::Clunk | DeckStage::Idle => DeckStage::Idle,
        }
    }

    /// boot 模式（结束 toast 用）。
    pub fn is_boot(&self) -> bool {
        self.boot_mode
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyframes_match_html() {
        assert_eq!(seat_kick(0.0), 0.0);
        assert!((seat_kick(0.42) - KICK_SEAT).abs() < 1e-6);
        assert_eq!(seat_kick(1.0), 0.0);
        assert_eq!(clunk_kick(0.0), 0.0);
        assert!((clunk_kick(0.32) - KICK_CLUNK_MAX).abs() < 1e-6);
        assert!((clunk_kick(0.64) - KICK_CLUNK_MIN).abs() < 1e-6);
        assert_eq!(clunk_kick(1.0), 0.0);
        // 越界钳制
        assert_eq!(seat_kick(-1.0), 0.0);
        assert_eq!(clunk_kick(2.0), 0.0);
    }

    #[test]
    fn boot_sequence_follows_html_timeline() {
        let mut seq = DeckSeq::default();
        seq.start_boot();
        assert!(seq.is_boot());
        let mut stages = vec![seq.stage()];
        // 用略大于各阶段时长的步长推进，收集阶段序列
        for _ in 0..400 {
            let frame = seq.tick(0.05);
            if seq.stage() != *stages.last().unwrap() {
                stages.push(seq.stage());
            }
            if frame.just_finished {
                break;
            }
        }
        assert_eq!(
            stages,
            vec![
                DeckStage::Open,
                DeckStage::Enter,
                DeckStage::Seat,
                DeckStage::Close,
                DeckStage::Clunk,
                DeckStage::Idle,
            ]
        );
    }

    #[test]
    fn load_with_eject_inserts_eject_stage() {
        let mut seq = DeckSeq::default();
        seq.start_load(true);
        let mut saw_eject = false;
        for _ in 0..400 {
            seq.tick(0.05);
            if seq.stage() == DeckStage::Eject {
                saw_eject = true;
            }
        }
        assert!(saw_eject, "换带必须有退带阶段");
        assert!(!seq.busy());
    }

    #[test]
    fn load_without_eject_skips_eject() {
        let mut seq = DeckSeq::default();
        seq.start_load(false);
        let mut saw_eject = false;
        for _ in 0..400 {
            seq.tick(0.05);
            if seq.stage() == DeckStage::Eject {
                saw_eject = true;
            }
        }
        assert!(!saw_eject);
    }

    #[test]
    fn frame_endpoints_are_consistent() {
        let mut seq = DeckSeq::default();
        seq.start_boot();
        // Open 结束后 open≈1
        let mut f = DeckFrame::default();
        for _ in 0..10 {
            f = seq.tick(0.05);
        }
        assert!((f.open - 1.0).abs() < 1e-6);
        // 捕获结束帧：just_finished 仅该帧为真，此时 open 合上、带已落座
        let mut finished = None;
        for _ in 0..100 {
            f = seq.tick(0.05);
            if f.just_finished {
                finished = Some(f);
                break;
            }
        }
        let f = finished.expect("boot 时间线必须在步进预算内完成");
        assert!((f.open).abs() < 1e-6);
        assert!((f.insert - 1.0).abs() < 1e-6);
        // Idle 保持
        let f2 = seq.tick(0.05);
        assert!(!f2.just_finished);
        assert_eq!(f2.status, None);
    }

    #[test]
    fn idle_seq_holds_position() {
        let mut seq = DeckSeq::default();
        let f = seq.tick(0.016);
        assert_eq!((f.open, f.insert), (0.0, 0.0));
        assert!(!f.just_finished);
    }
}
