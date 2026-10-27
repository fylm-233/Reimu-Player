//! GUI ↔ Provider 异步桥（规范 7.1/8.2：网络请求不得阻塞 UI 线程）。
//!
//! egui 是单线程立即模式 UI；Provider/下载调用是 async。本模块把请求派发到
//! 独立工作线程（内嵌 tokio 运行时），结果经 channel 送回 UI 线程轮询。
//!
//! 设计要点：
//! - **不引入具体实现**：桥持 `Arc<dyn DynMusicProvider>` + `Arc<dyn DynAudioFetcher>`
//!   （对象安全视图），换实现（真实网易云 ↔ 测试 Mock）不动本文件。
//! - **序列号防过期**：每个请求携带递增 `seq`；UI 侧只接受 `seq` 等于
//!   该面板最新请求号的响应，晚到的旧结果直接丢弃（规范 8.2）。
//! - **协作式取消**：[`ProviderBridge::cancel_pending`] 提升取消水位，工作线程用
//!   `tokio::select!` 让在途 future **提前终止**（含下载中断），队列中的旧请求
//!   开始执行前也会被跳过。取消结果以 [`Response::Cancelled`] 回执。
//! - **有界队列**：请求队列设容量上限（规范 8.2）；溢出立即回执取消，
//!   避免 UI 永久停在加载态。
//! - 错误以 [`ProviderError`] 原样传递，UI 再转人话（[`super::app`] 负责）。

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender, TrySendError};
use std::sync::Arc;

use tokio::sync::watch;

use crate::music::lyrics::Lyrics;
use crate::music::online_playback::{
    AudioKind, DynAudioFetcher, DynProviderShim, OnlineTrackLoader, PreparedTrack,
};
use crate::music::provider::{DynMusicProvider, Playlist, ProviderError, QualityCeiling, Track};
use crate::music::retry::RetryPolicy;

/// 请求队列容量上限（规范 8.2「channel 必须有合理的容量上限」）。
const REQUEST_QUEUE_CAP: usize = 32;

/// 封面并发下载线程数。
///
/// 4 是刻意保守的：网易云图片 CDN 对单 IP 的并发并不友好，再多只会换来更多
/// 超时重试；而一屏磁带架最多也就十几张，4 路已能把串行时的十几倍等待压平。
const COVER_WORKERS: usize = 4;

/// 每条封面队列的容量（溢出即回执取消，UI 侧可稍后重发）。
const COVER_QUEUE_CAP: usize = 16;

/// 装载队列的容量。
///
/// 不是 1：换台时旧下载要先被取消水位打断，这中间新点的歌必须排得进去，
/// 否则"点下一首"会被当场判成取消——用户看到的是按了没反应。
const AUDIO_QUEUE_CAP: usize = 4;

/// 封面磁盘缓存的条目上限（约 18 MB，见 [`COVER_CACHE_FILE_BYTES`]）。
const COVER_DISK_CACHE_MAX: usize = 512;

/// 单张封面的下载超时（秒）。
///
/// 比音频/接口请求短得多是有意的：封面是装饰，96 px 的小图，一个慢 CDN 节点
/// 不该占住一条封面线程十几秒——超时后回退程序化封面，用户至少能继续翻列表。
const COVER_TIMEOUT_SECS: u64 = 4;

/// 封面的重试预算：**比接口请求更小**（默认是 2 次 + 两级退避）。
///
/// 最坏情况 = 4 s + 300 ms + 4 s ≈ 8.3 s，而一条封面线程被这样占住时，
/// 后面排着的是整面墙的图。救不回来的封面在 UI 侧只是退回程序化图案，
/// 不值得为它押上整池的吞吐。
fn cover_retry_policy() -> RetryPolicy {
    RetryPolicy {
        max_retries: 1,
        base_delay: std::time::Duration::from_millis(300),
    }
}

/// UI → 工作线程的请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// 在线搜索。
    Search {
        /// 请求序列号（丢弃过期响应用）。
        seq: u64,
        keyword: String,
        limit: u32,
    },
    /// 歌曲详情（批量）。
    SongDetails {
        /// 请求序列号。
        seq: u64,
        ids: Vec<String>,
    },
    /// 歌单分页读取。
    Playlist {
        /// 请求序列号。
        seq: u64,
        playlist_id: String,
        offset: u32,
        limit: u32,
    },
    /// 精选列表（推荐歌单 / 官方排行榜）。
    Featured {
        /// 请求序列号。
        seq: u64,
        kind: crate::music::provider::FeaturedKind,
        limit: u32,
    },
    /// 歌词读取（阶段 F）。
    Lyrics {
        /// 请求序列号。
        seq: u64,
        song_id: String,
    },
    /// 每日推荐歌曲一批（需登录；服务端按账号 + 当天生成）。
    DailySongs {
        /// 请求序列号。
        seq: u64,
    },
    /// 查询登录状态（匿名态是合法结果，不是错误）。
    LoginStatus {
        /// 请求序列号。
        seq: u64,
    },
    /// 发起二维码登录（返回轮询 key + 二维码内容）。
    QrStart {
        /// 请求序列号。
        seq: u64,
    },
    /// 轮询二维码扫码状态。
    QrPoll {
        /// 请求序列号。
        seq: u64,
        /// `QrStart` 返回的轮询凭据。
        key: String,
    },
    /// 手机号 + 密码登录。
    CellphoneLogin {
        /// 请求序列号。
        seq: u64,
        phone: String,
        password: String,
    },
    /// Cookie 登录（用户粘贴的会话 Cookie）。
    CookieLogin {
        /// 请求序列号。
        seq: u64,
        cookie: String,
    },
    /// 退出登录。
    Logout {
        /// 请求序列号。
        seq: u64,
    },
    /// 喜欢 / 取消喜欢一首歌（官方红心，写入账号「我喜欢的音乐」）。
    LikeSong {
        /// 请求序列号。
        seq: u64,
        /// 歌曲 ID。
        song_id: String,
        /// `true` = 喜欢，`false` = 取消。
        like: bool,
    },
    /// 云端「我喜欢的音乐」ID 列表（收藏双向同步的**拉取**方向）。
    LikedSongs {
        /// 请求序列号。
        seq: u64,
    },
    /// 专辑曲目（点专辑名跳「专辑曲子」）。
    AlbumSongs {
        /// 请求序列号。
        seq: u64,
        /// 专辑 ID。
        album_id: String,
    },
    /// 艺人热门曲目（点歌手名跳「歌手曲子」）。
    ArtistSongs {
        /// 请求序列号。
        seq: u64,
        /// 艺人 ID。
        artist_id: String,
        /// 最多取几首。
        limit: u32,
    },
    /// 账号自己的歌单列表（登录后展示）。
    UserPlaylists {
        /// 请求序列号。
        seq: u64,
        /// 最多取几个。
        limit: u32,
    },
    /// 展开短链（`163cn.tv/xxx`）到最终 URL。
    ExpandLink {
        /// 请求序列号。
        seq: u64,
        /// 短链 URL。
        url: String,
    },
    /// 心动模式（智能播放）一批曲目——以「某首歌 + 它所属的歌单」为种子。
    IntelligenceList {
        /// 请求序列号。
        seq: u64,
        /// 种子歌曲 ID（接口 `id`）。
        song_id: String,
        /// 种子所属歌单 ID（接口 `pid`）。
        playlist_id: String,
        /// 从哪首开始播（接口 `sid`）；空串 = 由服务端取 `song_id`。
        start_id: String,
        /// 要几首。**必须显式给**：接口默认 `count=1`，不传就只回一首。
        count: u32,
    },
    /// 本地文件匹配网易云曲目信息（补封面 / 专辑 / 时长，并拿到可用于心动模式的数字歌曲 ID）。
    MatchLocal {
        /// 请求序列号。
        seq: u64,
        /// 本地文件绝对路径（也就是本地曲目的 `Track.id`）。
        path: String,
    },
}

impl Request {
    /// 本请求的序列号。
    pub fn seq(&self) -> u64 {
        match self {
            Request::Search { seq, .. }
            | Request::SongDetails { seq, .. }
            | Request::Playlist { seq, .. }
            | Request::Featured { seq, .. }
            | Request::Lyrics { seq, .. }
            | Request::DailySongs { seq }
            | Request::LoginStatus { seq }
            | Request::QrStart { seq }
            | Request::QrPoll { seq, .. }
            | Request::CellphoneLogin { seq, .. }
            | Request::CookieLogin { seq, .. }
            | Request::Logout { seq }
            | Request::LikeSong { seq, .. }
            | Request::LikedSongs { seq }
            | Request::AlbumSongs { seq, .. }
            | Request::ArtistSongs { seq, .. }
            | Request::UserPlaylists { seq, .. }
            | Request::ExpandLink { seq, .. }
            | Request::IntelligenceList { seq, .. }
            | Request::MatchLocal { seq, .. } => *seq,
        }
    }
}

/// 工作线程 → UI 的响应（携带对应请求的序列号）。
#[derive(Debug)]
pub enum Response {
    /// 搜索结果。
    Search {
        /// 对应请求的序列号。
        seq: u64,
        result: Result<Vec<Track>, ProviderError>,
    },
    /// 歌曲详情结果。
    SongDetails {
        /// 对应请求的序列号。
        seq: u64,
        result: Result<Vec<Track>, ProviderError>,
    },
    /// 歌单分页结果。
    Playlist {
        /// 对应请求的序列号。
        seq: u64,
        result: Result<Playlist, ProviderError>,
    },
    /// 精选列表结果（推荐歌单 / 官方排行榜）。
    Featured {
        /// 对应请求的序列号。
        seq: u64,
        result: Result<Vec<crate::music::provider::PlaylistSummary>, ProviderError>,
    },
    /// 在线曲目装载结果（临时文件已就绪）。
    Prepared {
        /// 对应请求的序列号。
        seq: u64,
        result: Result<PreparedTrack, ProviderError>,
    },
    /// 预取结果（只进会话缓存，**不播放**）。
    ///
    /// 为什么必须是独立变体而不是给 [`Self::Prepared`] 加个标志位：预取的
    /// `seq` 与 `PlayerCore::prepare_seq` 不会相等，但它一旦落进"其它响应"
    /// 分支就容易被当成播放结果处理。类型上分开，误用直接编译不过。
    Prefetched {
        /// 对应请求的序列号。
        seq: u64,
        /// 预取的歌曲 ID。
        song_id: String,
        result: Result<PreparedTrack, ProviderError>,
    },
    /// 歌词结果。
    Lyrics {
        /// 对应请求的序列号。
        seq: u64,
        result: Result<Lyrics, ProviderError>,
    },
    /// 每日推荐歌曲结果。
    DailySongs {
        /// 对应请求的序列号。
        seq: u64,
        result: Result<Vec<Track>, ProviderError>,
    },
    /// 登录状态结果。
    LoginStatus {
        /// 对应请求的序列号。
        seq: u64,
        result: Result<crate::music::provider::LoginState, ProviderError>,
    },
    /// 二维码登录发起结果。
    QrStarted {
        /// 对应请求的序列号。
        seq: u64,
        result: Result<crate::music::provider::QrLoginSession, ProviderError>,
    },
    /// 二维码轮询结果。
    QrPolled {
        /// 对应请求的序列号。
        seq: u64,
        result: Result<crate::music::provider::LoginState, ProviderError>,
    },
    /// 手机号登录结果。
    CellphoneLoggedIn {
        /// 对应请求的序列号。
        seq: u64,
        result: Result<crate::music::provider::LoginState, ProviderError>,
    },
    /// Cookie 登录结果。
    CookieLoggedIn {
        /// 对应请求的序列号。
        seq: u64,
        result: Result<crate::music::provider::LoginState, ProviderError>,
    },
    /// 退出登录结果。
    LoggedOut {
        /// 对应请求的序列号。
        seq: u64,
        result: Result<(), ProviderError>,
    },
    /// 喜欢 / 取消喜欢结果（携带 `like` 便于调用方归位）。
    Liked {
        /// 对应请求的序列号。
        seq: u64,
        /// 请求时的目标状态。
        like: bool,
        result: Result<(), ProviderError>,
    },
    /// 云端收藏 ID 列表结果。
    LikedSongs {
        /// 对应请求的序列号。
        seq: u64,
        result: Result<Vec<String>, ProviderError>,
    },
    /// 专辑曲目结果（携带 `album_id` 便于调用方归位）。
    AlbumSongs {
        /// 对应请求的序列号。
        seq: u64,
        /// 请求时的专辑 ID。
        album_id: String,
        result: Result<Vec<Track>, ProviderError>,
    },
    /// 艺人曲目结果（携带 `artist_id` 便于调用方归位）。
    ArtistSongs {
        /// 对应请求的序列号。
        seq: u64,
        /// 请求时的艺人 ID。
        artist_id: String,
        result: Result<Vec<Track>, ProviderError>,
    },
    /// 我的歌单列表结果。
    UserPlaylists {
        /// 对应请求的序列号。
        seq: u64,
        result: Result<Vec<crate::music::provider::PlaylistSummary>, ProviderError>,
    },
    /// 短链展开结果（携带原 URL 便于调用方归位）。
    Expanded {
        /// 对应请求的序列号。
        seq: u64,
        /// 原短链。
        url: String,
        /// 展开后的最终 URL。
        result: Result<String, ProviderError>,
    },
    /// 封面点阵结果（携带 URL 便于缓存归位）。
    ///
    /// `result` 里已经是**烘好的 RGBA**（`COVER_PX × COVER_PX × 4`），不是原始
    /// 图片字节：解码 + 缩放 + halftone 都在工作线程完成，UI 线程只上传纹理。
    /// 否则每次换曲都要在 UI 线程解一张 640×640 的 JPEG，直接掉帧
    /// （表现为「载入时磁带动画卡住」）。
    CoverImage {
        /// 对应请求的序列号。
        seq: u64,
        /// 原封面 URL。
        url: String,
        /// 已施加 §3.8 网纹的 RGBA 像素。
        result: Result<Vec<u8>, ProviderError>,
    },
    /// 请求被取消（用户换台 / 队列溢出 / 进程退出）。
    Cancelled {
        /// 对应请求的序列号。
        seq: u64,
    },
    /// 心动模式（智能播放）结果。
    IntelligenceList {
        /// 对应请求的序列号。
        seq: u64,
        result: Result<Vec<Track>, ProviderError>,
    },
    /// 本地文件匹配结果（携带原路径便于归位）。
    ///
    /// `Ok(None)` = 匹配成功但服务端没给出候选，**不是错误**：UI 静默保留
    /// 本地标签即可，不该为"这首歌在网上查不到"弹错误框。
    MatchLocal {
        /// 对应请求的序列号。
        seq: u64,
        /// 请求时的本地路径。
        path: String,
        result: Result<Option<Track>, ProviderError>,
    },
}

impl Response {
    /// 对应请求的序列号。
    pub fn seq(&self) -> u64 {
        match self {
            Response::Search { seq, .. }
            | Response::SongDetails { seq, .. }
            | Response::Playlist { seq, .. }
            | Response::Featured { seq, .. }
            | Response::Prepared { seq, .. }
            | Response::Prefetched { seq, .. }
            | Response::Lyrics { seq, .. }
            | Response::DailySongs { seq, .. }
            | Response::LoginStatus { seq, .. }
            | Response::QrStarted { seq, .. }
            | Response::QrPolled { seq, .. }
            | Response::CellphoneLoggedIn { seq, .. }
            | Response::CookieLoggedIn { seq, .. }
            | Response::LoggedOut { seq, .. }
            | Response::Liked { seq, .. }
            | Response::LikedSongs { seq, .. }
            | Response::AlbumSongs { seq, .. }
            | Response::ArtistSongs { seq, .. }
            | Response::UserPlaylists { seq, .. }
            | Response::Expanded { seq, .. }
            | Response::CoverImage { seq, .. }
            | Response::IntelligenceList { seq, .. }
            | Response::MatchLocal { seq, .. }
            | Response::Cancelled { seq } => *seq,
        }
    }
}

/// 异步桥。UI 每帧 [`ProviderBridge::poll`] 接收积压响应。
pub struct ProviderBridge {
    request_tx: Option<SyncSender<Request>>,
    /// 发送端克隆：队列溢出时由 UI 线程直接回执取消（避免永久加载）。
    overflow_tx: Sender<Response>,
    response_rx: Receiver<Response>,
    /// 元数据 lane 的取消水位（歌词/歌单/搜索/详情）。
    cancel_tx: watch::Sender<u64>,
    /// 音频 lane 的取消水位，**与元数据那条完全独立**。
    ///
    /// 为什么不复用 `cancel_tx`：换台必须立刻掐掉在途下载，但**不该**顺手作废
    /// 刚发出的歌单、搜索、歌词请求——那会让"点下一首"变成"整页数据重新加载"。
    /// 两条 lane 各有各的语义，就各用各的水位。
    audio_cancel_tx: watch::Sender<u64>,
    next_seq: u64,
    worker: Option<std::thread::JoinHandle<()>>,
    /// 音频装载线程：一条自己的通道，下载多久都不拦元数据请求。
    audio_tx: Option<SyncSender<AudioRequest>>,
    audio_worker: Option<std::thread::JoinHandle<()>>,
    /// 最近一次预取的目标歌曲 ID。
    ///
    /// 用户点播的正是这一首时**不打断**预取：它下的就是马上要用的那份字节，
    /// 打断等于把已经下好的部分全丢掉从 0 重来。
    prefetch_song: Option<String>,
    /// 引擎当前装载的文件（UI 线程写、装载线程读）。
    ///
    /// 缓存淘汰与半成品清扫**必须**跳过它：`RodioEngine::reload()` 是按路径
    /// 重开的，文件被删掉后下一次 play/seek 就 `MediaRead` 失败。
    active_audio: Arc<std::sync::Mutex<Option<PathBuf>>>,
    /// 封面下载池：每个线程一条自己的队列（轮转派发）。
    ///
    /// 为什么不排进那条串行队列：封面是**量大、互不相关、可并发**的读请求。
    /// 一屏磁带架一次就要拉十几张，全挤在同一条 worker 上时，它们既排在
    /// 「我的歌单」这种 1.4 秒的大请求后面，又彼此排队——用户看到的就是
    /// "列表出来了，封面一格一格慢慢点灯"。
    cover_txs: Vec<SyncSender<CoverRequest>>,
    /// 轮转下标（哪条队列下一个收）。
    cover_next: usize,
    /// 关机标志：置位后封面线程不再开始新的下载（理由见 `Drop` 里的说明）。
    cover_stop: Arc<std::sync::atomic::AtomicBool>,
    /// UI 侧的 Provider 句柄（与工作线程共享同一个实例）。
    ///
    /// 存在的唯一理由：**同步、无网络**的设置类调用（音质上限）不必排进
    /// 请求队列——那要占一个 seq、等一次回执，而它只是往原子量里写一个字节。
    provider: Arc<dyn DynMusicProvider>,
}

impl ProviderBridge {
    /// 启动桥：工作线程内嵌 tokio 当前线程运行时，逐个执行请求；
    /// 封面另开一小池并发下载（见 [`Self::cover_txs`]）。
    pub fn spawn(provider: Arc<dyn DynMusicProvider>, fetcher: Arc<dyn DynAudioFetcher>) -> Self {
        let (request_tx, request_rx) = sync_channel::<Request>(REQUEST_QUEUE_CAP);
        let (response_tx, response_rx) = channel::<Response>();
        let overflow_tx = response_tx.clone();
        let (cancel_tx, cancel_rx) = watch::channel(0u64);
        // 音频 lane 自己的一条水位：换台只掐下载，不作废元数据请求
        let (audio_cancel_tx, audio_cancel_rx) = watch::channel(0u64);

        // 封面池：先于 worker 启动拿 clone，`response_tx` 随后移进串行线程
        let cover_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut cover_txs = Vec::with_capacity(COVER_WORKERS);
        for id in 0..COVER_WORKERS {
            let (ctx, crx) = sync_channel::<CoverRequest>(COVER_QUEUE_CAP);
            let rtx = response_tx.clone();
            let stop = cover_stop.clone();
            let started = std::thread::Builder::new()
                .name(format!("cover-bridge-{id}"))
                .spawn(move || cover_loop(crx, rtx, stop))
                .is_ok();
            // 起不来的线程不登记发送端：这条队列会立即判为不可用，
            // 请求走「回执取消、UI 稍后重发」那条已经存在的路，而不是攒一堆没人收的活儿。
            if started {
                cover_txs.push(ctx);
            } else {
                drop(ctx);
                tracing::warn!("封面线程 {id} 启动失败，该槽位退出封面池");
            }
        }

        let worker_provider = provider.clone();
        let (audio_tx, audio_rx) = sync_channel::<AudioRequest>(AUDIO_QUEUE_CAP);
        let audio_response_tx = response_tx.clone();
        let audio_provider = provider.clone();
        let active_audio: Arc<std::sync::Mutex<Option<PathBuf>>> =
            Arc::new(std::sync::Mutex::new(None));
        let audio_active = active_audio.clone();
        let audio_worker = std::thread::Builder::new()
            .name("audio-loader".into())
            .spawn(move || {
                audio_loop(
                    audio_provider,
                    fetcher,
                    audio_rx,
                    audio_response_tx,
                    audio_cancel_rx,
                    audio_active,
                )
            })
            .map_err(|e| tracing::warn!("装载线程启动失败，播放将立即回执取消: {e}"))
            .ok();
        let audio_tx = if audio_worker.is_some() {
            Some(audio_tx)
        } else {
            drop(audio_tx);
            None
        };

        let worker = std::thread::Builder::new()
            .name("provider-bridge".into())
            .spawn(move || worker_loop(worker_provider, request_rx, response_tx, cancel_rx))
            .expect("无法启动 Provider 工作线程");

        Self {
            request_tx: Some(request_tx),
            overflow_tx,
            response_rx,
            cancel_tx,
            audio_cancel_tx,
            next_seq: 1,
            worker: Some(worker),
            audio_tx,
            audio_worker,
            prefetch_song: None,
            active_audio,
            cover_txs,
            cover_next: 0,
            cover_stop,
            provider,
        }
    }

    /// 设置播放音质上限（同步原子写，UI 线程直调）。
    pub fn set_quality(&self, ceiling: QualityCeiling) {
        self.provider.set_quality(ceiling);
    }

    /// 当前音质上限（面板回读用）。
    pub fn quality(&self) -> QualityCeiling {
        self.provider.quality()
    }

    /// 音频 lane 的取消水位（测试专用）。
    ///
    /// 存在的理由：换台的正确性全压在"先抬水位、后分配 seq"这一个顺序上。
    /// 水位必须**恰好等于**新请求的 seq（`wait_cancelled` 用严格 `>`，等于才安全），
    /// 写反时会大于它、新请求当场取消自己。这条不变式只能这样断言。
    #[cfg(test)]
    pub fn audio_watermark(&self) -> u64 {
        *self.audio_cancel_tx.borrow()
    }

    /// 分配下一个序列号并发送搜索请求，返回该请求的序列号。
    pub fn search(&mut self, keyword: impl Into<String>, limit: u32) -> u64 {
        let seq = self.next_seq();
        self.send(Request::Search {
            seq,
            keyword: keyword.into(),
            limit,
        });
        seq
    }

    /// 分配序列号并发送歌曲详情请求，返回序列号。
    pub fn song_details(&mut self, ids: Vec<String>) -> u64 {
        let seq = self.next_seq();
        self.send(Request::SongDetails { seq, ids });
        seq
    }

    /// 分配序列号并发送歌单分页请求，返回序列号。
    pub fn playlist(&mut self, playlist_id: impl Into<String>, offset: u32, limit: u32) -> u64 {
        let seq = self.next_seq();
        self.send(Request::Playlist {
            seq,
            playlist_id: playlist_id.into(),
            offset,
            limit,
        });
        seq
    }

    /// 分配序列号并发送精选列表请求（推荐歌单 / 官方排行榜），返回序列号。
    pub fn featured(&mut self, kind: crate::music::provider::FeaturedKind, limit: u32) -> u64 {
        let seq = self.next_seq();
        self.send(Request::Featured { seq, kind, limit });
        seq
    }

    /// 分配序列号并把音频装载交给**装载线程**（不占元数据队列）。
    ///
    /// 会先处理在途预取：目标就是这一首时**不打断**（它下的就是马上要用的
    /// 那份字节），否则抬水位掐掉它，让点播立刻拿到 lane，而不是排在预取
    /// 后面等几十秒。
    pub fn prepare_track(&mut self, song_id: impl Into<String>) -> u64 {
        let id = song_id.into();
        if self.prefetch_song.as_deref() == Some(id.as_str()) {
            self.prefetch_song = None;
        } else {
            self.cancel_audio();
        }
        self.send_audio(id, AudioKind::Play)
    }

    /// 预取下一首：结果只进会话缓存，回执走 [`Response::Prefetched`]。
    pub fn prefetch_track(&mut self, song_id: impl Into<String>) -> u64 {
        let id = song_id.into();
        self.prefetch_song = Some(id.clone());
        self.send_audio(id, AudioKind::Prefetch)
    }

    /// 告知装载线程「引擎现在拿着哪个文件」，供缓存淘汰与半成品清扫避让。
    pub fn set_active_audio(&self, path: Option<PathBuf>) {
        if let Ok(mut slot) = self.active_audio.lock() {
            *slot = path;
        }
    }

    fn send_audio(&mut self, song_id: String, kind: AudioKind) -> u64 {
        let seq = self.next_seq();
        let request = AudioRequest { seq, song_id, kind };
        let Some(tx) = &self.audio_tx else {
            let _ = self.overflow_tx.send(Response::Cancelled { seq });
            return seq;
        };
        match tx.try_send(request) {
            Ok(()) => {}
            Err(TrySendError::Full(request)) | Err(TrySendError::Disconnected(request)) => {
                tracing::warn!("装载队列已满/不可用，请求被取消: seq={}", request.seq);
                let _ = self.overflow_tx.send(Response::Cancelled { seq });
            }
        }
        seq
    }
    /// 分配序列号并发送歌词请求，返回序列号。
    pub fn lyrics(&mut self, song_id: impl Into<String>) -> u64 {
        let seq = self.next_seq();
        self.send(Request::Lyrics {
            seq,
            song_id: song_id.into(),
        });
        seq
    }

    /// 分配序列号并发送每日推荐歌曲请求，返回序列号。
    ///
    /// 无参数：要几首由服务端决定（通常 30 首），客户端**不传 limit**——
    /// 传了也只是截断，截断后的"每日推荐"就不再是服务端那批了。
    pub fn daily_songs(&mut self) -> u64 {
        let seq = self.next_seq();
        self.send(Request::DailySongs { seq });
        seq
    }

    /// 分配序列号并请求心动模式（智能播放）一批曲目，返回序列号。
    ///
    /// `start_id` 传空串表示"从种子本身开始"（服务端会把 `sid` 取成 `id`）。
    pub fn intelligence_list(
        &mut self,
        song_id: impl Into<String>,
        playlist_id: impl Into<String>,
        start_id: impl Into<String>,
        count: u32,
    ) -> u64 {
        let seq = self.next_seq();
        self.send(Request::IntelligenceList {
            seq,
            song_id: song_id.into(),
            playlist_id: playlist_id.into(),
            start_id: start_id.into(),
            count,
        });
        seq
    }

    /// 分配序列号并请求匹配某个本地文件的网易云信息，返回序列号。
    pub fn match_local(&mut self, path: impl Into<String>) -> u64 {
        let seq = self.next_seq();
        self.send(Request::MatchLocal {
            seq,
            path: path.into(),
        });
        seq
    }

    /// 分配序列号并查询登录状态，返回序列号。
    pub fn login_status(&mut self) -> u64 {
        let seq = self.next_seq();
        self.send(Request::LoginStatus { seq });
        seq
    }

    /// 分配序列号并发起二维码登录，返回序列号。
    pub fn qr_start(&mut self) -> u64 {
        let seq = self.next_seq();
        self.send(Request::QrStart { seq });
        seq
    }

    /// 分配序列号并轮询二维码状态，返回序列号。
    pub fn qr_poll(&mut self, key: impl Into<String>) -> u64 {
        let seq = self.next_seq();
        self.send(Request::QrPoll {
            seq,
            key: key.into(),
        });
        seq
    }

    /// 分配序列号并发送手机号登录请求，返回序列号。
    pub fn login_cellphone(
        &mut self,
        phone: impl Into<String>,
        password: impl Into<String>,
    ) -> u64 {
        let seq = self.next_seq();
        self.send(Request::CellphoneLogin {
            seq,
            phone: phone.into(),
            password: password.into(),
        });
        seq
    }

    /// 分配序列号并发送 Cookie 登录请求，返回序列号。
    pub fn login_cookie(&mut self, cookie: impl Into<String>) -> u64 {
        let seq = self.next_seq();
        self.send(Request::CookieLogin {
            seq,
            cookie: cookie.into(),
        });
        seq
    }

    /// 分配序列号并发送退出登录请求，返回序列号。
    pub fn logout(&mut self) -> u64 {
        let seq = self.next_seq();
        self.send(Request::Logout { seq });
        seq
    }

    /// 分配序列号并发送「喜欢 / 取消喜欢」请求，返回序列号。
    pub fn like_song(&mut self, song_id: impl Into<String>, like: bool) -> u64 {
        let seq = self.next_seq();
        self.send(Request::LikeSong {
            seq,
            song_id: song_id.into(),
            like,
        });
        seq
    }

    /// 当前会话 Cookie（同步、无网络；登录成功后 `Some`，供加密持久化）。
    pub fn session_cookie(&self) -> Option<String> {
        self.provider.session_cookie()
    }

    /// 分配序列号并请求云端「我喜欢的音乐」ID 列表。
    pub fn liked_songs(&mut self) -> u64 {
        let seq = self.next_seq();
        self.send(Request::LikedSongs { seq });
        seq
    }

    /// 分配序列号并请求专辑曲目。
    pub fn album_songs(&mut self, album_id: impl Into<String>) -> u64 {
        let seq = self.next_seq();
        self.send(Request::AlbumSongs {
            seq,
            album_id: album_id.into(),
        });
        seq
    }

    /// 分配序列号并请求艺人热门曲目。
    pub fn artist_songs(&mut self, artist_id: impl Into<String>, limit: u32) -> u64 {
        let seq = self.next_seq();
        self.send(Request::ArtistSongs {
            seq,
            artist_id: artist_id.into(),
            limit,
        });
        seq
    }

    /// 分配序列号并请求账号歌单列表。
    pub fn user_playlists(&mut self, limit: u32) -> u64 {
        let seq = self.next_seq();
        self.send(Request::UserPlaylists { seq, limit });
        seq
    }

    /// 分配序列号并请求展开短链，返回序列号。
    pub fn expand_link(&mut self, url: impl Into<String>) -> u64 {
        let seq = self.next_seq();
        self.send(Request::ExpandLink {
            seq,
            url: url.into(),
        });
        seq
    }

    /// 分配序列号并把封面交给封面池（**不占串行队列**）。
    pub fn cover_image(&mut self, url: impl Into<String>) -> u64 {
        let seq = self.next_seq();
        let request = CoverRequest {
            seq,
            url: url.into(),
        };
        if self.cover_txs.is_empty() {
            let _ = self.overflow_tx.send(Response::Cancelled { seq });
            return seq;
        }
        let i = self.cover_next % self.cover_txs.len();
        self.cover_next = self.cover_next.wrapping_add(1);
        match self.cover_txs[i].try_send(request) {
            Ok(()) => {}
            Err(TrySendError::Full(request)) | Err(TrySendError::Disconnected(request)) => {
                // 立即回执取消：UI 侧要能把这个 URL 从"在途"里摘掉、稍后重发
                tracing::debug!("封面队列已满/不可用，请求被取消: seq={}", request.seq);
                let _ = self.overflow_tx.send(Response::Cancelled { seq });
            }
        }
        seq
    }

    /// 取消**所有已发出未完成**的请求（退出、用户显式取消）。
    ///
    /// 已发出的请求（在途 + 排队）序列号都小于当前计数器，取消水位取
    /// 当前计数器即可精确覆盖它们、且不影响此后新发的请求。
    ///
    /// 两条 lane 一起抬——调用方要的语义是"全停"。**换台不要用这个**：
    /// 换台只想掐下载，见 [`Self::cancel_audio`]。
    pub fn cancel_pending(&mut self) {
        self.cancel_audio();
        let watermark = self.next_seq;
        let _ = self.cancel_tx.send(watermark);
    }

    /// 取消音频 lane 上已发出未完成的装载（换台 / 转去播本地）。
    ///
    /// 只抬音频水位，**不动**元数据 lane：换台时刚发出的歌单、搜索、歌词请求
    /// 必须继续跑完，否则用户看到的是"点下一首，整页列表重新加载"。
    ///
    /// **必须在分配新 seq 之前调用**：水位取当前计数器，新请求拿到的 seq 恰好
    /// 等于水位，而 [`wait_cancelled`] 用严格 `>` 比较——新请求不会取消自己。
    /// 顺序写反的表现是"点歌永远失败"，
    /// [`tests::cancel_audio_does_not_affect_the_next_request`] 守着这条。
    pub fn cancel_audio(&mut self) {
        let watermark = self.next_seq;
        let _ = self.audio_cancel_tx.send(watermark);
    }

    /// 非阻塞接收一个响应；无积压时返回 `None`。
    pub fn poll(&self) -> Option<Response> {
        self.response_rx.try_recv().ok()
    }

    fn next_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1).max(1);
        seq
    }

    fn send(&self, request: Request) {
        let Some(tx) = &self.request_tx else {
            return; // 工作线程退出后（进程关闭）静默丢弃即可
        };
        match tx.try_send(request) {
            Ok(()) => {}
            Err(TrySendError::Full(request)) | Err(TrySendError::Disconnected(request)) => {
                // 队列满/线程已退：立即回执取消，UI 不得永久停在加载态
                tracing::warn!("请求队列已满/不可用，请求被取消: seq={}", request.seq());
                let _ = self
                    .overflow_tx
                    .send(Response::Cancelled { seq: request.seq() });
            }
        }
    }
}

impl Drop for ProviderBridge {
    fn drop(&mut self) {
        // 提升取消水位，让在途 future 尽快结束；再关闭请求通道让循环退出。
        let _ = self.cancel_tx.send(u64::MAX);
        // 音频 lane 那条水位**也必须拉满**，否则下面的 join 会一直等到
        // 在途下载自然结束（最长 60s 超时）——用户点了关闭，进程却留在后台。
        let _ = self.audio_cancel_tx.send(u64::MAX);
        drop(self.request_tx.take());
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
        // 装载线程：水位已经拉到 MAX，在途下载会被 `select!` 打断，
        // 所以这里的 join 是短的；等它把临时文件扫干净再走。
        drop(self.audio_tx.take());
        if let Some(handle) = self.audio_worker.take() {
            let _ = handle.join();
        }
        // 封面池：先置关机标志（在途下载跑完这一张就收工），再关掉发送端让
        // 空闲线程立刻从 `recv()` 退出。
        //
        // 这里**不 join**：封面线程只持有自拥有的通道、`Arc` 标志与一个
        // `Sender<Response>`，进程退出时任其自行结束是安全的。换成 join 的话，
        // 一条正卡在 CDN 上的下载会把关机拖到超时上限——用户点了关闭，进程却
        // 还多留好几秒，比留几个孤儿线程糟得多。
        self.cover_stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.cover_txs.clear();
    }
}

/// 等待「序列号 `seq` 被取消」。水位高于 `seq` 即视为取消。
async fn wait_cancelled(cancel_rx: &mut watch::Receiver<u64>, seq: u64) {
    loop {
        if *cancel_rx.borrow_and_update() > seq {
            return;
        }
        if cancel_rx.changed().await.is_err() {
            return; // 发送端已销毁（桥关闭）
        }
    }
}

/// 工作线程主循环：逐请求执行并回送响应（串行，避免惊群）。
///
/// 这里**只做元数据请求**（搜索、歌单、歌词、详情……，每个都在几百毫秒内）。
/// 音频装载曾经也排在这条队上，代价是：下载一首歌要几秒到几十秒，期间用户
/// 点开的任何列表、歌词、心动批次全部原地等待——那才是"加载慢"最主要的一跳。
/// 装载现在有自己的线程（见 [`audio_loop`]）。
fn worker_loop(
    provider: Arc<dyn DynMusicProvider>,
    request_rx: Receiver<Request>,
    response_tx: Sender<Response>,
    mut cancel_rx: watch::Receiver<u64>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("无法创建 tokio 运行时: {}", e);
            return;
        }
    };

    while let Ok(request) = request_rx.recv() {
        let seq = request.seq();

        // 排队期间已被取消的请求：直接跳过执行
        if *cancel_rx.borrow_and_update() > seq {
            if response_tx.send(Response::Cancelled { seq }).is_err() {
                break;
            }
            continue;
        }

        let response = runtime.block_on(async {
            let mut cancel_wait = cancel_rx.clone();
            tokio::select! {
                _ = wait_cancelled(&mut cancel_wait, seq) => Response::Cancelled { seq },
                response = execute(&provider, request) => response,
            }
        });

        if response_tx.send(response).is_err() {
            break; // UI 已退出
        }
    }
}

/// 音频装载请求（走自己的线程，不占元数据队列）。
#[derive(Debug)]
struct AudioRequest {
    seq: u64,
    song_id: String,
    /// 点播还是预取。决定回执走 [`Response::Prepared`] 还是
    /// [`Response::Prefetched`]，以及要不要做冷恢复。
    kind: AudioKind,
}

/// 装载线程：一次只下一首歌，但可以下很久而不拖累任何人。
///
/// 取消语义与串行 worker 一致，但用的是**自己的水位**（`audio_cancel_rx`）：
/// 换台时 UI 调 [`ProviderBridge::cancel_audio`] 抬它，在途的下载被 `select!`
/// 打断并回执 `Cancelled`，排队中的直接跳过。元数据 lane 的水位不受影响。
/// 装载器只有这一份实例，临时目录也因此只由一个所有者清扫——拆两条 lane 各自
/// 建一个 loader 的话，一边 `sweep_temp_dir` 会把另一边刚写了一半的文件删掉。
fn audio_loop(
    provider: Arc<dyn DynMusicProvider>,
    fetcher: Arc<dyn DynAudioFetcher>,
    request_rx: Receiver<AudioRequest>,
    response_tx: Sender<Response>,
    mut audio_cancel_rx: watch::Receiver<u64>,
    active: Arc<std::sync::Mutex<Option<PathBuf>>>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("无法创建装载运行时: {e}");
            return;
        }
    };

    // 在线装载器：provider/fetcher 都是对象安全视图（经 shim 投影），可运行时替换
    let mut loader = OnlineTrackLoader::new(DynProviderShim::new(provider), fetcher);

    while let Ok(request) = request_rx.recv() {
        let AudioRequest { seq, song_id, kind } = request;

        if *audio_cancel_rx.borrow_and_update() > seq {
            if response_tx.send(Response::Cancelled { seq }).is_err() {
                break;
            }
            continue;
        }

        // 每次装载前同步"引擎当前拿着哪个文件"：缓存淘汰与半成品清扫要避让它
        let current = active.lock().ok().and_then(|slot| slot.clone());
        loader.set_active(current);

        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let response = runtime.block_on(async {
            let mut cancel_wait = audio_cancel_rx.clone();
            tokio::select! {
                _ = wait_cancelled(&mut cancel_wait, seq) => {
                    cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                    Response::Cancelled { seq }
                }
                // 装载器内部已有 RetryPolicy，这里不再叠加
                result = loader.prepare_kind(&song_id, kind) => match kind {
                    AudioKind::Play => Response::Prepared { seq, result },
                    AudioKind::Prefetch => Response::Prefetched {
                        seq,
                        song_id: song_id.clone(),
                        result,
                    },
                },
            }
        });

        // 装载被中途取消：清扫未登记的半成品临时文件
        if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            loader.sweep_temp_dir();
        }

        if response_tx.send(response).is_err() {
            break; // UI 已退出
        }
    }

    // 退出前清理本会话临时文件（尽力而为）
    loader.cleanup();
}

/// 对一次 provider 调用套用规范 8.3 的有限重试。
///
/// 为什么在编排层重试而不是塞进 Provider：Provider 的职责是「发一次请求 +
/// 把结果翻译成 [`ProviderError`]」，重试属于策略，和 [`OnlineTrackLoader`]
/// 保持同一层级。**登录/二维码轮询不走这里**——它们有自己的失败计数语义，
/// 在这里再套一层重试只会让轮询节奏失控。
///
/// 每一次尝试都重新 clone 参数：`FnMut() -> Fut` 要求 `Fut` 是同一个类型，
/// 借用调用方局部变量无法满足。
async fn with_retry<T, F, Fut>(op: F) -> Result<T, ProviderError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ProviderError>>,
{
    RetryPolicy::default().retry_limited(op).await
}

/// 本地文件的匹配素材（`search_match` 的五个入参）。
struct LocalProbe {
    title: String,
    album: String,
    artist: String,
    duration_secs: u64,
    md5: String,
}

/// 读本地文件的标签 + 时长 + 内容摘要。凑不出可用素材返回 `None`。
///
/// - **时长取整秒**：SDK 对 `duration` 做 `parse::<i64>()`，`"259.21"` 这类
///   小数字符串解析失败会静默变成 `0`，服务端随即返回空结果——本地曲目
///   永远匹配不上（实测踩过）。
/// - **时长未知就不发**：`0` 秒同样必然查无此曲，与其在单线程桥上占一次
///   带重试的往返，不如直接放弃（界面上这张碟仍是"没有封面"，不是错误）。
/// - 标签缺失时回退曲库那套文件名约定（`艺人 - 标题`），让匹配用的字与
///   界面上看得到的字是同一组，不另起一套。
fn local_match_probe(path: &str) -> Option<LocalProbe> {
    let p = std::path::Path::new(path);
    let info = crate::music::media::load_media(p).ok()?;
    let duration_secs = info.duration.map(|d| d.as_secs()).unwrap_or(0);
    if duration_secs == 0 {
        tracing::debug!("本地匹配跳过（时长未知，服务端必然查无此曲）: {path}");
        return None;
    }
    let md5 = crate::music::media::persist_id(p)?;

    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (name_artist, name_title) = super::library::parse_display_name(&stem);
    let artist_from_name = (name_artist != "—").then_some(name_artist.as_str());

    let title = info
        .tags
        .title
        .clone()
        .or(Some(name_title))
        .filter(|s| !s.trim().is_empty())?;
    let artist = info
        .tags
        .artist
        .clone()
        .or_else(|| artist_from_name.map(str::to_string))
        .unwrap_or_default();

    Some(LocalProbe {
        title,
        album: info.tags.album.clone().unwrap_or_default(),
        artist,
        duration_secs,
        md5,
    })
}

/// 执行单个请求（网络 I/O 全部在本 future 内，可被 select! 中断）。
///
/// 除登录相关请求外，所有 API 调用都经 [`with_retry`]：
/// 「复用到对端已关闭的空闲连接」这类瞬时故障靠重试消化，
/// 用户不必再看到偶发的网络错误弹窗。
async fn execute(provider: &Arc<dyn DynMusicProvider>, request: Request) -> Response {
    match request {
        Request::Search {
            seq,
            keyword,
            limit,
        } => Response::Search {
            seq,
            result: with_retry(|| {
                let provider = Arc::clone(provider);
                let keyword = keyword.clone();
                async move { provider.search_songs(&keyword, limit).await }
            })
            .await,
        },
        Request::SongDetails { seq, ids } => Response::SongDetails {
            seq,
            result: with_retry(|| {
                let provider = Arc::clone(provider);
                let ids = ids.clone();
                async move { provider.song_details(&ids).await }
            })
            .await,
        },
        Request::Playlist {
            seq,
            playlist_id,
            offset,
            limit,
        } => Response::Playlist {
            seq,
            result: with_retry(|| {
                let provider = Arc::clone(provider);
                let playlist_id = playlist_id.clone();
                async move { provider.playlist(&playlist_id, offset, limit).await }
            })
            .await,
        },
        Request::Featured { seq, kind, limit } => Response::Featured {
            seq,
            result: with_retry(|| {
                let provider = Arc::clone(provider);
                async move { provider.featured_playlists(kind, limit).await }
            })
            .await,
        },
        Request::Lyrics { seq, song_id } => Response::Lyrics {
            seq,
            result: with_retry(|| {
                let provider = Arc::clone(provider);
                let song_id = song_id.clone();
                async move { provider.lyrics(&song_id).await }
            })
            .await,
        },
        // 每日推荐是**读**操作（服务端按天生成），可安全重试
        Request::DailySongs { seq } => Response::DailySongs {
            seq,
            result: with_retry(|| {
                let provider = Arc::clone(provider);
                async move { provider.daily_songs().await }
            })
            .await,
        },
        // 心动模式同样是读操作，可安全重试
        Request::IntelligenceList {
            seq,
            song_id,
            playlist_id,
            start_id,
            count,
        } => Response::IntelligenceList {
            seq,
            result: with_retry(|| {
                let provider = Arc::clone(provider);
                let song_id = song_id.clone();
                let playlist_id = playlist_id.clone();
                let start_id = start_id.clone();
                async move {
                    provider
                        .intelligence_list(&song_id, &playlist_id, &start_id, count)
                        .await
                }
            })
            .await,
        },
        // 本地匹配：查不到是**常态**（翻录自购碟、二创纯音乐），
        // 所以 `EmptyResult` 归一成 `Ok(None)`，不让 UI 为它弹错误框。
        Request::MatchLocal { seq, path } => {
            let result = match local_match_probe(&path) {
                None => Ok(None),
                Some(probe) => with_retry(|| {
                    let provider = Arc::clone(provider);
                    let (title, album, artist) = (
                        probe.title.clone(),
                        probe.album.clone(),
                        probe.artist.clone(),
                    );
                    let md5 = probe.md5.clone();
                    let secs = probe.duration_secs;
                    async move {
                        provider
                            .search_match(&title, &album, &artist, secs, &md5)
                            .await
                    }
                })
                .await
                .map(|mut tracks| tracks.pop())
                .or_else(|e| {
                    if e == ProviderError::EmptyResult {
                        Ok(None)
                    } else {
                        Err(e)
                    }
                }),
            };
            Response::MatchLocal { seq, path, result }
        }
        // 登录相关：不重试（各自有失败计数/轮询节奏）
        Request::LoginStatus { seq } => Response::LoginStatus {
            seq,
            result: provider.login_status().await,
        },
        Request::QrStart { seq } => Response::QrStarted {
            seq,
            result: provider.qr_login_start().await,
        },
        Request::QrPoll { seq, key } => Response::QrPolled {
            seq,
            result: provider.qr_login_poll(&key).await,
        },
        // 手机号 / Cookie / 退出：登录相关，不重试
        Request::CellphoneLogin {
            seq,
            phone,
            password,
        } => Response::CellphoneLoggedIn {
            seq,
            result: provider.login_cellphone(&phone, &password).await,
        },
        Request::CookieLogin { seq, cookie } => Response::CookieLoggedIn {
            seq,
            result: provider.login_cookie(&cookie).await,
        },
        Request::Logout { seq } => Response::LoggedOut {
            seq,
            result: provider.logout().await,
        },
        // 红心是**写操作**：不重试（重试可能把「取消喜欢」重复提交成两次切换，
        // 用户看到的结果与点击相反）。失败如实上报，由 UI 提示重试。
        Request::LikeSong { seq, song_id, like } => Response::Liked {
            seq,
            like,
            result: provider.like_song(&song_id, like).await,
        },
        // 以下四个都是**读**操作，可安全重试（幂等）
        Request::LikedSongs { seq } => Response::LikedSongs {
            seq,
            result: with_retry(|| {
                let provider = Arc::clone(provider);
                async move { provider.liked_song_ids().await }
            })
            .await,
        },
        Request::AlbumSongs { seq, album_id } => Response::AlbumSongs {
            seq,
            result: with_retry(|| {
                let provider = Arc::clone(provider);
                let album_id = album_id.clone();
                async move { provider.album_songs(&album_id).await }
            })
            .await,
            album_id,
        },
        Request::ArtistSongs {
            seq,
            artist_id,
            limit,
        } => Response::ArtistSongs {
            seq,
            result: with_retry(|| {
                let provider = Arc::clone(provider);
                let artist_id = artist_id.clone();
                async move { provider.artist_songs(&artist_id, limit).await }
            })
            .await,
            artist_id,
        },
        Request::UserPlaylists { seq, limit } => Response::UserPlaylists {
            seq,
            result: with_retry(|| {
                let provider = Arc::clone(provider);
                async move { provider.user_playlists(limit).await }
            })
            .await,
        },
        Request::ExpandLink { seq, url } => Response::Expanded {
            seq,
            result: with_retry(|| {
                let url = url.clone();
                async move { expand_short_link(&url).await }
            })
            .await,
            url,
        },
    }
}

/// 封面图体积上限（超过即放弃：封面是缩略图，不需要原图）。
const MAX_COVER_BYTES: usize = 4 * 1024 * 1024;

/// 向图片服务索取的封面边长（px）。
///
/// 界面只画 [`super::halftone::COVER_PX`] 的网点图，取 200 是为了留一点下采样
/// 的余量（原图越大，缩到 96 的边缘越干净），而不是为了看清细节。
const COVER_REQUEST_PX: u32 = 200;

/// 给网易云图片地址补上缩放参数（`…jpg?param=200y200`）。
///
/// 不补的话下的是**原图**：实测见过 5 MB 的一张，直接撞在 [`MAX_COVER_BYTES`]
/// 上被判死，那一格只好退回程序化图案——用户要的只是一个拇指大的方格，
/// 却为一整张原图付了带宽和等待。加了参数以后同一格只要十几 KB。
///
/// 只认扩展名，不认域名：图片服务换子域是常事（`p1`→`p3`，`music.126.net`→
/// 各种新域名），按域名白名单会在某天悄悄失效。已带 `param=` 的地址原样返回。
fn sized_cover_url(url: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, q)) => (base, Some(q)),
        None => (url, None),
    };
    let is_image = ["jpg", "jpeg", "png", "webp"]
        .iter()
        .any(|ext| base.to_ascii_lowercase().ends_with(&format!(".{ext}")));
    if !is_image || query.is_some() {
        return url.to_string();
    }
    format!("{base}?param={COVER_REQUEST_PX}y{COVER_REQUEST_PX}")
}

/// `reqwest::Error` → [`ProviderError`]。
///
/// 判据与 `providers::ncm::map_error` 保持同一套顺序，避免同一个物理故障
/// 在不同链路被翻译成两种说法：
/// 超时 → 状态码 → 连接建立失败 → 正文/解码 → **请求中途断开**。
///
/// 最后一档是"偶发网络错误"的真正来源：连接复用撞上对端已关闭的
/// keep-alive，hyper 报 `connection closed before message completed`。
/// 它既不是"网络不可用"（网络好得很），也值得重试一次。
fn classify_reqwest(e: &reqwest::Error) -> ProviderError {
    if e.is_timeout() {
        return ProviderError::RequestTimeout;
    }
    if let Some(status) = e.status() {
        return match status.as_u16() {
            429 => ProviderError::RateLimited,
            401 | 403 => ProviderError::AuthenticationRequired,
            500..=599 => ProviderError::DataProviderUnavailable,
            other => ProviderError::Unknown(format!("HTTP {}", other)),
        };
    }
    if e.is_connect() {
        tracing::warn!("连接建立失败（DNS/TCP/TLS）: {e}");
        return ProviderError::NetworkUnavailable;
    }
    if e.is_body() || e.is_decode() {
        tracing::warn!("响应正文读取/解析失败: {e}");
        return ProviderError::InvalidData;
    }
    if e.is_request() {
        tracing::warn!("请求中途连接断开（可重试）: {e}");
        return ProviderError::ConnectionReset;
    }
    tracing::warn!("未分类的 HTTP 错误: {e}");
    ProviderError::Unknown(e.to_string())
}

/// 展开短链：跟随重定向取最终 URL。
///
/// 用 `HEAD` 而非 `GET`：只需要 `Location` 链，不需要正文。
/// 网易云短链服务对 HEAD 支持良好；失败时回退 `GET`（只读首字节即丢弃）。
async fn expand_short_link(url: &str) -> Result<String, ProviderError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|_| ProviderError::NetworkUnavailable)?;
    let response = client.head(url).send().await.map_err(|e| {
        tracing::warn!("短链展开失败: {e}");
        classify_reqwest(&e)
    })?;
    let final_url = response.url().to_string();
    tracing::info!(target: "provider", "短链展开: {} -> {}", url, final_url);
    Ok(final_url)
}

/// 封面池的单条请求（不走 [`Request`]：它有自己的并发队列，不参与串行取消水位）。
#[derive(Debug)]
struct CoverRequest {
    seq: u64,
    url: String,
}

/// 一条封面线程：取出请求 → 查磁盘缓存 → 未命中才下载 → 回执。
///
/// 每个线程一个 current-thread 运行时：`reqwest` 的 future 需要 tokio 上下文，
/// 而这里没有跨任务共享的状态，起多线程运行时只是白占线程。
fn cover_loop(
    rx: Receiver<CoverRequest>,
    tx: Sender<Response>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("无法创建封面运行时: {e}");
            return;
        }
    };
    while let Ok(request) = rx.recv() {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return; // 桥正在销毁：不再开新的网络往返
        }
        let CoverRequest { seq, url } = request;
        let result = runtime.block_on(fetch_cover_cached(&url));
        if tx.send(Response::CoverImage { seq, url, result }).is_err() {
            return; // UI 已退出
        }
    }
}

/// 一张封面的磁盘缓存大小（烘好的 RGBA，未压缩）。
const COVER_CACHE_FILE_BYTES: usize = super::halftone::COVER_PX * super::halftone::COVER_PX * 4;

/// 封面磁盘缓存目录（`%APPDATA%\ReimuPlayer\covers`）。拿不到配置目录 → `None`（只走网络）。
fn cover_cache_dir() -> Option<std::path::PathBuf> {
    super::library::config_dir().map(|dir| dir.join("covers"))
}

/// URL → 缓存文件名（64 位哈希的十六进制）。
fn cover_cache_path(url: &str) -> Option<std::path::PathBuf> {
    let dir = cover_cache_dir()?;
    Some(dir.join(format!("{:016x}.bin", super::widgets::hash_id(url))))
}

/// 读磁盘缓存；长度不对（半截文件 / 尺寸改过）一律当没有。
fn read_cover_cache(path: &Path) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() != COVER_CACHE_FILE_BYTES {
        return None;
    }
    Some(bytes)
}

/// 写磁盘缓存，并在累计写入若干张后按最旧访问时间修剪到上限的一半。
///
/// 为什么修剪到一半而不是刚好超限：每次都刚好删到 512 的话，下一张又要删，
/// 目录扫描会变成常规开销。
fn write_cover_cache(path: &Path, rgba: &[u8]) {
    if rgba.len() != COVER_CACHE_FILE_BYTES {
        return;
    }
    let Some(dir) = path.parent().map(std::path::PathBuf::from) else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return; // 缓存是加速手段，写不进去不该影响显示
    }
    let tmp = path.with_extension("bin.tmp");
    if std::fs::write(&tmp, rgba).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
    static WRITES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n % 128 == 127 {
        prune_cover_cache(&dir);
    }
}

/// 按最后访问时间把缓存目录修剪到 [`COVER_DISK_CACHE_MAX`] 的一半。
fn prune_cover_cache(dir: &Path) {
    let mut entries: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("bin") {
            continue;
        }
        let atime = entry
            .metadata()
            .ok()
            .and_then(|m| m.accessed().ok())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        entries.push((path, atime));
    }
    if entries.len() <= COVER_DISK_CACHE_MAX {
        return;
    }
    entries.sort_by_key(|(_, atime)| *atime);
    let remove = entries.len() - COVER_DISK_CACHE_MAX / 2;
    for (path, _) in entries.into_iter().take(remove) {
        let _ = std::fs::remove_file(path);
    }
    tracing::debug!("封面磁盘缓存已修剪 {remove} 张");
}

/// 先查磁盘缓存、未命中才下载的封面获取（缓存让重启不必重下一整面墙的封面）。
///
/// 网络那一跳必须带重试（预算见 [`cover_retry_policy`]）：UI 侧一次失败就把这个
/// URL 永久记进"别再试"的名单，这里省掉重试等于让一次抖动变成一整轮的空白封面。
async fn fetch_cover_cached(url: &str) -> Result<Vec<u8>, ProviderError> {
    let path = cover_cache_path(url);
    if let Some(path) = path.as_ref() {
        if let Some(cached) = read_cover_cache(path) {
            return Ok(cached);
        }
    }
    let rgba = cover_retry_policy()
        .retry_limited(|| {
            let url = sized_cover_url(url);
            async move { fetch_cover_rgba(&url).await }
        })
        .await?;
    if let Some(path) = path.as_ref() {
        write_cover_cache(path, &rgba);
    }
    Ok(rgba)
}

/// 下载封面并**在后台线程**完成解码 + 缩放 + 网纹，返回可直接上传的 RGBA。
///
/// 为什么不返回原始字节：解码一张 640×640 JPEG 再缩到 96×96 并施加网纹是
/// 纯 CPU 活，放在 UI 线程会让那一帧变长十几到几十毫秒——正好落在换曲的
/// 舱体动画上，看起来就是"卡顿"。工作线程本来就为网络等待空转，顺路做掉。
async fn fetch_cover_rgba(url: &str) -> Result<Vec<u8>, ProviderError> {
    let bytes = fetch_cover_bytes(url).await?;
    super::halftone::halftone_cover_bytes(&bytes, super::halftone::COVER_PX)
        .ok_or(ProviderError::UnsupportedResponse)
}

/// 封面下载共用的 HTTP 客户端。
///
/// 必须共享：一屏磁带架一次就要发十几张封面，每次 `Client::builder()` 都换一个
/// 新连接池，等于每张图重新做一次 TLS 握手——同一个 CDN 明明一条 keep-alive
/// 连接就能跑完。共享之后只有第一批是冷的。
fn cover_client() -> Result<&'static reqwest::Client, ProviderError> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .pool_idle_timeout(std::time::Duration::from_secs(90))
                .build()
                .ok()
        })
        .as_ref()
        .ok_or(ProviderError::NetworkUnavailable)
}

async fn fetch_cover_bytes(url: &str) -> Result<Vec<u8>, ProviderError> {
    let client = cover_client()?;
    let response = client
        .get(url)
        .timeout(std::time::Duration::from_secs(COVER_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|e| {
            tracing::warn!("封面下载失败: {e}");
            classify_reqwest(&e)
        })?;
    if !response.status().is_success() {
        return Err(ProviderError::Unknown(format!(
            "HTTP {}",
            response.status()
        )));
    }
    let bytes = response.bytes().await.map_err(|e| classify_reqwest(&e))?;
    if bytes.len() > MAX_COVER_BYTES {
        tracing::warn!("封面过大（{} B），放弃", bytes.len());
        return Err(ProviderError::UnsupportedResponse);
    }
    Ok(bytes.to_vec())
}

/// 便捷：为只测桥逻辑的调用方提供「无下载器」的桩。
///
/// 桥的歌词/搜索等请求不触碰下载器；只有音频装载需要。
#[allow(dead_code)]
pub fn noop_fetcher() -> Arc<dyn DynAudioFetcher> {
    struct Noop;
    impl crate::music::online_playback::AudioFetcher for Noop {
        async fn fetch_to_file(&self, _url: &str, _dest: &Path) -> Result<u64, ProviderError> {
            Err(ProviderError::PlaybackUrlUnavailable)
        }
    }
    Arc::new(Noop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::music::online_playback::AudioFetcher;
    use crate::music::provider::{
        LoginState, MusicProvider, PlaybackSource, ProviderId, QrLoginSession, TrackAvailability,
    };
    use std::time::Duration;

    /// 可脚本化 Mock：验证桥的正常/异常路径与序列号透传。
    struct BridgeMock {
        fail: bool,
        /// 前 N 次 `search_songs` 强制返回可重试错误（验证有限重试）。
        ///
        /// 工作线程是串行的，`load` + `fetch_sub` 不存在竞态。
        flaky: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl MusicProvider for BridgeMock {
        fn id(&self) -> ProviderId {
            ProviderId::Netease
        }

        async fn search_songs(
            &self,
            keyword: &str,
            _limit: u32,
        ) -> Result<Vec<Track>, ProviderError> {
            if self
                .flaky
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |n| n.checked_sub(1),
                )
                .is_ok()
            {
                // 模拟"复用到对端已关闭的空闲连接"这类瞬时故障
                return Err(ProviderError::ConnectionReset);
            }
            if self.fail {
                // 用**不可重试**错误：本用例只验证错误类型透传，不该被重试拖慢
                Err(ProviderError::InvalidData)
            } else {
                Ok(vec![Track {
                    provider: ProviderId::Netease,
                    id: "1".into(),
                    title: keyword.to_string(),
                    artists: vec![],
                    album: None,
                    duration_ms: Some(1000),
                    cover_url: None,
                    ..Track::default()
                }])
            }
        }

        async fn song_details(&self, ids: &[String]) -> Result<Vec<Track>, ProviderError> {
            if ids.is_empty() {
                return Err(ProviderError::InvalidParameter);
            }
            Ok(ids
                .iter()
                .map(|id| Track {
                    provider: ProviderId::Netease,
                    id: id.clone(),
                    title: format!("d{}", id),
                    artists: vec![],
                    album: None,
                    duration_ms: None,
                    cover_url: None,
                    ..Track::default()
                })
                .collect())
        }

        async fn playlist(
            &self,
            playlist_id: &str,
            _offset: u32,
            _limit: u32,
        ) -> Result<Playlist, ProviderError> {
            if self.fail {
                Err(ProviderError::ContentUnavailable)
            } else {
                Ok(Playlist {
                    provider: ProviderId::Netease,
                    id: playlist_id.to_string(),
                    name: Some("mock".into()),
                    total: Some(0),
                    tracks: vec![],
                })
            }
        }
        async fn daily_songs(&self) -> Result<Vec<Track>, ProviderError> {
            if self.fail {
                return Err(ProviderError::NetworkUnavailable);
            }
            Ok(vec![Track {
                provider: ProviderId::Netease,
                id: "daily-mock".into(),
                title: "daily mock".into(),
                artists: vec!["a".into()],
                ..Track::default()
            }])
        }
        async fn intelligence_list(
            &self,
            song_id: &str,
            _: &str,
            _: &str,
            _: u32,
        ) -> Result<Vec<Track>, ProviderError> {
            if self.fail {
                return Err(ProviderError::NetworkUnavailable);
            }
            Ok(vec![Track {
                provider: ProviderId::Netease,
                id: format!("intel-{song_id}"),
                title: "intel mock".into(),
                artists: vec!["a".into()],
                ..Track::default()
            }])
        }
        async fn search_match(
            &self,
            title: &str,
            _: &str,
            _: &str,
            _: u64,
            md5: &str,
        ) -> Result<Vec<Track>, ProviderError> {
            if self.fail {
                return Err(ProviderError::NetworkUnavailable);
            }
            if title.trim().is_empty() || md5.trim().is_empty() {
                return Err(ProviderError::InvalidParameter);
            }
            Ok(vec![Track {
                provider: ProviderId::Netease,
                id: "match-mock".into(),
                title: title.into(),
                artists: vec!["a".into()],
                cover_url: Some("https://pic/mock.jpg".into()),
                ..Track::default()
            }])
        }

        async fn featured_playlists(
            &self,
            kind: crate::music::provider::FeaturedKind,
            limit: u32,
        ) -> Result<Vec<crate::music::provider::PlaylistSummary>, ProviderError> {
            if self.fail {
                Err(ProviderError::NetworkUnavailable)
            } else if limit == 0 {
                Err(ProviderError::InvalidParameter)
            } else {
                Ok(vec![crate::music::provider::PlaylistSummary {
                    provider: ProviderId::Netease,
                    id: "9001".into(),
                    name: format!("mock-{:?}", kind),
                    cover_url: None,
                    track_count: Some(1),
                    play_count: None,
                    description: None,
                }])
            }
        }

        async fn check_availability(
            &self,
            _song_id: &str,
        ) -> Result<TrackAvailability, ProviderError> {
            Ok(TrackAvailability::Available)
        }

        async fn playback_source(&self, _song_id: &str) -> Result<PlaybackSource, ProviderError> {
            // 模拟慢速取址（网络延迟），用于取消测试
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(PlaybackSource {
                url: "http://cdn/mock.mp3".into(),
                expires_at: None,
                quality: None,
                content_type: Some("mp3".into()),
                bitrate_kbps: None,
            })
        }

        async fn lyrics(&self, _song_id: &str) -> Result<Lyrics, ProviderError> {
            if self.fail {
                Err(ProviderError::RequestTimeout)
            } else {
                Ok(Lyrics {
                    kind: crate::music::lyrics::LyricKind::Plain,
                    lines: vec![crate::music::lyrics::LyricLine {
                        start_ms: 0,
                        text: "第一行".into(),
                    }],
                    offset_ms: 0,
                    dropped_lines: 0,
                })
            }
        }

        async fn login_status(&self) -> Result<LoginState, ProviderError> {
            if self.fail {
                Err(ProviderError::RequestTimeout)
            } else {
                Ok(LoginState::Anonymous)
            }
        }

        async fn qr_login_start(&self) -> Result<QrLoginSession, ProviderError> {
            if self.fail {
                Err(ProviderError::RequestTimeout)
            } else {
                Ok(QrLoginSession {
                    key: "mock-key".into(),
                    qr_url: "https://example.invalid/qr".into(),
                })
            }
        }

        async fn qr_login_poll(&self, _key: &str) -> Result<LoginState, ProviderError> {
            if self.fail {
                Err(ProviderError::RequestTimeout)
            } else {
                Ok(LoginState::QrWaiting)
            }
        }

        async fn login_cellphone(
            &self,
            _phone: &str,
            _password: &str,
        ) -> Result<LoginState, ProviderError> {
            if self.fail {
                Err(ProviderError::InvalidParameter)
            } else {
                Ok(LoginState::LoggedIn {
                    nickname: Some("phone-user".into()),
                    user_id: Some("2".into()),
                })
            }
        }

        async fn login_cookie(&self, _cookie: &str) -> Result<LoginState, ProviderError> {
            if self.fail {
                Err(ProviderError::InvalidParameter)
            } else {
                Ok(LoginState::LoggedIn {
                    nickname: Some("cookie-user".into()),
                    user_id: Some("3".into()),
                })
            }
        }

        async fn logout(&self) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    /// 下载桩：写入最小 mp3 字节或报错。
    struct FetchStub;

    impl AudioFetcher for FetchStub {
        async fn fetch_to_file(&self, _url: &str, dest: &Path) -> Result<u64, ProviderError> {
            let bytes = b"ID3\x03\x00mock-audio";
            tokio::fs::write(dest, bytes)
                .await
                .map_err(|e| ProviderError::Unknown(format!("写盘失败: {}", e)))?;
            Ok(bytes.len() as u64)
        }
    }

    /// 慢下载桩：用于验证取消能中断在途下载。
    ///
    /// 载荷写成 5 字节的合法 ID3 头而不是裸 `b"ID3"`：`looks_like_audio`
    /// 要求至少 4 字节，3 字节会被判成非音频，让"不打断预取"这类**要跑完**
    /// 的用例拿到 `UnsupportedResponse` 而不是文件。
    struct SlowFetchStub;

    impl AudioFetcher for SlowFetchStub {
        async fn fetch_to_file(&self, _url: &str, dest: &Path) -> Result<u64, ProviderError> {
            tokio::time::sleep(Duration::from_millis(600)).await;
            let payload = b"ID3\x03\x00\x00";
            tokio::fs::write(dest, payload)
                .await
                .map_err(|e| ProviderError::Unknown(format!("写盘失败: {}", e)))?;
            Ok(payload.len() as u64)
        }
    }

    fn spawn_mock(fail: bool) -> ProviderBridge {
        ProviderBridge::spawn(
            Arc::new(BridgeMock {
                fail,
                flaky: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
            Arc::new(FetchStub),
        )
    }

    /// 前 `n` 次搜索失败（可重试错误），之后成功。
    fn spawn_flaky(n: usize) -> ProviderBridge {
        ProviderBridge::spawn(
            Arc::new(BridgeMock {
                fail: false,
                flaky: Arc::new(std::sync::atomic::AtomicUsize::new(n)),
            }),
            Arc::new(FetchStub),
        )
    }

    /// 轮询直到收到响应或超时（工作线程是异步的，不能假设瞬时完成）。
    fn recv_with_timeout(bridge: &ProviderBridge) -> Response {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(resp) = bridge.poll() {
                return resp;
            }
            if std::time::Instant::now() > deadline {
                panic!("等待桥响应超时");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn search_round_trip_preserves_seq_and_payload() {
        let mut bridge = spawn_mock(false);
        let seq = bridge.search("海阔天空", 5);
        let resp = recv_with_timeout(&bridge);
        assert_eq!(resp.seq(), seq);
        match resp {
            Response::Search { result, .. } => {
                let tracks = result.unwrap();
                assert_eq!(tracks[0].title, "海阔天空");
            }
            other => panic!("期望搜索响应，得到 {:?}", other),
        }
    }

    #[test]
    fn seq_increments_per_request() {
        let mut bridge = spawn_mock(false);
        let s1 = bridge.search("a", 1);
        let s2 = bridge.song_details(vec!["2".into()]);
        let s3 = bridge.playlist("3", 0, 10);
        let s4 = bridge.prepare_track("4");
        let s5 = bridge.lyrics("5");
        assert!(s1 < s2 && s2 < s3 && s3 < s4 && s4 < s5, "序列号应严格递增");
    }

    #[test]
    fn provider_errors_pass_through_typed() {
        let mut bridge = spawn_mock(true);
        bridge.search("x", 1);
        let resp = recv_with_timeout(&bridge);
        match resp {
            Response::Search { result, .. } => {
                assert_eq!(result.unwrap_err(), ProviderError::InvalidData)
            }
            other => panic!("期望搜索响应，得到 {:?}", other),
        }

        bridge.playlist("9", 0, 5);
        let resp = recv_with_timeout(&bridge);
        match resp {
            Response::Playlist { result, .. } => {
                assert_eq!(result.unwrap_err(), ProviderError::ContentUnavailable)
            }
            other => panic!("期望歌单响应，得到 {:?}", other),
        }
    }

    #[test]
    fn transient_connection_reset_is_retried_then_succeeds() {
        // 前 2 次 ConnectionReset（可重试），第 3 次成功：
        // 这正是「偶发网络错误弹窗」要消掉的场景——用户不该看到它。
        let mut bridge = spawn_flaky(2);
        let seq = bridge.search("retry-me", 3);
        let resp = recv_with_timeout(&bridge);
        assert_eq!(resp.seq(), seq);
        match resp {
            Response::Search { result, .. } => {
                let tracks = result.expect("重试后应成功，而不是把瞬时故障抛给用户");
                assert_eq!(tracks[0].title, "retry-me");
            }
            other => panic!("期望搜索响应，得到 {:?}", other),
        }
    }

    #[test]
    fn retry_budget_is_finite_and_surfaces_last_error() {
        // 失败次数超过重试预算：必须把最后一次的错误如实交回，不能无限重试
        let mut bridge = spawn_flaky(usize::MAX);
        bridge.search("never", 1);
        let resp = recv_with_timeout(&bridge);
        match resp {
            Response::Search { result, .. } => {
                assert_eq!(result.unwrap_err(), ProviderError::ConnectionReset)
            }
            other => panic!("期望搜索响应，得到 {:?}", other),
        }
    }

    #[test]
    fn closed_connection_mid_request_classifies_as_retryable_reset() {
        // 真实 TCP 层回归：这就是「偶发网络错误」的物理形态。
        // 对端在写出任何字节前关闭连接（keep-alive 空闲连接被回收），
        // reqwest 只置 is_request()，其余谓词全为 false。
        // 历史 bug：旧判据 `is_connect() || is_request()` 把它当 NetworkUnavailable，
        // 于是弹窗劝用户「检查网络连接」——而用户的网络根本没有问题。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let addr = listener.local_addr().expect("取端口");
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                drop(stream); // 接受后立刻断开，不回任何字节
            }
        });

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("建 runtime");
        let url = format!("http://{addr}/x.jpg");
        let err = rt
            .block_on(async { fetch_cover_bytes(&url).await })
            .expect_err("对端已关闭，必须报错");

        assert_eq!(
            err,
            ProviderError::ConnectionReset,
            "对端关闭连接应归类为 ConnectionReset，而不是网络不可用"
        );
        assert!(err.is_retryable(), "这类断连是瞬时故障，必须纳入重试预算");
    }

    #[test]
    fn multiple_requests_arrive_with_matching_seqs() {
        // 连发三个请求，响应的 seq 必须与请求一一对得上（过期丢弃的前提）
        let mut bridge = spawn_mock(false);
        let seqs = [
            bridge.search("a", 1),
            bridge.search("b", 1),
            bridge.search("c", 1),
        ];
        let mut got = Vec::new();
        for _ in 0..3 {
            got.push(recv_with_timeout(&bridge).seq());
        }
        got.sort_unstable();
        let mut expected = seqs.to_vec();
        expected.sort_unstable();
        assert_eq!(got, expected);
    }

    #[test]
    fn cellphone_login_round_trips_and_returns_logged_in() {
        let mut bridge = spawn_mock(false);
        let seq = bridge.login_cellphone("13800138000", "secret");
        let resp = recv_with_timeout(&bridge);
        assert_eq!(resp.seq(), seq);
        match resp {
            Response::CellphoneLoggedIn { result, .. } => {
                assert!(matches!(result.unwrap(), LoginState::LoggedIn { .. }));
            }
            other => panic!("期望 CellphoneLoggedIn，得到 {:?}", other),
        }
    }

    #[test]
    fn cookie_login_and_logout_round_trip() {
        let mut bridge = spawn_mock(false);
        let cookie_seq = bridge.login_cookie("MUSIC_U=abc; __csrf=xyz");
        match recv_with_timeout(&bridge) {
            Response::CookieLoggedIn { seq, result } => {
                assert_eq!(seq, cookie_seq);
                assert!(matches!(result.unwrap(), LoginState::LoggedIn { .. }));
            }
            other => panic!("期望 CookieLoggedIn，得到 {:?}", other),
        }
        let logout_seq = bridge.logout();
        match recv_with_timeout(&bridge) {
            Response::LoggedOut { seq, result } => {
                assert_eq!(seq, logout_seq);
                assert!(result.is_ok());
            }
            other => panic!("期望 LoggedOut，得到 {:?}", other),
        }
    }

    #[test]
    fn prepare_track_downloads_and_returns_prepared() {
        let mut bridge = spawn_mock(false);
        let seq = bridge.prepare_track("347230");
        let resp = recv_with_timeout(&bridge);
        assert_eq!(resp.seq(), seq);
        match resp {
            Response::Prepared { result, .. } => {
                let prepared = result.expect("装载应成功");
                assert!(prepared.path.exists(), "临时音频文件应存在");
                let _ = std::fs::remove_file(&prepared.path);
            }
            other => panic!("期望 Prepared 响应，得到 {:?}", other),
        }
    }

    #[test]
    fn lyrics_round_trip_returns_parsed_lines() {
        let mut bridge = spawn_mock(false);
        let seq = bridge.lyrics("347230");
        let resp = recv_with_timeout(&bridge);
        assert_eq!(resp.seq(), seq);
        match resp {
            Response::Lyrics { result, .. } => {
                let lyrics = result.unwrap();
                assert_eq!(lyrics.lines[0].text, "第一行");
            }
            other => panic!("期望 Lyrics 响应，得到 {:?}", other),
        }
    }

    #[test]
    fn cancel_pending_aborts_inflight_prepare() {
        // 慢下载桩：取消必须中断在途装载并回执 Cancelled
        let mut bridge = ProviderBridge::spawn(
            Arc::new(BridgeMock {
                fail: false,
                flaky: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
            Arc::new(SlowFetchStub),
        );
        let seq = bridge.prepare_track("1");
        // 给工作线程一点时间进入 future
        std::thread::sleep(Duration::from_millis(80));
        bridge.cancel_pending();
        let resp = recv_with_timeout(&bridge);
        assert_eq!(resp.seq(), seq);
        assert!(
            matches!(resp, Response::Cancelled { .. }),
            "取消必须回执 Cancelled，得到 {:?}",
            resp
        );
    }

    #[test]
    fn cancel_does_not_affect_new_requests() {
        let mut bridge = spawn_mock(false);
        let old_seq = bridge.lyrics("old");
        bridge.cancel_pending();
        let new_seq = bridge.lyrics("new");
        assert!(new_seq > old_seq);

        let mut cancelled = 0;
        let mut ok = 0;
        for _ in 0..2 {
            match recv_with_timeout(&bridge) {
                Response::Cancelled { seq } => {
                    assert_eq!(seq, old_seq, "只有旧请求可被取消");
                    cancelled += 1;
                }
                Response::Lyrics { seq, result } => {
                    assert_eq!(seq, new_seq);
                    assert!(result.is_ok());
                    ok += 1;
                }
                other => panic!("意外响应 {:?}", other),
            }
        }
        assert_eq!((cancelled, ok), (1, 1));
    }

    #[test]
    fn queued_requests_are_skipped_when_cancelled() {
        // 取消后排队中的旧请求不得执行（直接回执取消）
        let mut bridge = ProviderBridge::spawn(
            Arc::new(BridgeMock {
                fail: false,
                flaky: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
            Arc::new(SlowFetchStub),
        );
        let s1 = bridge.prepare_track("1"); // 会被慢下载拖住
        std::thread::sleep(Duration::from_millis(50));
        let s2 = bridge.lyrics("2"); // 排在后面
        bridge.cancel_pending();

        let mut seen = Vec::new();
        for _ in 0..2 {
            seen.push(recv_with_timeout(&bridge).seq());
        }
        seen.sort_unstable();
        assert_eq!(seen, vec![s1, s2], "两个请求都要有回执");
        // 至少排头的请求是被中断的
        let first = recv_nothing(&bridge);
        assert!(first.is_none(), "不应有多余响应");
    }

    #[test]
    fn cancel_audio_leaves_metadata_requests_running() {
        // 音频水位是独立的：掐下载**不得**连带作废歌单/歌词/搜索。
        // 复用全局水位的话，用户点下一首会看到"整页列表重新加载"。
        let mut bridge = ProviderBridge::spawn(
            Arc::new(BridgeMock {
                fail: false,
                flaky: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
            Arc::new(SlowFetchStub),
        );
        let audio_seq = bridge.prepare_track("1");
        std::thread::sleep(Duration::from_millis(80));
        let lyric_seq = bridge.lyrics("2");
        bridge.cancel_audio();

        let mut cancelled = 0;
        let mut lyrics_ok = 0;
        for _ in 0..2 {
            match recv_with_timeout(&bridge) {
                Response::Cancelled { seq } => {
                    assert_eq!(seq, audio_seq, "只有音频请求可被音频水位取消");
                    cancelled += 1;
                }
                Response::Lyrics { seq, result } => {
                    assert_eq!(seq, lyric_seq);
                    assert!(result.is_ok(), "歌词请求不得被音频水位作废");
                    lyrics_ok += 1;
                }
                other => panic!("意外响应 {:?}", other),
            }
        }
        assert_eq!((cancelled, lyrics_ok), (1, 1));
    }

    #[test]
    fn cancel_audio_aborts_the_inflight_and_skips_the_queued_download() {
        // 在途的那首被 select! 打断，排队的那首直接跳过——两条都要有回执，
        // 否则 UI 侧会留下两个永远等不到结果的 seq。
        let mut bridge = ProviderBridge::spawn(
            Arc::new(BridgeMock {
                fail: false,
                flaky: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
            Arc::new(SlowFetchStub),
        );
        let s1 = bridge.prepare_track("1"); // 在途
        std::thread::sleep(Duration::from_millis(50));
        let s2 = bridge.prepare_track("2"); // 排队
        bridge.cancel_audio();

        for _ in 0..2 {
            match recv_with_timeout(&bridge) {
                Response::Cancelled { seq } => assert!(seq == s1 || seq == s2),
                other => panic!("两个装载请求都应被取消，得到 {:?}", other),
            }
        }
        assert!(recv_nothing(&bridge).is_none(), "不应有多余响应");
    }

    #[test]
    fn cancel_audio_does_not_affect_the_next_request() {
        // 换台的顺序不变式：cancel_audio() 必须在分配新 seq **之前**。
        // 水位取当前计数器，新 seq 恰好等于它，而 wait_cancelled 用严格 `>`。
        let mut bridge = ProviderBridge::spawn(
            Arc::new(BridgeMock {
                fail: false,
                flaky: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
            Arc::new(SlowFetchStub),
        );
        let old = bridge.prepare_track("1");
        std::thread::sleep(Duration::from_millis(80));
        bridge.cancel_audio();
        let new = bridge.prepare_track("2");
        assert!(new > old);
        assert_eq!(bridge.audio_watermark(), new, "新 seq 必须恰好等于水位");

        match recv_with_timeout(&bridge) {
            Response::Cancelled { seq } => assert_eq!(seq, old),
            other => panic!("旧装载应被取消，得到 {:?}", other),
        }
        match recv_with_timeout(&bridge) {
            Response::Prepared { seq, result } => {
                assert_eq!(seq, new, "新请求不得被自己的水位取消");
                if let Ok(prepared) = result {
                    let _ = std::fs::remove_file(&prepared.path);
                }
            }
            other => panic!("新装载应正常完成，得到 {:?}", other),
        }
    }

    #[test]
    fn prefetch_returns_a_distinct_response_variant() {
        // 预取的 seq 永远不等于 `PlayerCore::prepare_seq`，但"落进其它分支"
        // 这种事故只能靠类型挡住：回执必须是 Prefetched，不是 Prepared。
        let mut bridge = spawn_mock(false);
        let seq = bridge.prefetch_track("347230");
        match recv_with_timeout(&bridge) {
            Response::Prefetched {
                seq: got,
                song_id,
                result,
            } => {
                assert_eq!(got, seq);
                assert_eq!(song_id, "347230", "回执必须带回原歌 ID");
                if let Ok(prepared) = result {
                    assert!(prepared.path.exists(), "预取也要落成可播放的临时文件");
                    let _ = std::fs::remove_file(&prepared.path);
                }
            }
            other => panic!("预取必须回执 Prefetched，得到 {:?}", other),
        }
    }

    #[test]
    fn a_play_for_the_same_song_does_not_cancel_its_prefetch() {
        // 点播的就是正在预取的那一首：掐掉它等于把已经下了一半的字节扔掉，
        // 再从零下一遍。这是"预取命中"唯一能省钱的地方，必须不打断。
        let mut bridge = ProviderBridge::spawn(
            Arc::new(BridgeMock {
                fail: false,
                flaky: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
            Arc::new(SlowFetchStub),
        );
        let prefetch_seq = bridge.prefetch_track("same");
        std::thread::sleep(Duration::from_millis(80));
        let play_seq = bridge.prepare_track("same");

        let mut seen_prefetch = false;
        let mut seen_play = false;
        for _ in 0..2 {
            match recv_with_timeout(&bridge) {
                Response::Prefetched { seq, result, .. } => {
                    assert_eq!(seq, prefetch_seq);
                    assert!(result.is_ok(), "预取不得被同曲点播掐断");
                    seen_prefetch = true;
                }
                Response::Prepared { seq, result } => {
                    assert_eq!(seq, play_seq);
                    assert!(result.is_ok());
                    if let Ok(prepared) = result {
                        let _ = std::fs::remove_file(&prepared.path);
                    }
                    seen_play = true;
                }
                other => panic!("不应出现取消或意外响应: {:?}", other),
            }
        }
        assert!(seen_prefetch && seen_play, "两个请求都要有终态回执");
    }

    #[test]
    fn a_play_for_a_different_song_cancels_the_running_prefetch() {
        // 换到别的歌：预取的那份字节已经不是马上要用的了，继续下完会让
        // 点播排在一个几十秒的下载后面——必须立刻让出 lane。
        let mut bridge = ProviderBridge::spawn(
            Arc::new(BridgeMock {
                fail: false,
                flaky: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
            Arc::new(SlowFetchStub),
        );
        let prefetch_seq = bridge.prefetch_track("old");
        std::thread::sleep(Duration::from_millis(80));
        let play_seq = bridge.prepare_track("new");

        match recv_with_timeout(&bridge) {
            Response::Cancelled { seq } => assert_eq!(seq, prefetch_seq, "被掐的必须是预取那条"),
            other => panic!("异曲点播应先取消预取，得到 {:?}", other),
        }
        match recv_with_timeout(&bridge) {
            Response::Prepared { seq, result } => {
                assert_eq!(seq, play_seq, "点播不得被自己的水位取消");
                if let Ok(prepared) = result {
                    let _ = std::fs::remove_file(&prepared.path);
                }
            }
            other => panic!("点播应正常完成，得到 {:?}", other),
        }
    }

    /// 本地匹配素材（`local_match_probe`）：标题走文件名约定，摘要非空。
    ///
    /// 用无标签的最小 WAV：元数据读不出标题时必须**降级**成文件名拆出来的字，
    /// 而不是整个匹配请求消失——用户看到的是一张没有封面的碟，而不是一句错误。
    #[test]
    fn local_match_probe_falls_back_to_the_filename_convention() {
        let dir = std::env::temp_dir().join(format!(
            "reimu-player-bridge-probe-{}-{}",
            std::process::id(),
            "filename"
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = crate::music::media::minimal_wav(&dir, "YOASOBI - 夜に駆ける.wav", 3);

        let probe = local_match_probe(&path.to_string_lossy()).expect("应能凑出匹配素材");
        assert_eq!(probe.title, "夜に駆ける");
        assert_eq!(probe.artist, "YOASOBI");
        assert_eq!(
            probe.duration_secs, 3,
            "时长必须是整秒（小数会被服务端判 0）"
        );
        assert_eq!(probe.md5.len(), 32, "空摘要会被服务端判为无匹配");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 时长未知 → 不发请求（`0` 秒在服务端必然查无此曲，白占一次带重试的往返）。
    #[test]
    fn local_match_probe_skips_a_file_without_a_duration() {
        let dir = std::env::temp_dir().join(format!(
            "reimu-player-bridge-probe-{}-{}",
            std::process::id(),
            "noduration"
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let junk = dir.join("YOASOBI - 夜に駆ける.mp3");
        std::fs::write(&junk, b"not really audio").unwrap();
        assert!(local_match_probe(&junk.to_string_lossy()).is_none());
        assert!(local_match_probe(&dir.join("missing.mp3").to_string_lossy()).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 封面地址必须带上缩放参数——原图能到 5 MB，而界面只画 96 px 的网点。
    #[test]
    fn cover_urls_are_asked_at_thumbnail_size() {
        assert_eq!(
            sized_cover_url("http://p3.music.126.net/abc/def.jpg"),
            format!(
                "http://p3.music.126.net/abc/def.jpg?param={COVER_REQUEST_PX}y{COVER_REQUEST_PX}"
            )
        );
        // 已有查询串的不动：可能是别人指定的质量/裁剪参数
        assert_eq!(
            sized_cover_url("http://p3.music.126.net/abc/def.jpg?param=64y64"),
            "http://p3.music.126.net/abc/def.jpg?param=64y64"
        );
        // 不是图片扩展名的（短链、接口回传的奇怪地址）原样交给网络层
        assert_eq!(
            sized_cover_url("http://music.163.com/api/playlist/123"),
            "http://music.163.com/api/playlist/123"
        );
        // 大写扩展名同样认
        assert!(
            sized_cover_url("https://p1.music.126.net/x/y.JPG").ends_with(".JPG?param=200y200"),
            "扩展名判断不能大小写敏感"
        );
    }

    /// 一个永远回 404 的本地图片服务器：让封面请求**即时且确定地**失败。
    ///
    /// 不用"不存在的端口"：那种地址在某些环境下不会立刻拒绝，SYN 被悄悄丢掉、
    /// 一直等到超时才回，测出来的耗时根本不是被测代码的耗时。
    fn stub_404_cover_server() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let addr = listener.local_addr().expect("取端口");
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                use std::io::Write;
                let mut stream = stream;
                let _ = stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                let _ = stream.flush();
            }
        });
        format!("http://{addr}")
    }

    /// 封面不得排在串行队列后面：这是"列表出来了，封面一格一格慢慢点灯"的判据。
    ///
    /// 串行 worker 被一次 600 ms 的音频装载占住时，旧实现里 20 张封面只能依次
    /// 排队（合计十几秒）；现在它们走自己的线程池，第一张应在毫秒级回来。
    /// 一首歌的下载不能拖住任何元数据请求——这是"点了歌之后列表转圈"的根因判据。
    ///
    /// 旧结构里 `PrepareTrack` 排在元数据队列上：装载要几秒（慢桩 600 ms），
    /// 期间歌词、搜索、歌单全部原地等待。现在它们各走各的线程。
    #[test]
    fn an_audio_download_does_not_stall_metadata_requests() {
        let mut bridge = ProviderBridge::spawn(
            Arc::new(BridgeMock {
                fail: false,
                flaky: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
            Arc::new(SlowFetchStub), // 装载固定耗时 600 ms
        );
        let prep = bridge.prepare_track("1");
        std::thread::sleep(Duration::from_millis(50));
        let lyric = bridge.lyrics("2");
        let search = bridge.search("海阔天空", 1);

        let started = std::time::Instant::now();
        let mut fast = Vec::new();
        while fast.len() < 2 {
            match bridge.poll() {
                Some(resp) => fast.push(resp.seq()),
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "两条元数据请求应在下载完成前就回来，实际等了 {:?}",
            started.elapsed()
        );
        assert!(
            fast.contains(&lyric) && fast.contains(&search),
            "回执不匹配"
        );
        // 下载随后也要正常回执，不能因为换了线程就丢掉结果
        let resp = recv_with_timeout(&bridge);
        assert_eq!(resp.seq(), prep);
        assert!(matches!(resp, Response::Prepared { .. }), "得到 {:?}", resp);
    }

    /// 下载中途换台：装载线程必须按同一个取消水位打断，并扫走半成品。
    /// （单线程打断的既有回归见 [`cancel_pending_aborts_inflight_prepare`]，
    /// 那条现在跑在装载线程上，语义没变。）
    #[test]
    fn cancelling_an_inflight_download_leaves_the_lane_usable() {
        let mut bridge = ProviderBridge::spawn(
            Arc::new(BridgeMock {
                fail: false,
                flaky: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
            Arc::new(SlowFetchStub),
        );
        let dropped = bridge.prepare_track("1");
        std::thread::sleep(Duration::from_millis(80));
        bridge.cancel_pending();
        let next = bridge.prepare_track("2"); // 换台后立刻点下一首

        let mut got = Vec::new();
        for _ in 0..2 {
            got.push(recv_with_timeout(&bridge).seq());
        }
        assert!(got.contains(&dropped), "被取消的装载要有回执");
        assert!(
            got.contains(&next),
            "新点的装载绝不能因为上一条还在收尾而被吞掉"
        );
    }

    #[test]
    fn cover_requests_do_not_queue_behind_the_serial_worker() {
        let mut bridge = ProviderBridge::spawn(
            Arc::new(BridgeMock {
                fail: false,
                flaky: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
            Arc::new(SlowFetchStub),
        );
        let base = stub_404_cover_server();
        let prep = bridge.prepare_track("1"); // 占住串行 worker
        std::thread::sleep(Duration::from_millis(50));
        let cover = bridge.cover_image(format!("{base}/nope.jpg"));

        let started = std::time::Instant::now();
        let resp = recv_with_timeout(&bridge);
        assert_eq!(
            resp.seq(),
            cover,
            "封面回执抢在串行装载之前，说明它没排那条队"
        );
        assert!(
            started.elapsed() < Duration::from_millis(550),
            "封面回执应当是毫秒级，实际 {:?}——排到串行队列上了？",
            started.elapsed()
        );
        match resp {
            Response::CoverImage { result, .. } => {
                assert!(result.is_err(), "404 必须如实回执失败，不能吞掉");
            }
            other => panic!("期望 CoverImage 响应，得到 {:?}", other),
        }
        assert!(bridge.poll().is_none(), "封面池不该替串行请求产出多余回执");
        let _ = prep;
    }

    /// 灌满封面池也不会丢请求：每一个 seq 都必须拿到一个终态回执。
    ///
    /// 静默吞掉是这里最坏的失败模式——UI 把那个 URL 记成"在途"，于是这一格
    /// 封面永远点不亮，也不会重试，用户只看到磁带架上少了几张图却说不出为什么。
    #[test]
    fn every_cover_request_gets_a_terminal_response() {
        let mut bridge = ProviderBridge::spawn(
            Arc::new(BridgeMock {
                fail: false,
                flaky: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
            Arc::new(SlowFetchStub),
        );
        // 超出 4 条 × 16 格的总容量，逼出溢出路径
        let base = stub_404_cover_server();
        let total = COVER_WORKERS * COVER_QUEUE_CAP + 1;
        let seqs: Vec<u64> = (0..total)
            .map(|i| bridge.cover_image(format!("{base}/{i}.jpg")))
            .collect();

        let mut got = std::collections::HashSet::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while got.len() < total && std::time::Instant::now() < deadline {
            match bridge.poll() {
                Some(Response::CoverImage { seq, .. }) | Some(Response::Cancelled { seq }) => {
                    got.insert(seq);
                }
                Some(other) => panic!("封面池不该产出别的响应：{other:?}"),
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        assert_eq!(
            got.len(),
            total,
            "每个封面请求都要有回执（下载结果或取消），漏一个就是一格永久空白"
        );
        assert!(
            seqs.iter().all(|s| got.contains(s)),
            "回执的序列号必须落在请求的那批里"
        );
    }

    /// 磁盘缓存：写进去的必须原样读得出来；尺寸不对的一律当没有。
    #[test]
    fn cover_disk_cache_round_trips_and_rejects_bad_lengths() {
        let dir = temp_dir("cover-cache");
        let path = dir.join("000000000000000a.bin");
        let rgba = vec![7u8; COVER_CACHE_FILE_BYTES];

        write_cover_cache(&path, &rgba);
        assert_eq!(
            read_cover_cache(&path).as_deref(),
            Some(rgba.as_slice()),
            "缓存读回来的字节必须与写入的一致"
        );

        // 半截文件（写到一半掉电）不能当封面用——那会画出一格花屏。
        std::fs::write(&path, &rgba[..rgba.len() / 2]).unwrap();
        assert!(read_cover_cache(&path).is_none(), "长度不对必须判为未命中");

        // 尺寸不对的写入直接丢弃，且不能污染既有缓存文件。
        std::fs::write(&path, &rgba).unwrap();
        write_cover_cache(&path, b"nope");
        assert_eq!(read_cover_cache(&path).as_deref(), Some(rgba.as_slice()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 缓存只能无限增长到某个上限，之后按最久未用修剪。
    #[test]
    fn cover_disk_cache_is_pruned_to_its_cap() {
        let dir = temp_dir("cover-prune");
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..COVER_DISK_CACHE_MAX + 8 {
            std::fs::write(dir.join(format!("{:016x}.bin", i)), b"x").unwrap();
        }
        // 非缓存文件不该被顺手删掉（同目录可能有别的用途）。
        std::fs::write(dir.join("keep.txt"), b"x").unwrap();

        prune_cover_cache(&dir);

        let bins = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("bin"))
            .count();
        assert!(
            bins <= COVER_DISK_CACHE_MAX / 2,
            "修剪后应明显低于上限，实际 {bins}"
        );
        assert!(dir.join("keep.txt").exists(), "只动 .bin，别碰邻居");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一个进程内独立的临时目录（测试之间不能共用同名目录）。
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "reimu-player-bridge-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn recv_nothing(bridge: &ProviderBridge) -> Option<Response> {
        std::thread::sleep(Duration::from_millis(50));
        bridge.poll()
    }
}
