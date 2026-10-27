//! 音乐领域层：模型、媒体加载、播放控制与在线 Provider 抽象。
//!
//! 分工：
//! - [`models`]：领域模型（媒体描述、播放状态、进度）。
//! - [`lyrics`]：歌词模型与容错解析（普通/逐字分级、时间戳容错、播放位置同步查询）。
//! - [`load_state`]：异步数据加载状态机（加载中/已加载/加载失败，规范 8.2/9.2）。
//! - [`media`]：媒体加载（文件探测 + 元数据），**不需要音频设备**。
//! - [`playback_controller`]：播放状态机与控制入口，**UI 唯一交互对象**。
//! - [`provider`]：在线音乐 Provider 契约与统一歌曲/歌单/播放地址模型。
//! - [`queue`]：有界播放队列（容量上限、只存元数据，规范 9.2）。
//! - [`retry`]：有限重试策略（规范 8.3）。
//! - [`stream_buffer`]：边下边播的共享字节缓冲（写入者推进水位、读取者越线即等）。
//! - [`stream_transport`]：边下边播的传输任务（自带线程的写侧 + 断线换新地址续传）。
//!
//! 依赖方向约束（`NETEASE_INTEGRATION_SPEC.md` 3.2）：
//! `music` 不依赖任何具体第三方实现；具体实现放在 [`crate::providers`]。

pub mod link;
pub mod load_state;
pub mod lyrics;
pub mod media;
pub mod models;
pub mod online_playback;
pub mod play_session;
pub mod playback_controller;
pub mod provider;
pub mod queue;
pub mod retry;
pub mod stream_buffer;
pub mod stream_transport;

pub use link::{parse_share, ShareKind, ShareRef};
pub use load_state::LoadState;
pub use lyrics::{parse_lrc, parse_yrc, LyricKind, LyricLine, Lyrics};
pub use media::load_media;
pub use models::{AudioFormat, MediaInfo, PlaybackState, Progress, Tags};
pub use online_playback::{
    AudioFetcher, AudioKind, DynAudioFetcher, DynProviderShim, OnlineTrackLoader, PreparedTrack,
};
pub use play_session::{FetchPhase, PlaySession};
pub use playback_controller::PlaybackController;
pub use provider::{
    AudioQuality, BoxFuture, DynMusicProvider, MusicProvider, PlaybackSource, Playlist,
    PlaylistWindow, ProviderError, ProviderId, Track, TrackAvailability,
};
pub use queue::{PlayQueue, QueueError, DEFAULT_MAX_QUEUE_LEN};
pub use retry::RetryPolicy;
pub use stream_buffer::{start_gate, BufferPhase, StreamBuffer, StreamReader};
pub use stream_transport::{StreamTask, UrlRefresher};
