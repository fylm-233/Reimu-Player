//! 确定性音频引擎实现，用于测试与无声卡环境下的验证。
//!
//! 时钟由 [`MockEngine::advance`] 显式推进，**完全确定**，
//! 因此状态机与进度计算可以被精确断言，不受真实音频设备抖动影响。

use std::path::Path;
use std::time::Duration;

use super::engine::{AudioEngine, EngineMedia};
use crate::error::{PlayerError, Result};

/// 引擎行为记录，供测试断言「到底调用了什么」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineCall {
    Load(String),
    Play,
    Pause,
    Stop,
    Seek(Duration),
    SetVolume(u32), // volume * 1000，避免 f32 比较
}
/// 确定性测试引擎。
pub struct MockEngine {
    /// 已加载的媒体路径；`None` 表示未加载。
    loaded: Option<String>,
    duration: Option<Duration>,
    position: Duration,
    playing: bool,
    finished: bool,
    volume: f32,
    /// 调用日志，测试用。
    calls: Vec<EngineCall>,
    /// 若置位，则下一个指定操作会失败，用于测试错误路径。
    fail_next: Option<&'static str>,
}

impl Default for MockEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl MockEngine {
    pub fn new() -> Self {
        Self {
            loaded: None,
            duration: None,
            position: Duration::ZERO,
            playing: false,
            finished: false,
            volume: 1.0,
            calls: Vec::new(),
            fail_next: None,
        }
    }

    /// 让下一个 `op` 操作返回 [`PlayerError::OutputError`]。
    pub fn fail_next(&mut self, op: &'static str) {
        self.fail_next = Some(op);
    }

    /// 覆盖加载后的总时长（用于测试未知时长场景）。
    pub fn with_duration(mut self, duration: Option<Duration>) -> Self {
        self.duration = duration;
        self
    }

    /// 手动推进播放时钟；超过总时长时标记为播放结束。
    ///
    /// 这是「输出时钟」的确定性替身，不是定时器累加——测试显式决定时间。
    pub fn advance(&mut self, delta: Duration) {
        if !self.playing {
            return;
        }
        self.position = self.position.saturating_add(delta);
        if let Some(total) = self.duration {
            if self.position >= total {
                self.position = total;
                self.playing = false;
                self.finished = true;
            }
        }
    }

    /// 已发生的引擎调用，供测试断言。
    pub fn calls(&self) -> &[EngineCall] {
        &self.calls
    }

    fn check_fail(&mut self, op: &'static str) -> Result<()> {
        if self.fail_next == Some(op) {
            self.fail_next = None;
            return Err(PlayerError::OutputError(format!("模拟 {} 失败", op)));
        }
        Ok(())
    }
}

impl AudioEngine for MockEngine {
    fn load(&mut self, path: &Path) -> Result<EngineMedia> {
        self.check_fail("load")?;
        let display = path.to_string_lossy().into_owned();
        self.calls.push(EngineCall::Load(display.clone()));
        self.loaded = Some(display);
        self.position = Duration::ZERO;
        self.playing = false;
        self.finished = false;
        // 时长不由 load 隐式填充：`with_duration` 未指定时保持 `None`，
        // 以便测试「总时长未知」这条真实路径。
        Ok(EngineMedia {
            duration: self.duration,
        })
    }

    fn play(&mut self) -> Result<()> {
        self.check_fail("play")?;
        if self.loaded.is_none() {
            return Err(PlayerError::InvalidState {
                state: "Idle".into(),
                action: "play".into(),
            });
        }
        self.calls.push(EngineCall::Play);
        if self.finished {
            // 播放结束后再 play 视为重新开始
            self.position = Duration::ZERO;
            self.finished = false;
        }
        self.playing = true;
        Ok(())
    }

    fn pause(&mut self) -> Result<()> {
        self.check_fail("pause")?;
        if !self.playing {
            return Err(PlayerError::InvalidState {
                state: if self.loaded.is_some() {
                    "Paused"
                } else {
                    "Idle"
                }
                .into(),
                action: "pause".into(),
            });
        }
        self.calls.push(EngineCall::Pause);
        self.playing = false;
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.check_fail("stop")?;
        self.calls.push(EngineCall::Stop);
        self.playing = false;
        self.position = Duration::ZERO;
        self.finished = false;
        Ok(())
    }

    fn seek(&mut self, position: Duration) -> Result<()> {
        self.check_fail("seek")?;
        if self.loaded.is_none() {
            return Err(PlayerError::InvalidState {
                state: "Idle".into(),
                action: "seek".into(),
            });
        }
        if let Some(total) = self.duration {
            if position > total {
                return Err(PlayerError::InvalidPosition(format!(
                    "{} 超出总时长 {}",
                    crate::music::models::format_duration(position),
                    crate::music::models::format_duration(total)
                )));
            }
        }
        self.calls.push(EngineCall::Seek(position));
        self.position = position;
        self.finished = self.duration.is_some_and(|t| position >= t);
        Ok(())
    }

    fn set_volume(&mut self, volume: f32) -> Result<()> {
        self.check_fail("set_volume")?;
        if !(0.0..=1.0).contains(&volume) {
            return Err(PlayerError::InvalidVolume(volume));
        }
        self.calls
            .push(EngineCall::SetVolume((volume * 1000.0) as u32));
        self.volume = volume;
        Ok(())
    }

    fn volume(&self) -> f32 {
        self.volume
    }

    fn position(&self) -> Duration {
        self.position
    }

    fn duration(&self) -> Option<Duration> {
        self.duration
    }

    fn is_finished(&self) -> bool {
        self.finished
    }

    fn has_media(&self) -> bool {
        self.loaded.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn advance_moves_clock_only_while_playing() {
        let mut e = MockEngine::new();
        e.load(Path::new("a.mp3")).unwrap();
        e.advance(Duration::from_secs(10));
        assert_eq!(e.position(), Duration::ZERO, "未播放时时钟不动");

        e.play().unwrap();
        e.advance(Duration::from_secs(10));
        assert_eq!(e.position(), Duration::from_secs(10));

        e.pause().unwrap();
        e.advance(Duration::from_secs(10));
        assert_eq!(e.position(), Duration::from_secs(10), "暂停时时钟不动");
    }

    #[test]
    fn reaching_total_marks_finished() {
        let mut e = MockEngine::new().with_duration(Some(Duration::from_secs(30)));
        e.load(Path::new("a.mp3")).unwrap();
        e.play().unwrap();
        e.advance(Duration::from_secs(40));
        assert!(e.is_finished());
        assert_eq!(e.position(), Duration::from_secs(30), "位置不越过总时长");
    }

    #[test]
    fn play_without_media_is_invalid_state() {
        let mut e = MockEngine::new();
        let err = e.play().unwrap_err();
        assert!(matches!(err, PlayerError::InvalidState { .. }));
    }

    #[test]
    fn seek_beyond_duration_rejected() {
        let mut e = MockEngine::new().with_duration(Some(Duration::from_secs(30)));
        e.load(Path::new("a.mp3")).unwrap();
        let err = e.seek(Duration::from_secs(60)).unwrap_err();
        assert!(matches!(err, PlayerError::InvalidPosition(_)));
    }

    #[test]
    fn volume_range_enforced() {
        let mut e = MockEngine::new();
        assert!(matches!(
            e.set_volume(1.5).unwrap_err(),
            PlayerError::InvalidVolume(_)
        ));
        assert!(matches!(
            e.set_volume(-0.1).unwrap_err(),
            PlayerError::InvalidVolume(_)
        ));
        e.set_volume(0.5).unwrap();
        assert_eq!(e.volume(), 0.5);
    }

    #[test]
    fn fail_next_injects_error_once() {
        let mut e = MockEngine::new();
        e.load(Path::new("a.mp3")).unwrap();
        e.fail_next("play");
        assert!(e.play().is_err());
        assert!(e.play().is_ok(), "失败只注入一次");
    }

    #[test]
    fn call_log_records_operations() {
        let mut e = MockEngine::new();
        e.load(&PathBuf::from("x.mp3")).unwrap();
        e.play().unwrap();
        e.set_volume(0.25).unwrap();
        assert_eq!(
            e.calls(),
            &[
                EngineCall::Load("x.mp3".into()),
                EngineCall::Play,
                EngineCall::SetVolume(250)
            ]
        );
    }
}
