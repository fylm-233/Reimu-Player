//! 音频引擎抽象。
//!
//! 为什么需要这个 trait（`ENGINEERING_SPEC.md` 要求「复杂度需要理由」）：
//! 1. `DESIGN_SPEC.md` 11.1 明确把 AudioEngine 划为独立模块，11.2 要求「UI 不直接解码音频」。
//! 2. 真实音频输出依赖硬件设备；没有设备时整个播放状态机就无法被验证。
//!    有了 trait，[`crate::audio::mock_engine::MockEngine`] 可以提供确定性时钟，
//!    使控制流、状态迁移和进度计算**无需声卡即可完整测试**。
//! 3. 已存在消费者：[`crate::music::PlaybackController`] 泛型绑定该 trait。
//!
//! 若未来需要第二个引擎（例如网络流式播放），trait 也是自然落点。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// 引擎加载媒体后的权威描述。
///
/// 与 [`crate::music::MediaInfo`]（元数据视角，无设备也能取）区分：
/// 这里的 `duration` 来自**实际解码器**，是播放进度的权威总时长。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineMedia {
    /// 解码器给出的总时长；无法确定时为 `None`。
    pub duration: Option<Duration>,
}

/// 音频引擎接口。
///
/// 实现者**不得**在任何方法中执行阻塞式日志文件写入或 UI 回调
/// （`DESIGN_SPEC.md` 15.2）。
pub trait AudioEngine {
    /// 加载媒体。若已在播放，应先停止当前内容。
    fn load(&mut self, path: &Path) -> crate::error::Result<EngineMedia>;

    /// 以「边下边播的共享字节缓冲」装载媒体（方案 2A）。
    ///
    /// 与 [`load`](AudioEngine::load) 的区别只在**数据来源**：这里给的是还在长的水位，
    /// 解码搬到 [`crate::audio::stream_pump`] 的独立线程上，输出线程因此不会等网络。
    /// 调用方须保证已过起播闸门（`StreamBuffer::can_start_at`）。
    ///
    /// 默认实现退回整首装载——不需要流式路径的实现（如
    /// [`crate::audio::mock_engine::MockEngine`]）无需关心。
    fn load_stream(
        &mut self,
        buffer: Arc<crate::music::stream_buffer::StreamBuffer>,
    ) -> crate::error::Result<EngineMedia> {
        self.load(buffer.path())
    }

    /// 输出侧是否正在等字节（「缓冲中」）。
    ///
    /// 判据来自 PCM 环本身：环空了且源头还没收工。默认 `false`——
    /// 不经过环形缓冲的实现没有「等字节」这件事。
    fn is_buffering(&self) -> bool {
        false
    }

    /// 开始/继续播放。
    fn play(&mut self) -> crate::error::Result<()>;

    /// 暂停。位置保留。
    fn pause(&mut self) -> crate::error::Result<()>;

    /// 停止并回到起始位置。停止后位置归零。
    fn stop(&mut self) -> crate::error::Result<()>;

    /// 跳转到指定位置。
    fn seek(&mut self, position: Duration) -> crate::error::Result<()>;

    /// 设置音量，取值 `[0.0, 1.0]`。
    fn set_volume(&mut self, volume: f32) -> crate::error::Result<()>;

    /// 当前音量。
    fn volume(&self) -> f32;

    /// 当前播放位置。
    ///
    /// **必须**来自输出时钟/解码进度，而不是调用侧累加的定时器。
    /// 这是 `DESIGN_SPEC.md` 15.3 的硬性要求。
    fn position(&self) -> Duration;

    /// 当前加载内容的总时长（解码器权威值）。
    fn duration(&self) -> Option<Duration>;

    /// 已加载的媒体是否播放完毕。
    fn is_finished(&self) -> bool;

    /// 是否已加载媒体。
    fn has_media(&self) -> bool;

    /// 当前一帧实时频谱（[`crate::audio::spectrum::BANDS`] 段，`0.0..=1.0`）。
    ///
    /// 返回 `None` 表示「没有可用的频谱数据」：未加载、已暂停、无音频设备，
    /// 或样本尚未攒够一个分析窗。调用方应据此显示静止状态，而不是画假波形。
    ///
    /// 默认实现返回 `None`——不产生声音的引擎（如
    /// [`crate::audio::mock_engine::MockEngine`]）无需关心频谱。
    fn spectrum(&self) -> Option<crate::audio::spectrum::Bands> {
        None
    }
}
