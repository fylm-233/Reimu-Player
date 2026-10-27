//! # reimuplayer
//!
//! System 6.x 风格音乐播放器核心库。
//!
//! ## 模块地图
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`error`] | 统一错误类型，第三方错误不外泄 |
//! | [`logging`] | 日志初始化（不在音频回调中写日志） |
//! | [`music`] | 领域模型、媒体加载、播放控制器、Provider 契约 |
//! | [`audio`] | 音频引擎抽象与实现（rodio / mock） |
//! | [`providers`] | 具体在线 Provider（网易云） |
//!
//! ## 分层规则
//!
//! ```text
//!   UI / CLI
//!      │
//!      ▼
//!   music::PlaybackController   ← 唯一控制入口
//!      │              │
//!      ▼              ▼
//!   audio::AudioEngine   music::MusicProvider
//!      │                      │
//!      ▼                      ▼
//!   rodio / mock         providers::NeteaseProvider
//! ```
//!
//! 依据 `NETEASE_INTEGRATION_SPEC.md` 3.2：
//! - UI 不直接调用第三方 API。
//! - 不依赖第三方原始 JSON。
//! - 网络错误转换为项目内部错误类型。
//! - 播放器核心不依赖 Node.js。
//!
//! ## 验证能力
//!
//! [`audio::MockEngine`] 提供确定性时钟，使播放状态机与进度计算
//! **无需音频输出设备即可完整测试**；[`audio::RodioEngine`] 提供真实输出。

/// 专辑取色自适应主题（纯计算层：OKLab/OKLCH 色彩学、色板推导、取色、过渡）。
///
/// 零第三方依赖、不 import 任何业务/UI 类型、不启动线程、不触文件系统/网络。
/// 本模块刻意与 UI 框架解耦：只做纯计算，不依赖 egui。
pub mod album_theme;
pub mod audio;
pub mod error;
pub mod i18n;
pub mod logging;
pub mod music;
pub mod providers;

/// 正式软件名。**中英文一律用这个写法**（不做本地化翻译），窗口标题、托盘提示、
/// 桌面歌词窗口、媒体会话、README 里出现的品牌词全部引用此处，避免改名时漏掉一处。
pub const BRAND: &str = "Reimu Player";

/// GUI 测试程序（阶段 E，`--features gui`）：Reimu Player 窗口 UI 视觉规范验证载体。
#[cfg(feature = "gui")]
pub mod gui;

// 常用项提升到 crate 根，方便调用方
pub use audio::{AudioEngine, MockEngine, RodioEngine};
pub use error::{PlayerError, Result};
pub use music::{
    load_media, parse_lrc, parse_yrc, AudioFetcher, AudioQuality, BoxFuture, DynAudioFetcher,
    DynMusicProvider, DynProviderShim, FetchPhase, LoadState, LyricKind, LyricLine, Lyrics,
    MediaInfo, MusicProvider, OnlineTrackLoader, PlayQueue, PlaySession, PlaybackController,
    PlaybackSource, PlaybackState, Playlist, PreparedTrack, Progress, ProviderError, ProviderId,
    QueueError, RetryPolicy, Track, TrackAvailability, DEFAULT_MAX_QUEUE_LEN,
};
pub use providers::HttpAudioFetcher;
