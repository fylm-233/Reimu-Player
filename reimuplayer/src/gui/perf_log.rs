//! 帧时统计（开发期诊断，生产零开销）。
//!
//! 触发方式（不设环境变量则整段逻辑不执行，且环境变量只读一次）：
//!
//! ```text
//! REIMU_FPS_LOG=1  reimu-player.exe
//! ```
//!
//! 每 60 帧往 stderr 打一行 `avg` / `max`。定位「动画卡顿」时先看这里：
//! `max` 远大于 `avg` 说明存在单帧长任务（典型元凶是 UI 线程里做图片解码、
//! 大文件 IO、或把网络结果同步解析）。若 `avg` 本身就在 100ms 上下，
//! 则是重绘节流设置问题（见 `app::ReimuApp::needs_smooth_frames`）。
//!
//! 为什么用 `stable_dt`：它是 egui 平滑后的帧间隔，能反映真实掉帧；
//! 原始 `unstable_dt` 抖动太大，读数没有参考价值。

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;

/// 统计窗口（帧）。
const WINDOW: u32 = 60;

fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("REIMU_FPS_LOG").is_ok())
}

/// 记录一帧（`dt` 秒）。未启用时直接返回。
pub(crate) fn sample(dt: f32) {
    if !enabled() {
        return;
    }
    static N: AtomicU32 = AtomicU32::new(0);
    static SUM: AtomicU64 = AtomicU64::new(0);
    static MAX: AtomicU64 = AtomicU64::new(0);

    let us = (dt.max(0.0) * 1_000_000.0) as u64;
    SUM.fetch_add(us, Ordering::Relaxed);
    MAX.fetch_max(us, Ordering::Relaxed);
    let n = N.fetch_add(1, Ordering::Relaxed) + 1;
    if n % WINDOW == 0 {
        let sum = SUM.swap(0, Ordering::Relaxed);
        let max = MAX.swap(0, Ordering::Relaxed);
        eprintln!(
            "[PERF] frames={n} avg={:.1}ms max={:.1}ms",
            sum as f64 / WINDOW as f64 / 1000.0,
            max as f64 / 1000.0
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_by_default_does_not_panic() {
        // 未设环境变量时只是一次空调用（并且环境变量只读一次）
        if std::env::var("REIMU_FPS_LOG").is_err() {
            sample(0.016);
            sample(0.5);
        }
    }
}
