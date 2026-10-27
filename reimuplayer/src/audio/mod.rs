//! 音频子系统：引擎抽象与具体实现。
//!
//! 分层：
//! - [`engine`]：[`AudioEngine`] trait，播放控制的唯一契约。
//! - [`rodio_engine`]：真实实现，依赖音频输出设备。
//! - [`mock_engine`]：确定性实现，供测试与无声卡环境验证。
//! - [`spectrum`]：实时频谱分析（手写 FFT），为 EQ 面板提供真实数据。
//! - [`stream_pump`]：流式播放的解码泵线程 + 有界 PCM 环（输出线程永不等待网络）。
//!
//! **音频回调约束**（`DESIGN_SPEC.md` 15.2）：本模块任何代码都不得
//! 执行文件读取、阻塞锁等待、日志文件写入或 UI 事件调用。
//! [`spectrum::SpectrumShared`] 的音频侧写入用 `try_lock`，抢不到即丢样本。
//! 这条约束正是 [`stream_pump`] 存在的原因：边下边播的解码必须搬去独立线程。

pub mod engine;
pub mod mock_engine;
pub mod rodio_engine;
pub mod spectrum;
pub mod stream_pump;

pub use engine::{AudioEngine, EngineMedia};
pub use mock_engine::MockEngine;
pub use rodio_engine::RodioEngine;
pub use spectrum::{Bands, BANDS, FFT_SIZE};
pub use stream_pump::{PcmSource, StreamPump};
