//! 在线音频取址与装载（阶段 C / M3 播放链路接入）。
//!
//! 职责：把「在线歌曲 ID」变成「可交给统一 [`crate::music::PlaybackController`]
//! 的本地媒体」，并处理播放地址的**过期、无效与有限重试**。
//!
//! 两种交付形态（由 [`PreparedTrack::stream`] 区分，调用方据此选装载入口）：
//!
//! - **整首**（传统）：取址 → 下载到临时文件 → 校验 → 交路径。
//! - **边下边播**（方案 2A）：取址 → 起 [`StreamTask`] 写线程 → 等起播闸门 →
//!   核验真实容器在白名单里 → 交**还在长的** [`StreamBuffer`]。
//!   字节由写线程继续推进，解码在 [`crate::audio::stream_pump`] 的独立线程上，
//!   于是「出声」不再等「下完」——90 MB 母带档第一次有了百毫秒级的起播。
//!
//! 选型说明（`docs/NETEASE_PHASE3_PLAN.md` §3）：rodio 0.23 的解码器要求
//! `Read + Seek`，不支持纯网络流，因此所有形态都**先落盘**再解码，
//! **音频引擎只见本地文件/本地水位**——从结构上排除「音频回调做网络」。
//!
//! 边界约束：
//! - 本模块只依赖 [`MusicProvider`] / [`AudioFetcher`] 两个接口，
//!   不依赖任何第三方 SDK/HTTP 实现（实现分别在 `providers/`）。
//! - 播放 URL 不写盘、不持久缓存（规范 9.1）；临时文件只存音频字节。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::music::lyrics::Lyrics;
use crate::music::models::AudioSpec;
use crate::music::provider::{
    BoxFuture, DynMusicProvider, LoginState, MusicProvider, PlaybackSource, Playlist,
    ProviderError, ProviderId, QrLoginSession, QualityCeiling, Track, TrackAvailability,
};
use crate::music::retry::RetryPolicy;
use crate::music::stream_buffer::{start_gate, BufferPhase, StreamBuffer};
use crate::music::stream_transport::{StreamTask, UrlRefresher};

/// 距过期不足该秒数即视为「将过期」，取址后主动重新取一次。
pub const EXPIRY_MARGIN_SECS: i64 = 30;

/// 音频文件体积上限（字节）。流式写盘方应在超限时中止，
/// 本模块下载完成后同样做一次兜底核查。
pub const MAX_AUDIO_BYTES: u64 = 120 * 1024 * 1024;

/// 会话缓存的条目上限。
///
/// 12 首在 320k 下约 120 MB；无损时由下面的字节上限先触发。设上限不只是省内存：
/// `NETEASE_INTEGRATION_SPEC.md` §9.2 第 1 条要求"所有缓存必须设置上限"。
const CACHE_MAX_ENTRIES: usize = 12;

/// 会话缓存的字节上限。
const CACHE_MAX_BYTES: u64 = 300 * 1024 * 1024;

/// 条目存活时间（§9.2 第 2 条「缓存失效时间必须可以调整」）。
const CACHE_TTL: Duration = Duration::from_secs(30 * 60);

/// 边下边播的**容器白名单**（v1 只有 flac）。
///
/// 为什么窄：白名单挡的是「解码泵在还在长的文件上能不能正常工作」这件事，
/// 而每一条都得有真实素材背书。
/// - **flac**：正是痛点本身（网易云最高无损单文件可达 90 MB），且容器头
///   （`fLaC` + STREAMINFO）在最前面，采样率/位深/总时长在第一个字节批次里就齐了；
///   symphonia 0.5.5 的 flac 读取器要求流标记严格在偏移 0——所以白名单核验用的是
///   **文件头魔数**而不是只看接口 `type`，带 ID3 前缀的 flac 一律走整首路径（不误开流）。
/// - **mp3/ogg/wav**：本来就只有几 MB，整首下载已经够快，开流只是多两条线程；
///   m4a 现在根本解不了（rodio 未启用 `mp4` 特性）。
///
/// 名单外的一律整首，且**根本不起流**（判据见 [`OnlineTrackLoader::stream_plan`]）：
/// 后缀要在写线程创建文件之前就定死（引擎 `reload()` 按路径重开，起播后不能改名），
/// 所以「先开着流再看是什么容器」这条路走不通。接口声明与文件头对不上时
/// （要 flac 回了 mp3）同样回退整首——此时丢弃的只是一个闸门大小的前缀。
const STREAM_CONTAINERS: &[&str] = &["flac"];

/// 装载侧问一次水位的间隔。
///
/// 20 ms 既能让取消（`tokio::select!` 那条分支）按得下去，也不会把装载线程
/// 变成空转轮询器——闸门通常在几百毫秒内到达，也就是几十次轮询。
const STREAM_POLL: Duration = Duration::from_millis(20);

/// 等闸门/等收工时允许的「水位静止」上限。
///
/// 与读侧同一个判据（[`crate::music::stream_buffer::READ_PATIENCE`]）：传输层
/// 自己的空闲超时是 15 s + 有限次重连，静止到 45 s 说明写手已经不会再来字节了，
/// 装载侧再等下去只是让用户多看一会儿转圈。
const STREAM_STALL_PATIENCE: Duration = crate::music::stream_buffer::READ_PATIENCE;

/// 音频字节获取接口。
///
/// HTTP 细节（客户端、超时、流式写盘）由 `providers::http_fetch` 实现；
/// 领域层只见该接口，便于离线确定性测试。
///
/// `Sync` 是超约束而非装饰：装载器把同一个 fetcher 同时交给装载线程与流式写
/// 线程，[`AudioFetcher::stream_to_buffer`] 的默认实现也据此才能证明自己的
/// future 是 `Send` 的。
pub trait AudioFetcher: Sync {
    /// 把 `url` 的内容下载到 `dest`，返回写入字节数。
    ///
    /// 失败时应删除半成品文件（调用方也会兜底清理）。
    /// 非 2xx 响应应映射为分类错误（URL 失效类 → [`ProviderError::PlaybackUrlUnavailable`]）。
    fn fetch_to_file(
        &self,
        url: &str,
        dest: &Path,
    ) -> impl Future<Output = Result<u64, ProviderError>> + Send;

    /// 边下边播：把 `url` 的字节写进 [`StreamBuffer`] 对应的文件，
    /// 并随落盘推进水位；返回最终字节数。
    ///
    /// 与 [`Self::fetch_to_file`] 的区别只在**记账方式**：本方法必须在每批字节
    /// 落盘后立即 [`StreamBuffer::commit`]，让解码器能在整首下完之前起播。
    /// 起始偏移取自缓冲当前水位——签名 URL 失效后换新地址重进本方法，
    /// 就是原地续传而不是从头重下。
    ///
    /// 失败/取消时**不删文件**（文件正被读方打开，删除属于装载器的事），
    /// 也**不下终局判断**——要不要换新地址再续一段，只有调用方知道。
    /// 成功返回 `Ok` 时必须已把缓冲标记为 `Done`：读方越线即真 EOF。
    ///
    /// 默认实现等价于「整首下完再一次性公布」：没有流式收益，但任何实现
    /// （测试桩、不支持流式的后端）都能填缓冲。真正逐批推进水位的是
    /// [`crate::providers::http_fetch::HttpAudioFetcher`]。
    fn stream_to_buffer(
        &self,
        url: &str,
        buf: &std::sync::Arc<crate::music::stream_buffer::StreamBuffer>,
    ) -> impl Future<Output = Result<u64, ProviderError>> + Send {
        let dest = buf.path().to_path_buf();
        let url = url.to_string();
        let buf = std::sync::Arc::clone(buf);
        async move {
            let bytes = self.fetch_to_file(&url, &dest).await?;
            // 一次性公布：此刻字节确实全在文件里，水位与总长都是事实
            buf.declare_len(bytes);
            buf.commit(bytes);
            buf.mark_done();
            Ok(bytes)
        }
    }
}

/// [`AudioFetcher`] 的对象安全视图（与 [`MusicProvider`] → `DynMusicProvider` 同构）。
///
/// UI/桥层持有 `Arc<dyn DynAudioFetcher>` 即可**运行时替换**下载实现
/// （真实 HTTP ↔ 测试桩），装载器无需泛型参数具体化。
pub trait DynAudioFetcher: Send + Sync {
    /// 语义同 [`AudioFetcher::fetch_to_file`]。
    fn fetch_to_file_boxed<'a>(
        &'a self,
        url: &'a str,
        dest: &'a Path,
    ) -> BoxFuture<'a, Result<u64, ProviderError>>;

    /// 语义同 [`AudioFetcher::stream_to_buffer`]。
    fn stream_to_buffer_boxed<'a>(
        &'a self,
        url: &'a str,
        buf: &'a std::sync::Arc<crate::music::stream_buffer::StreamBuffer>,
    ) -> BoxFuture<'a, Result<u64, ProviderError>>;
}

/// 全称 blanket impl：任何同步可发送的 [`AudioFetcher`] 自动可作
/// [`DynAudioFetcher`] 使用。
impl<F> DynAudioFetcher for F
where
    F: AudioFetcher + Send + Sync,
{
    fn fetch_to_file_boxed<'a>(
        &'a self,
        url: &'a str,
        dest: &'a Path,
    ) -> BoxFuture<'a, Result<u64, ProviderError>> {
        Box::pin(F::fetch_to_file(self, url, dest))
    }

    fn stream_to_buffer_boxed<'a>(
        &'a self,
        url: &'a str,
        buf: &'a std::sync::Arc<crate::music::stream_buffer::StreamBuffer>,
    ) -> BoxFuture<'a, Result<u64, ProviderError>> {
        Box::pin(F::stream_to_buffer(self, url, buf))
    }
}

/// 反向适配：`Arc<dyn DynAudioFetcher>` 可直接作为 [`AudioFetcher`] 泛型实参。
impl AudioFetcher for std::sync::Arc<dyn DynAudioFetcher> {
    fn fetch_to_file(
        &self,
        url: &str,
        dest: &Path,
    ) -> impl Future<Output = Result<u64, ProviderError>> + Send {
        // 先克隆出自有数据再进 async 块：返回的 future 不捕获入参生命周期
        let this = std::sync::Arc::clone(self);
        let url = url.to_string();
        let dest = dest.to_path_buf();
        async move { DynAudioFetcher::fetch_to_file_boxed(this.as_ref(), &url, &dest).await }
    }

    fn stream_to_buffer(
        &self,
        url: &str,
        buf: &std::sync::Arc<crate::music::stream_buffer::StreamBuffer>,
    ) -> impl Future<Output = Result<u64, ProviderError>> + Send {
        let this = std::sync::Arc::clone(self);
        let url = url.to_string();
        let buf = std::sync::Arc::clone(buf);
        async move { DynAudioFetcher::stream_to_buffer_boxed(this.as_ref(), &url, &buf).await }
    }
}

/// [`MusicProvider`] 适配器：把 UI 层的 `Arc<dyn DynMusicProvider>` 重新投影为
/// 泛型绑定 [`MusicProvider`] 的组件（如 [`OnlineTrackLoader`]）可用的形态。
///
/// 用 newtype 而非直接为 `Arc<dyn DynMusicProvider>` 实现 [`MusicProvider`]：
/// 后者会让同一类型同时实现 [`MusicProvider`] 与 `DynMusicProvider`，
/// 两者同名方法在调用点产生 E0034 歧义。newtype 把双投影隔离在本类型上，
/// 其它代码不受影响。行为与被包装的动态分发完全一致。
#[derive(Clone)]
pub struct DynProviderShim(pub std::sync::Arc<dyn DynMusicProvider>);

impl DynProviderShim {
    /// 包装对象安全 Provider。
    pub fn new(provider: std::sync::Arc<dyn DynMusicProvider>) -> Self {
        Self(provider)
    }
}

impl MusicProvider for DynProviderShim {
    fn id(&self) -> ProviderId {
        DynMusicProvider::id(self.0.as_ref())
    }

    /// 必须转发：默认实现返回 `QualityCeiling::default()`，会让装载器误以为
    /// 音质档位永远是"无损"，改档后会话缓存不作废（"设置没生效"）。
    fn quality(&self) -> QualityCeiling {
        DynMusicProvider::quality(self.0.as_ref())
    }

    fn search_songs(
        &self,
        keyword: &str,
        limit: u32,
    ) -> impl Future<Output = Result<Vec<Track>, ProviderError>> + Send {
        let this = std::sync::Arc::clone(&self.0);
        let keyword = keyword.to_string();
        async move { DynMusicProvider::search_songs(this.as_ref(), &keyword, limit).await }
    }

    fn song_details(
        &self,
        ids: &[String],
    ) -> impl Future<Output = Result<Vec<Track>, ProviderError>> + Send {
        let this = std::sync::Arc::clone(&self.0);
        let ids = ids.to_vec();
        async move { DynMusicProvider::song_details(this.as_ref(), &ids).await }
    }

    fn playlist(
        &self,
        playlist_id: &str,
        offset: u32,
        limit: u32,
    ) -> impl Future<Output = Result<Playlist, ProviderError>> + Send {
        let this = std::sync::Arc::clone(&self.0);
        let playlist_id = playlist_id.to_string();
        async move { DynMusicProvider::playlist(this.as_ref(), &playlist_id, offset, limit).await }
    }
    fn daily_songs(&self) -> impl Future<Output = Result<Vec<Track>, ProviderError>> + Send {
        let this = std::sync::Arc::clone(&self.0);
        async move { DynMusicProvider::daily_songs(this.as_ref()).await }
    }

    fn intelligence_list(
        &self,
        song_id: &str,
        playlist_id: &str,
        start_id: &str,
        count: u32,
    ) -> impl Future<Output = Result<Vec<Track>, ProviderError>> + Send {
        let this = std::sync::Arc::clone(&self.0);
        let song_id = song_id.to_string();
        let playlist_id = playlist_id.to_string();
        let start_id = start_id.to_string();
        async move {
            DynMusicProvider::intelligence_list(
                this.as_ref(),
                &song_id,
                &playlist_id,
                &start_id,
                count,
            )
            .await
        }
    }

    fn search_match(
        &self,
        title: &str,
        album: &str,
        artist: &str,
        duration_secs: u64,
        md5: &str,
    ) -> impl Future<Output = Result<Vec<Track>, ProviderError>> + Send {
        let this = std::sync::Arc::clone(&self.0);
        let title = title.to_string();
        let album = album.to_string();
        let artist = artist.to_string();
        let md5 = md5.to_string();
        async move {
            DynMusicProvider::search_match(
                this.as_ref(),
                &title,
                &album,
                &artist,
                duration_secs,
                &md5,
            )
            .await
        }
    }

    fn featured_playlists(
        &self,
        kind: crate::music::provider::FeaturedKind,
        limit: u32,
    ) -> impl Future<Output = Result<Vec<crate::music::provider::PlaylistSummary>, ProviderError>> + Send
    {
        let this = std::sync::Arc::clone(&self.0);
        async move { DynMusicProvider::featured_playlists(this.as_ref(), kind, limit).await }
    }

    fn check_availability(
        &self,
        song_id: &str,
    ) -> impl Future<Output = Result<TrackAvailability, ProviderError>> + Send {
        let this = std::sync::Arc::clone(&self.0);
        let song_id = song_id.to_string();
        async move { DynMusicProvider::check_availability(this.as_ref(), &song_id).await }
    }

    fn playback_source(
        &self,
        song_id: &str,
    ) -> impl Future<Output = Result<PlaybackSource, ProviderError>> + Send {
        let this = std::sync::Arc::clone(&self.0);
        let song_id = song_id.to_string();
        async move { DynMusicProvider::playback_source(this.as_ref(), &song_id).await }
    }

    fn lyrics(&self, song_id: &str) -> impl Future<Output = Result<Lyrics, ProviderError>> + Send {
        let this = std::sync::Arc::clone(&self.0);
        let song_id = song_id.to_string();
        async move { DynMusicProvider::lyrics(this.as_ref(), &song_id).await }
    }

    fn login_status(&self) -> impl Future<Output = Result<LoginState, ProviderError>> + Send {
        let this = std::sync::Arc::clone(&self.0);
        async move { DynMusicProvider::login_status(this.as_ref()).await }
    }

    fn qr_login_start(&self) -> impl Future<Output = Result<QrLoginSession, ProviderError>> + Send {
        let this = std::sync::Arc::clone(&self.0);
        async move { DynMusicProvider::qr_login_start(this.as_ref()).await }
    }

    fn qr_login_poll(
        &self,
        key: &str,
    ) -> impl Future<Output = Result<LoginState, ProviderError>> + Send {
        let this = std::sync::Arc::clone(&self.0);
        let key = key.to_string();
        async move { DynMusicProvider::qr_login_poll(this.as_ref(), &key).await }
    }

    fn login_cellphone(
        &self,
        phone: &str,
        password: &str,
    ) -> impl Future<Output = Result<LoginState, ProviderError>> + Send {
        let this = std::sync::Arc::clone(&self.0);
        let phone = phone.to_string();
        let password = password.to_string();
        async move { DynMusicProvider::login_cellphone(this.as_ref(), &phone, &password).await }
    }

    fn login_cookie(
        &self,
        cookie: &str,
    ) -> impl Future<Output = Result<LoginState, ProviderError>> + Send {
        let this = std::sync::Arc::clone(&self.0);
        let cookie = cookie.to_string();
        async move { DynMusicProvider::login_cookie(this.as_ref(), &cookie).await }
    }

    fn logout(&self) -> impl Future<Output = Result<(), ProviderError>> + Send {
        let this = std::sync::Arc::clone(&self.0);
        async move { DynMusicProvider::logout(this.as_ref()).await }
    }

    fn session_cookie(&self) -> Option<String> {
        DynMusicProvider::session_cookie(self.0.as_ref())
    }
}

/// 已准备好的可播放媒体：本地路径 + 取得时的播放地址 +（可选）还在长的字节缓冲。
///
/// `path` 可直接交给 `PlaybackController::load` / `switch_to`。
/// 播放期间临时文件必须保留；用完统一 [`OnlineTrackLoader::cleanup`]。
#[derive(Debug, Clone)]
pub struct PreparedTrack {
    pub path: PathBuf,
    pub source: PlaybackSource,
    /// 临时文件的真实音频流参数（采样率/位深/码率）。
    ///
    /// 在**下载线程**里探测，不占 UI 线程；探测失败留空，不伪造。
    pub spec: AudioSpec,
    /// 边下边播的共享字节缓冲。
    ///
    /// `Some` = **`path` 这个文件还没下完**，调用方必须把它交给
    /// [`crate::audio::AudioEngine::load_stream`]（解码泵线程），
    /// **不能**用整首 `load` 打开——那会读到尾部空洞，或者干脆解码失败。
    /// `None` = 整首已在盘上，走原来的 `load`。
    pub stream: Option<Arc<StreamBuffer>>,
}

/// 装载请求的性质。
///
/// 预取与点播走**同一条**装载 lane、**同一个**会话缓存：预取只是提前往缓存里
/// 放一条，用户随后点播同一首时直接命中。这是"预取"与"会话缓存"必须一起做的
/// 原因——分开实现等于把同一份字节下两遍。
///
/// **只有点播会边下边播**：预取的收益是「下完」而不是「早交」，把一条没人听的
/// 曲子中途中转成交付形态，等于让它在后台占着整首的带宽却可能永远不放。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioKind {
    /// 用户点了这首歌，结果要播放。
    Play,
    /// 预取下一首，结果只进缓存。
    Prefetch,
}

/// 会话缓存的一条。
#[derive(Debug, Clone)]
struct CachedEntry {
    path: PathBuf,
    source: PlaybackSource,
    spec: AudioSpec,
    bytes: u64,
    /// 单调递增的使用序号（不用 `SystemTime`：可测、同帧稳定）。
    last_used: u64,
    inserted_at: std::time::Instant,
}

/// 一条**已经交给调用方、但还没下完**的流式装载。
///
/// 装载器是它的所有者：`task` 活着 = 字节还在推进，`task` 一 drop 就请求停手。
/// 「什么时候把它转正进 [`CachedEntry`]」只能由装载器决定（见
/// [`OnlineTrackLoader::promote_finished_stream`]）——半截文件进缓存，
/// 下次点播同一首就会把一个残缺容器交给解码器。
#[derive(Debug)]
struct PendingStream {
    song_id: String,
    path: PathBuf,
    source: PlaybackSource,
    spec: AudioSpec,
    buffer: Arc<StreamBuffer>,
    /// 写线程句柄。字段本身不被读，它的存在就是「这条下载还在跑」这件事。
    _task: StreamTask,
}

/// 一条边下边播装载的中间结局（只在本模块内部流转）。
enum Streamed {
    /// 交回调用方：可能是**还在长的**缓冲，也可能是「闸门没到就先下完了」的整首。
    Delivered(PreparedTrack),
    /// 这一轮不适合流式（总长未知 / 容器与声明不符 / 写手认输）。
    /// 携带人话原因供日志；临时文件已由 [`StreamGuard`] 收干净，
    /// 交调用方重走整首路径。
    Fallback(String),
}

/// [`OnlineTrackLoader::await_gate`] 的结论。
enum GateWait {
    /// 水位到闸门：可以交解码器了。
    Ready,
    /// 整首先后到了（短文件 / 网速够快）：按整首交付，不进 `pending`。
    Complete,
    /// 写手留下的终局（失败 / 总长未知 / 被取消）。
    Gone(BufferPhase),
    /// 水位静止超过耐心：写手大概已经不会再来字节了。
    Stalled,
}

/// 「闸门之前」这段时间里流的所有权。
///
/// 存在的唯一理由：装载的 future 会被**丢弃**（换台时桥层的 `select!` 走取消分支），
/// 而 `StreamTask` 住在线程上，不会因为谁丢弃了 future 就自己停手。
/// 没有这个守卫，被丢弃的那条下载会一直占着带宽与临时目录，直到整首下完——
/// 正是本模块想消灭的那种浪费。走到「交出去」那一步时 [`Self::release`]
/// 会拆掉引信，此后取消归装载器的 `pending` 管。
struct StreamGuard {
    task: Option<StreamTask>,
    path: PathBuf,
}

impl StreamGuard {
    fn new(task: StreamTask) -> Self {
        let path = task.buffer().path().to_path_buf();
        Self {
            task: Some(task),
            path,
        }
    }

    /// 交出去：把写线程句柄交给调用方，本守卫从此不再管这条流。
    fn release(&mut self) -> StreamTask {
        // 只 release 一次：调用方在后面还要拿句柄装进 pending
        self.task.take().expect("StreamGuard 只能交出一次")
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        let Some(task) = self.task.take() else {
            return;
        };
        // drop(task) 同步把缓冲标成 Aborted 并请写线程在下一个轮询边界退出；
        // 不 join：取消反应速度是用户能感觉到的东西。
        drop(task);
        // 这份文件从没登记进 `downloaded`、也从没交给引擎，删它是安全的
        // （写线程看到 `is_stopped` 就再不会 `File::create` 把它造回来）。
        if let Err(e) = std::fs::remove_file(&self.path) {
            tracing::debug!("流式半成品清理失败: {} ({})", self.path.display(), e);
        }
    }
}

/// 判断播放地址相对 `now_unix` 是否已过期/将过期。
///
/// `expires_at` 未知（接口未提供 `expi`）视为不过期——**不伪造**。
pub fn is_expired(source: &PlaybackSource, now_unix: i64, margin_secs: i64) -> bool {
    match source.expires_at {
        Some(ts) => now_unix.saturating_add(margin_secs.max(0)) >= ts,
        None => false,
    }
}

/// 音频内容魔数校验：拦截 CDN 以 HTTP 200 返回的 HTML 错误页等非音频内容。
///
/// 支持 mp3（ID3 / 帧同步）、FLAC、WAV、Ogg、MP4/M4A（`ftyp`）。
pub fn looks_like_audio(head: &[u8]) -> bool {
    if head.len() < 4 {
        return false;
    }
    // mp3：ID3 标签头，或帧同步字 0xFFEx
    if &head[..3] == b"ID3" {
        return true;
    }
    if head[0] == 0xFF && (head[1] & 0xE0) == 0xE0 {
        return true;
    }
    // FLAC / Ogg
    if &head[..4] == b"fLaC" || &head[..4] == b"OggS" {
        return true;
    }
    // WAV：RIFF....WAVE
    if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WAVE" {
        return true;
    }
    // MP4/M4A：'ftyp' 位于偏移 4
    if head.len() >= 8 && &head[4..8] == b"ftyp" {
        return true;
    }
    false
}

/// 真实容器提示（小写、不带点），用于临时文件后缀与面板 codec。
///
/// 顺序是**文件头魔数优先、接口 `type` 兜底**：
/// - 魔数是刚刚通过 [`looks_like_audio`] 校验过的**事实**；
/// - 接口 `type` 只是请求时的期望，服务端完全可能给了别的档（要 320k 回 FLAC，
///   或反过来），拿它当权威会把"实际在播什么"标错。
///
/// 返回 `None` = 真的认不出来（不猜）。此时文件保留 `.audio` 占位名、codec 留空，
/// 面板显示 `—` 而不是那个毫无意义的 `AUDIO`。
fn container_hint(head: &[u8], content_type: Option<&str>) -> Option<&'static str> {
    if head.len() >= 4 {
        if &head[..3] == b"ID3" || (head[0] == 0xFF && (head[1] & 0xE0) == 0xE0) {
            return Some("mp3");
        }
        match &head[..4] {
            b"fLaC" => return Some("flac"),
            b"OggS" => return Some("ogg"),
            _ => {}
        }
        if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WAVE" {
            return Some("wav");
        }
        if head.len() >= 8 && &head[4..8] == b"ftyp" {
            return Some("m4a");
        }
    }
    // 接口 type 容错：大小写、`audio/` MIME 前缀、多余空白都认
    let raw = content_type?.trim().to_ascii_lowercase();
    let raw = raw.strip_prefix("audio/").unwrap_or(&raw);
    Some(match raw {
        "mpeg" | "mp3" => "mp3",
        "flac" | "x-flac" => "flac",
        "wav" | "wave" | "x-wav" => "wav",
        "ogg" | "vorbis" => "ogg",
        "m4a" | "mp4" | "aac" => "m4a",
        _ => return None, // 不认识就承认不认识
    })
}

/// 在装载线程里探测真实流参数，并给面板一个可信的 codec。
///
/// 探测失败留空，**不伪造**（`load_media` 自己会降级并记一条 warn）。
///
/// codec 以**容器事实**为准，不信文件名：原来靠 `load_media` 从扩展名推，
/// 而在线临时文件曾经永远叫 `.audio` → codec 恒为 `Some("AUDIO")`；
/// `PlayerCore` 那边"codec 为空才用接口 type 兜底"的分支因此永不成立，
/// 面板上永远看不到 FLAC/MP3。认不出容器时显式置空（面板显示 `—`），
/// 也不留那个假的 AUDIO。
///
/// `path` 可以是**还没下完**的流式文件：白名单里的容器把元数据块放在最前面
/// （FLAC 的 STREAMINFO 紧跟魔数），读得到就是事实，读不到就是空。
fn probe_spec(path: &Path, hint: Option<&'static str>) -> AudioSpec {
    let mut spec = crate::music::load_media(path)
        .map(|m| m.audio)
        .unwrap_or_default();
    spec.codec = hint.map(|h| h.to_ascii_uppercase());
    spec
}

/// 当前 Unix 秒。
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 读取文件头部 `n` 字节（用于魔数校验）。
fn read_head(path: &Path, n: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; n];
    let mut filled = 0;
    while filled < n {
        match f.read(&mut buf[filled..])? {
            0 => break,
            k => filled += k,
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

/// 缓冲终局 → 「为什么这一首改走整首」的人话原因。
///
/// 只用于日志：不外泄 URL、原始响应或堆栈（规范 3.2），也不参与错误分类。
fn fallback_reason(phase: &BufferPhase) -> String {
    match phase {
        BufferPhase::LengthUnknown => "响应没声明 Content-Length（总长未知）".to_string(),
        BufferPhase::Failed(why) => format!("流式下载认输：{why}"),
        BufferPhase::Aborted => "装载已取消".to_string(),
        BufferPhase::Done => "整首已经下完".to_string(),
        BufferPhase::Growing => "仍在下载".to_string(),
    }
}

/// 该失败是否值得「冷恢复」一次（全新取址 + 重新下载）。
///
/// 只针对**该播放地址无效/内容不对**的一类问题；
/// 瞬时网络故障已由 [`RetryPolicy`] 消化，语义类错误（不可播、需要登录、
/// 参数非法）恢复也无意义——一律直接清晰报错。
fn worth_cold_recovery(e: &ProviderError) -> bool {
    matches!(
        e,
        ProviderError::PlaybackUrlUnavailable
            | ProviderError::UnsupportedResponse
            | ProviderError::InvalidData
    )
}

/// 主暂存目录建不起来时，依次尝试 `<主名>_1` … `<主名>_N`。
const SCRATCH_BRANCH_TRIES: u32 = 8;

/// 创建暂存目录；主名被占死时退到同辈分叉目录，返回真正可用的路径。
///
/// 为什么要分叉：目录名按 PID 生成，而 PID 会被系统回收。Windows 上一个仍有
/// 句柄引用（杀软、资源管理器、存储感知）的目录可以处于「删除挂起」态——它已经
/// 不算存在，却又拒绝同名重建，`CreateDirectory` 直接报 ERROR_ACCESS_DENIED。
/// 新一代进程正好接上那个 PID 时，不处理就会让此后每一次播放都失败。
/// 撞了就换隔壁，别把整条播放链路卡死在一颗钉子上。
fn create_scratch(dir: &Path) -> std::io::Result<PathBuf> {
    match std::fs::create_dir_all(dir) {
        Ok(()) => return Ok(dir.to_path_buf()),
        Err(first) => {
            let (Some(parent), Some(stem)) =
                (dir.parent(), dir.file_name().and_then(|n| n.to_str()))
            else {
                return Err(first);
            };
            for n in 1..=SCRATCH_BRANCH_TRIES {
                let branch = parent.join(format!("{stem}_{n}"));
                if std::fs::create_dir_all(&branch).is_ok() {
                    return Ok(branch);
                }
            }
            Err(first)
        }
    }
}

/// 在线曲目装载器：取址 → 过期判断 → 下载（整首或边下边播）→ 校验 →（必要时）冷恢复。
pub struct OnlineTrackLoader<P, F> {
    provider: Arc<P>,
    fetcher: Arc<F>,
    retry: RetryPolicy,
    temp_dir: PathBuf,
    max_bytes: u64,
    /// 本会话已下载的临时文件，供 [`OnlineTrackLoader::cleanup`] 统一清理。
    downloaded: Vec<PathBuf>,
    next_seq: u32,
    /// 会话缓存：`song_id → 条目`。条目数很少（≤12），线性扫描比手写 LRU 更直白。
    ///
    /// 这是**本模块是临时文件唯一所有者**的落脚点：文件在缓存里就还能用，
    /// 出了缓存就该删。会话层不再参与文件处置（见 `PlaySession` 模块文档）。
    cache: Vec<(String, CachedEntry)>,
    cached_bytes: u64,
    /// 单调使用序号，供 LRU 排序。
    use_tick: u64,
    cache_max_entries: usize,
    cache_max_bytes: u64,
    cache_ttl: Duration,
    /// 引擎当前装载的文件（由桥注入）。淘汰与清扫**永不**碰它。
    ///
    /// 不能指望"Windows 会拒绝删除正在播放的文件"：现代 Rust std 在 Windows
    /// 上以允许删除的共享模式打开文件，`remove_file` 很可能成功——目录项消失，
    /// 解码器手里句柄继续读，但 `RodioEngine::reload()` 是按**路径**重开的，
    /// 下一次 play/seek 就 `MediaRead` 失败。
    active: Option<PathBuf>,
    /// 缓存建立时的音质档位。档位一变整批作废，否则用户改完设置听到的还是旧档。
    cache_quality: Option<QualityCeiling>,
    /// 边下边播总开关（默认开）。关掉后一律整首下载，用于 A/B 与排障。
    streaming: bool,
    /// 允许流式播放的容器（默认 [`STREAM_CONTAINERS`]）。
    stream_containers: &'static [&'static str],
    /// 已交给调用方、但还没下完的那条流（一次只留一条，见
    /// [`PendingStream`]）。
    pending: Option<PendingStream>,
}

/// `Send + 'static` 是边下边播带出来的：装载器要把 Provider/Fetcher 交给
/// **另一条线程**上的 [`StreamTask`]（自带运行器的写线程），还要在那里回调
/// Provider 重新取址——对象跨线程流动且没有借用生命周期可言。
impl<P, F> OnlineTrackLoader<P, F>
where
    P: MusicProvider + Send + Sync + 'static,
    F: AudioFetcher + Send + Sync + 'static,
{
    /// 创建装载器。临时文件写入进程独立目录（`reimuplayer_online_<pid>`）。
    ///
    /// 同进程里再来一个实例就往后备一个编号：两个装载器共用同一个目录时，
    /// 先退出的那个会在 `Drop` 里把目录连根删掉，后用的那个下载时就只剩
    /// 「系统找不到指定的路径」（os error 3）。生产环境一个进程只建一个装载器，
    /// 所以常规路径仍是那个不带后缀的规范名。
    pub fn new(provider: P, fetcher: F) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static INSTANCES: AtomicU32 = AtomicU32::new(0);
        let base = format!("reimuplayer_online_{}", std::process::id());
        let nth = INSTANCES.fetch_add(1, Ordering::Relaxed);
        let temp_dir = std::env::temp_dir().join(if nth == 0 {
            base
        } else {
            format!("{base}_{nth}")
        });
        Self {
            provider: Arc::new(provider),
            fetcher: Arc::new(fetcher),
            retry: RetryPolicy::default(),
            temp_dir,
            max_bytes: MAX_AUDIO_BYTES,
            downloaded: Vec::new(),
            next_seq: 0,
            cache: Vec::new(),
            cached_bytes: 0,
            use_tick: 0,
            cache_max_entries: CACHE_MAX_ENTRIES,
            cache_max_bytes: CACHE_MAX_BYTES,
            cache_ttl: CACHE_TTL,
            active: None,
            cache_quality: None,
            streaming: true,
            stream_containers: STREAM_CONTAINERS,
            pending: None,
        }
    }

    /// 覆盖重试策略（测试或调优用）。
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// 覆盖临时目录（测试用）。
    pub fn with_temp_dir(mut self, dir: PathBuf) -> Self {
        self.temp_dir = dir;
        self
    }

    /// 覆盖体积上限（测试或配置调优用）。
    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// 覆盖会话缓存上限（测试或调优用）。
    pub fn with_cache_limits(mut self, entries: usize, bytes: u64) -> Self {
        self.cache_max_entries = entries;
        self.cache_max_bytes = bytes;
        self
    }

    /// 覆盖缓存 TTL（测试用）。
    pub fn with_cache_ttl(mut self, ttl: Duration) -> Self {
        self.cache_ttl = ttl;
        self
    }

    /// 开/关边下边播（默认开）。关掉后一律整首下载——A/B 对比与排障都用它，
    /// 比如「起播快但声音不对」时先确认问题出在解码泵还是传输层。
    pub fn with_streaming(mut self, on: bool) -> Self {
        self.streaming = on;
        self
    }

    /// 覆盖容器白名单（默认 [`STREAM_CONTAINERS`]）。
    ///
    /// 加条目要付的代价：那个容器的元数据必须在文件开头，且解码泵在
    /// 「还在长的文件」上真实跑通过；两条都不成立时多开一个容器只是多一种崩法。
    pub fn with_stream_containers(mut self, containers: &'static [&'static str]) -> Self {
        self.stream_containers = containers;
        self
    }

    /// 边下边播总开关（诊断/测试）。
    pub fn is_streaming(&self) -> bool {
        self.streaming
    }

    /// 当前在飞的流式装载是哪首（测试/诊断）。
    pub fn pending_song(&self) -> Option<&str> {
        self.pending.as_ref().map(|p| p.song_id.as_str())
    }

    /// 告知「引擎现在拿着哪个文件」（`None` = 已转去本地播放或已停止）。
    ///
    /// 淘汰与清扫永不碰它——理由见 [`Self::active`] 的字段注释。
    pub fn set_active(&mut self, path: Option<PathBuf>) {
        self.active = path;
    }

    /// 会话缓存条目数（测试/诊断）。
    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    /// 会话缓存占用字节（测试/诊断）。
    pub fn cached_bytes(&self) -> u64 {
        self.cached_bytes
    }

    /// 临时目录。
    pub fn temp_dir(&self) -> &Path {
        &self.temp_dir
    }

    /// 本会话已下载的临时文件。
    pub fn downloaded_paths(&self) -> &[PathBuf] {
        &self.downloaded
    }

    /// 确保暂存目录可用，必要时把 [`Self::temp_dir`] 换到分叉目录上。
    ///
    /// 报错文案带完整路径：这类问题只能靠玩家转述的一条提示定位，
    /// 不写出究竟是哪个目录建不起来，等于让人复述一句没用的话。
    fn ensure_scratch_dir(&mut self) -> Result<(), ProviderError> {
        match create_scratch(&self.temp_dir) {
            Ok(used) => {
                if used != self.temp_dir {
                    tracing::warn!(
                        "暂存目录 {} 不可用，本会话改用 {}",
                        self.temp_dir.display(),
                        used.display()
                    );
                    self.temp_dir = used;
                }
                Ok(())
            }
            Err(e) => {
                tracing::error!("临时目录创建失败: {} ({})", self.temp_dir.display(), e);
                Err(ProviderError::Unknown(format!(
                    "临时目录创建失败: {}（{}）",
                    e,
                    self.temp_dir.display()
                )))
            }
        }
    }

    /// 准备可播放的本地媒体（用户点播的主入口）。
    ///
    /// 流程：会话缓存命中即返回；否则取播放地址（有限重试；过期/将过期主动重取）
    /// → 下载（有限重试）→ 体积核查 + 魔数校验；若失败属于「地址无效/内容不对」，
    /// **冷恢复一次**（全新取址 + 重新下载）；仍失败返回分类清晰的 [`ProviderError`]。
    pub async fn prepare(&mut self, song_id: &str) -> Result<PreparedTrack, ProviderError> {
        self.prepare_kind(song_id, AudioKind::Play).await
    }

    /// 按性质准备：点播（可冷恢复）或预取（不冷恢复）。
    pub async fn prepare_kind(
        &mut self,
        song_id: &str,
        kind: AudioKind,
    ) -> Result<PreparedTrack, ProviderError> {
        self.refresh_quality_epoch();
        // 上一条流可能已经悄悄下完了（没人再按闸门轮询它）：先转正，
        // 才能既进缓存、又让下面的缓存命中直接把它当整首交出去。
        self.promote_finished_stream();
        if let Some(hit) = self.cache_hit(song_id) {
            tracing::debug!("会话缓存命中: {} -> {}", song_id, hit.path.display());
            return Ok(hit);
        }
        // 同一首还在下（上一轮的流没下完又点了它）：接着用那条流，别起第二条
        if matches!(kind, AudioKind::Play) {
            if let Some(hit) = self.pending_hit(song_id) {
                tracing::info!("沿用在飞的流式装载: {song_id}");
                return Ok(hit);
            }
        }
        match kind {
            AudioKind::Play => match self.prepare_once(song_id, kind).await {
                Ok(track) => Ok(track),
                Err(first) if worth_cold_recovery(&first) => {
                    tracing::warn!("在线装载失败（{}），冷恢复一次：全新取址并重新下载", first);
                    self.prepare_once(song_id, kind).await
                }
                other => other,
            },
            // 预取不值得为一次失败押上两倍下载时间：失败就算了，用户真点播时再试
            AudioKind::Prefetch => self.prepare_once(song_id, kind).await,
        }
    }

    // ------------------------------------------------------------------
    // 会话缓存（本模块是临时文件的唯一所有者）
    // ------------------------------------------------------------------

    /// 命中则返回副本并刷新使用序号；文件已不在/已过期则顺手剔除。
    fn cache_hit(&mut self, song_id: &str) -> Option<PreparedTrack> {
        let pos = self.cache.iter().position(|(id, _)| id == song_id)?;
        let fresh = {
            let entry = &self.cache[pos].1;
            entry.path.exists() && entry.inserted_at.elapsed() < self.cache_ttl
        };
        if !fresh {
            let (_, entry) = self.cache.remove(pos);
            self.cached_bytes = self.cached_bytes.saturating_sub(entry.bytes);
            self.downloaded.retain(|p| p != &entry.path);
            if !entry.path.exists() {
                tracing::debug!("缓存条目文件已消失，剔除: {}", entry.path.display());
            }
            return None;
        }
        self.use_tick = self.use_tick.wrapping_add(1);
        let tick = self.use_tick;
        let entry = &mut self.cache[pos].1;
        entry.last_used = tick;
        Some(PreparedTrack {
            path: entry.path.clone(),
            source: entry.source.clone(),
            spec: entry.spec.clone(),
            // 缓存里只放**已下完**的文件（见 `promote_finished_stream`）
            stream: None,
        })
    }

    /// 入缓存并淘汰超限条目。
    fn cache_insert(&mut self, song_id: &str, entry: CachedEntry) {
        self.use_tick = self.use_tick.wrapping_add(1);
        let mut entry = entry;
        entry.last_used = self.use_tick;
        self.cached_bytes = self.cached_bytes.saturating_add(entry.bytes);
        // 同一首重复下载（上一次失败后重来）：先挤掉旧条目，别让上限虚高
        if let Some(pos) = self.cache.iter().position(|(id, _)| id == song_id) {
            let (_, old) = self.cache.remove(pos);
            self.cached_bytes = self.cached_bytes.saturating_sub(old.bytes);
            self.downloaded.retain(|p| p != &old.path);
            let _ = std::fs::remove_file(&old.path);
        }
        let inserted = entry.path.clone();
        self.cache.push((song_id.to_string(), entry));
        self.evict(&inserted);
    }

    /// 淘汰到上限以内：按 `last_used` 从小到大，**跳过 `pinned`（刚下好的那条）
    /// 与 `active`（引擎正按路径打开的那条）**。
    ///
    /// 两条都得钉住：`prepare(b)` 的瞬间 `active` 还是 a，而 b 是用户马上要播的
    /// —— 淘汰谁都会立刻炸。两者都不可淘汰时允许临时超限，等下一次插入再收。
    fn evict(&mut self, pinned: &std::path::Path) {
        let active = self.active.clone();
        while self.cache.len() > self.cache_max_entries || self.cached_bytes > self.cache_max_bytes
        {
            let victim = self
                .cache
                .iter()
                .enumerate()
                .filter(|(_, (_, e))| e.path != pinned && Some(&e.path) != active.as_ref())
                .min_by_key(|(_, (_, e))| e.last_used)
                .map(|(i, _)| i);
            let Some(victim) = victim else {
                // 只剩 pinned 与 active：允许临时超限，绝不动它们
                break;
            };
            let (_, entry) = self.cache.remove(victim);
            self.cached_bytes = self.cached_bytes.saturating_sub(entry.bytes);
            self.downloaded.retain(|p| p != &entry.path);
            if let Err(e) = std::fs::remove_file(&entry.path) {
                // 删不掉不影响正确性：`cleanup()` 退出时按目录整体清掉
                tracing::debug!("缓存淘汰删除失败: {} ({e})", entry.path.display());
            }
        }
    }

    /// 音质档位变了就整批作废（保留 active 指向的那份）。
    ///
    /// 不作废的后果：用户把档位从 LOSSLESS 调到 EX-HIGH，重播同一首听到的仍是
    /// 缓存里的 FLAC —— 表现为"设置没生效"。
    fn refresh_quality_epoch(&mut self) {
        let current = self.provider.quality();
        match self.cache_quality {
            None => self.cache_quality = Some(current),
            Some(prev) if prev == current => {}
            Some(prev) => {
                tracing::info!("音质档位 {prev:?} → {current:?}，会话缓存整批作废");
                self.cache_quality = Some(current);
                // 在飞的那条流是旧档位下的字节，同样不能留（更不能转正进新缓存）
                self.discard_pending();
                let active = self.active.clone();
                let mut kept = Vec::new();
                let mut dropped = Vec::new();
                for (id, entry) in std::mem::take(&mut self.cache) {
                    if Some(&entry.path) == active.as_ref() {
                        kept.push((id, entry));
                    } else {
                        dropped.push(entry);
                    }
                }
                self.cache = kept;
                for entry in dropped {
                    self.cached_bytes = self.cached_bytes.saturating_sub(entry.bytes);
                    self.downloaded.retain(|p| p != &entry.path);
                    if let Err(e) = std::fs::remove_file(&entry.path) {
                        tracing::debug!("作废缓存删除失败: {} ({e})", entry.path.display());
                    }
                }
            }
        }
    }

    /// 单轮装载：取址 → （白名单容器）边下边播 → 否则整首下载 → 校验。
    ///
    /// 两条路径共用一次取址：`source` 已经在手里，为了换个交付形态再取一次址
    /// 只是多花一个 RTT，还可能拿回不同的签名地址（缓存里记的那份就对不上了）。
    async fn prepare_once(
        &mut self,
        song_id: &str,
        kind: AudioKind,
    ) -> Result<PreparedTrack, ProviderError> {
        let source = self.resolve_source(song_id).await?;

        // 暂存目录在算路径之前定稿：分叉改名之后，目标路径必须跟着走
        self.ensure_scratch_dir()?;
        let stem = format!("track_{:04}_{}", self.next_seq, sanitize_id(song_id));
        self.next_seq = self.next_seq.wrapping_add(1);

        if let Some(ext) = self.stream_plan(&source, kind) {
            match self.prepare_stream(song_id, &stem, &source, ext).await {
                Ok(Streamed::Delivered(prepared)) => return Ok(prepared),
                Ok(Streamed::Fallback(reason)) => {
                    tracing::warn!("这一首改走整首下载：{reason}（{stem}）")
                }
                Err(e) => return Err(e),
            }
        }
        self.prepare_whole_file(song_id, &stem, source).await
    }

    /// 这一首要不要**起**一条流；返回可以流式的容器后缀，`None` = 直接整首。
    ///
    /// 判据用**接口声明的容器**而不是文件头：头要等第一批字节到手才有，而临时
    /// 文件的后缀必须在写线程创建它之前定死——引擎 `reload()` 是按路径重开的，
    /// 起播以后再改名就会把下一次的 play/seek 打成 `MediaRead` 失败。
    /// 声明与实际是否相符，由 [`Self::prepare_stream`] 在闸门处再核一次。
    ///
    /// 预取不开流（收益是「下完」不是「早交」），总开关关掉也一律整首。
    fn stream_plan(&self, source: &PlaybackSource, kind: AudioKind) -> Option<&'static str> {
        if !self.streaming || !matches!(kind, AudioKind::Play) {
            return None;
        }
        // 空文件头 = 只按接口 `type` 推，认不出来（`None`）就不开流
        let declared = container_hint(b"", source.content_type.as_deref())?;
        self.stream_containers
            .contains(&declared)
            .then_some(declared)
    }

    /// 边下边播的单轮装载：起写线程 → 等起播闸门 → 核对真实容器 →
    /// 交**还没下完**的缓冲。
    ///
    /// 失败分两类：**不适合流式**（回退整首，`Streamed::Fallback`）和
    /// **真的播不了**（直接 `Err`，交给上层分类）。区分它们的意义是别把
    /// 一次 chunked 响应误报成播放失败。
    async fn prepare_stream(
        &mut self,
        song_id: &str,
        stem: &str,
        source: &PlaybackSource,
        ext: &'static str,
    ) -> Result<Streamed, ProviderError> {
        // 一次只留一条在飞的流：走到这里说明用户已经不看上一条了（换台第一件事
        // 是 `stop_media()`，旧解码泵已经拆掉），继续下它只是浪费带宽
        self.discard_pending();

        let gate = start_gate(source.bitrate_kbps);
        let dest = self.temp_dir.join(format!("{stem}.{ext}"));
        let buffer = StreamBuffer::new(dest.clone());
        let task = StreamTask::spawn(
            Arc::clone(&self.fetcher) as Arc<dyn DynAudioFetcher>,
            source.url.clone(),
            Arc::clone(&buffer),
            Some(self.url_refresher(song_id)),
        );
        // 从现在起到「交出去」为止，这条流归守卫管：装载 future 被丢弃也必须停手
        let mut guard = StreamGuard::new(task);

        match Self::await_gate(&buffer, gate).await {
            GateWait::Ready | GateWait::Complete => {}
            // 缓冲被外部取消（同一首轮里不会有别人抬它）＝这轮装载作废
            GateWait::Gone(BufferPhase::Aborted) => return Err(ProviderError::Cancelled),
            GateWait::Gone(phase) => return Ok(Streamed::Fallback(fallback_reason(&phase))),
            GateWait::Stalled => {
                tracing::warn!(
                    "边下边播水位静止超过 {:?}（已落盘 {} B），收场",
                    STREAM_STALL_PATIENCE,
                    buffer.written()
                );
                return Err(ProviderError::RequestTimeout);
            }
        }

        // 体积闸门提前到**响应头到手时**：整首路径是下完才核查，流式路径拿到
        // 声明总长就知道这首播不起，不该让用户等满 90 MB 才发现。
        if let Some(total) = buffer.declared().filter(|t| *t > self.max_bytes) {
            return Err(ProviderError::Unknown(format!(
                "音频内容超过大小上限（{} MB，实际 {total} B）",
                self.max_bytes / (1024 * 1024)
            )));
        }

        // 核对真实容器：接口声明与实际不符（要 flac 回了 mp3）不能继续流式——
        // 文件名已经按声明定死了，硬开流会把面板格式标错、让解码器找错门。
        let head = read_head(buffer.path(), 16)
            .map_err(|e| ProviderError::Unknown(format!("音频临时文件读取失败: {}", e)))?;
        if !looks_like_audio(&head) {
            return Err(ProviderError::UnsupportedResponse);
        }
        if container_hint(&head, source.content_type.as_deref()) != Some(ext) {
            return Ok(Streamed::Fallback(format!(
                "接口声明 {ext}、实际容器是 {:?}（已按声明命名，不能开流）",
                container_hint(&head, source.content_type.as_deref())
            )));
        }

        // 闸门没到就先下完了 = 这首本来就不长：按整首交付、进缓存
        if matches!(buffer.phase(), BufferPhase::Done) {
            let bytes = buffer.written();
            let prepared = self.assemble(buffer.path().to_path_buf(), source, Some(ext));
            self.cache_insert(
                song_id,
                CachedEntry {
                    path: prepared.path.clone(),
                    source: prepared.source.clone(),
                    spec: prepared.spec.clone(),
                    bytes,
                    last_used: 0,
                    inserted_at: std::time::Instant::now(),
                },
            );
            let _ = guard.release();
            tracing::info!(
                target: "provider",
                "在线曲目已就绪（流式收满）: {} bytes -> {}",
                bytes,
                prepared.path.display()
            );
            return Ok(Streamed::Delivered(prepared));
        }

        // 交出去：此刻文件还在长，调用方只能走 `load_stream`（见 `PreparedTrack::stream`）
        let spec = probe_spec(buffer.path(), Some(ext));
        self.downloaded.push(dest.clone());
        let task = guard.release();
        let written = buffer.written();
        self.pending = Some(PendingStream {
            song_id: song_id.to_string(),
            path: dest.clone(),
            source: source.clone(),
            spec: spec.clone(),
            buffer: Arc::clone(&buffer),
            _task: task,
        });
        tracing::info!(
            target: "provider",
            "边下边播起播闸门已过: 闸门 {} B、已落盘 {} B -> {} (expires_at={:?})",
            gate,
            written,
            dest.display(),
            source.expires_at
        );
        Ok(Streamed::Delivered(PreparedTrack {
            path: dest,
            source: source.clone(),
            spec,
            stream: Some(buffer),
        }))
    }

    /// 轮询到起播闸门（或终局/停滞）。
    ///
    /// 用轮询而不是条件变量：装载线程跑的是 `current_thread` 运行器，
    /// 阻塞在 `Condvar` 上会把这一层唯一的线程连同**别人的**定时器一起冻住；
    /// 而且取消走的是「丢弃 future」，只有让出执行权才可能被打断。
    async fn await_gate(buffer: &Arc<StreamBuffer>, gate: u64) -> GateWait {
        loop {
            if buffer.can_start_at(gate) {
                return GateWait::Ready;
            }
            let phase = buffer.phase();
            match phase {
                BufferPhase::Growing => {}
                BufferPhase::Done => return GateWait::Complete,
                other => return GateWait::Gone(other),
            }
            // 判据是「最近一次推进」，慢网只要一直在动就不算停滞
            if buffer.is_stalled(STREAM_STALL_PATIENCE) {
                return GateWait::Stalled;
            }
            tokio::time::sleep(STREAM_POLL).await;
        }
    }

    /// 造一条「重新取址」回调交给写线程（签名地址过期时换新地址**原地续传**）。
    ///
    /// URL 只在内存里交给写线程：不落盘、不入日志（规范 9.1/3.2 第 10 条），
    /// 日志里出现的地址片段只有主机名，由 `providers::http_fetch` 负责。
    fn url_refresher(&self, song_id: &str) -> UrlRefresher {
        let provider = Arc::clone(&self.provider);
        let retry = self.retry;
        let id = song_id.to_string();
        Box::new(move || {
            let provider = Arc::clone(&provider);
            let id = id.clone();
            Box::pin(async move {
                retry
                    .retry_limited(|| {
                        let provider = Arc::clone(&provider);
                        let id = id.clone();
                        async move { provider.playback_source(&id).await }
                    })
                    .await
                    .map(|source| source.url)
            }) as BoxFuture<'static, Result<String, ProviderError>>
        })
    }

    /// 整首轮装载：下载到 `.part` → 体积/魔数核查 → 按真实容器改名 → 入缓存。
    ///
    /// 顺序不能反——先按接口 `type` 命名的话，服务端给了别的档就会标错。
    async fn prepare_whole_file(
        &mut self,
        song_id: &str,
        stem: &str,
        source: PlaybackSource,
    ) -> Result<PreparedTrack, ProviderError> {
        let scratch = self.temp_dir.join(format!("{stem}.part"));

        // 下载：可重试类错误做有限次重试
        let fetcher = &self.fetcher;
        let url = source.url.clone();
        let dest_for_fetch = scratch.clone();
        let bytes = self
            .retry
            .retry_limited(|| {
                let url = url.clone();
                let dest = dest_for_fetch.clone();
                async move { fetcher.fetch_to_file(&url, &dest).await }
            })
            .await;

        let bytes = match bytes {
            Ok(b) => b,
            Err(e) => {
                // 半成品清理（下载方失败时也应自清，这里兜底）
                let _ = std::fs::remove_file(&scratch);
                return Err(e);
            }
        };

        // 体积兜底核查（写盘方应在超限时中止，这里是防线）
        if bytes == 0 {
            let _ = std::fs::remove_file(&scratch);
            return Err(ProviderError::InvalidData);
        }
        if bytes > self.max_bytes {
            let _ = std::fs::remove_file(&scratch);
            return Err(ProviderError::Unknown(format!(
                "音频内容超过大小上限（{} MB）",
                self.max_bytes / (1024 * 1024)
            )));
        }

        // 内容校验：拦截 CDN 以 200 返回的非音频内容
        let head = read_head(&scratch, 16).map_err(|e| {
            let _ = std::fs::remove_file(&scratch);
            ProviderError::Unknown(format!("音频临时文件读取失败: {}", e))
        })?;
        if !looks_like_audio(&head) {
            let _ = std::fs::remove_file(&scratch);
            return Err(ProviderError::UnsupportedResponse);
        }

        // 校验通过 → 这时才敢给文件安上真实后缀（见 `container_hint` 的顺序说明）
        let hint = container_hint(&head, source.content_type.as_deref());
        let dest = match hint {
            Some(ext) => {
                let target = self.temp_dir.join(format!("{stem}.{ext}"));
                match std::fs::rename(&scratch, &target) {
                    Ok(()) => target,
                    Err(e) => {
                        // 改名失败**绝不能**让播放失败：只是面板标不出格式而已
                        tracing::debug!(
                            "临时文件改名失败，保留占位名: {} ({e})",
                            scratch.display()
                        );
                        scratch.clone()
                    }
                }
            }
            None => scratch.clone(),
        };

        let prepared = self.assemble(dest, &source, hint);
        tracing::info!(
            target: "provider",
            "在线曲目已就绪: {} bytes -> {} (expires_at={:?})",
            bytes,
            prepared.path.display(),
            source.expires_at
        );
        // 入会话缓存：下次播同一首（重听 / 单曲循环 / 上一首回跳 / 预取命中）
        // 直接拿这份，不再取址、不再下载。
        self.cache_insert(
            song_id,
            CachedEntry {
                path: prepared.path.clone(),
                source: prepared.source.clone(),
                spec: prepared.spec.clone(),
                bytes,
                last_used: 0,
                inserted_at: std::time::Instant::now(),
            },
        );
        Ok(prepared)
    }

    /// 交付形态收尾：登记临时文件 + 在装载线程里探测流参数。
    ///
    /// 真实流参数在后台线程探测（lofty 只读头部，开销可忽略），
    /// UI 线程拿到的就是可直接展示的成品。
    fn assemble(
        &mut self,
        dest: PathBuf,
        source: &PlaybackSource,
        hint: Option<&'static str>,
    ) -> PreparedTrack {
        self.downloaded.push(dest.clone());
        let spec = probe_spec(&dest, hint);
        PreparedTrack {
            path: dest,
            source: source.clone(),
            spec,
            stream: None,
        }
    }

    /// 把已经下完的那条流转正进会话缓存。
    ///
    /// 半截文件绝不能进缓存：下次点播同一首就会把一个残缺容器交给解码器，
    /// 而且缓存的 `bytes` 记账也会按残缺长度算。
    fn promote_finished_stream(&mut self) {
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        if pending.buffer.phase() != BufferPhase::Done {
            return;
        }
        // 记账以声明总长为准：水位可能还差最后一批没刷盘的缓冲
        let bytes = pending
            .buffer
            .declared()
            .unwrap_or_else(|| pending.buffer.written());
        let song_id = pending.song_id.clone();
        let path = pending.path.clone();
        let source = pending.source.clone();
        let spec = pending.spec.clone();
        // 交还句柄：已 Done 的流，写线程本就退出了，drop 只是走个过场
        self.pending = None;
        tracing::debug!(
            "流式装载转正进缓存: {song_id} -> {} ({bytes} B)",
            path.display()
        );
        self.cache_insert(
            &song_id,
            CachedEntry {
                path,
                source,
                spec,
                bytes,
                last_used: 0,
                inserted_at: std::time::Instant::now(),
            },
        );
    }

    /// 同一首还在飞：把**同一条**缓冲再交出去一次，不起第二条下载。
    ///
    /// 触发场景是「点正在缓冲的那首」——缓存里还没有它（半截文件不进缓存），
    /// 不认出来的话就是两份并行下载抢同一条链路，而用户只要一首。
    fn pending_hit(&self, song_id: &str) -> Option<PreparedTrack> {
        let pending = self.pending.as_ref()?;
        if pending.song_id != song_id || pending.buffer.is_aborted() {
            return None;
        }
        Some(PreparedTrack {
            path: pending.path.clone(),
            source: pending.source.clone(),
            spec: pending.spec.clone(),
            stream: Some(Arc::clone(&pending.buffer)),
        })
    }

    /// 丢掉在飞的那条流：只停手，不删文件。
    ///
    /// 不删的理由和 [`Self::active`] 一样——那份文件可能正是引擎按路径拿着的
    /// 当前曲目（`set_active_audio` 要到下一次 Prepared 才换），删除交给
    /// [`Self::cleanup`] 与 `sweep_temp_dir`。
    fn discard_pending(&mut self) {
        if let Some(pending) = self.pending.take() {
            tracing::debug!(
                "放弃未完成的流式装载: {}（已落盘 {} B / 声明 {:?}）",
                pending.song_id,
                pending.buffer.written(),
                pending.buffer.declared()
            );
        }
    }

    /// 取播放地址：有限重试 + 过期主动重取。
    async fn resolve_source(&self, song_id: &str) -> Result<PlaybackSource, ProviderError> {
        let provider = &self.provider;
        let id = song_id.to_string();

        let source = self
            .retry
            .retry_limited(|| {
                let id = id.clone();
                async move { provider.playback_source(&id).await }
            })
            .await?;

        if is_expired(&source, now_unix(), EXPIRY_MARGIN_SECS) {
            tracing::info!(
                "播放地址已过期/将过期，重新取址（expires_at={:?}）",
                source.expires_at
            );
            return self
                .retry
                .retry_limited(|| {
                    let id = id.clone();
                    async move { provider.playback_source(&id).await }
                })
                .await;
        }
        Ok(source)
    }

    /// 清理本会话的临时文件（尽力而为）。
    ///
    /// 这个目录只属于本进程这一次会话（名字带 pid，实例号保证不共享），所以
    /// **按目录整体清**比逐条记帐更不容易漏：被换台丢弃的、取消留下的半成品、
    /// 缓存里还没淘汰的，一次清干净。
    ///
    /// 播放仍在进行时文件可能被引擎占用（Windows 下删除会失败），
    /// 失败只记调试日志，不视为错误。
    pub fn cleanup(&mut self) {
        // 先请写线程停手，再删目录：反过来的话，正在写的进程会在被清空的
        // 目录里凭空造回一个文件（本会话再也没人认识它）
        self.discard_pending();
        if let Ok(entries) = std::fs::read_dir(&self.temp_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Err(e) = std::fs::remove_file(&path) {
                    tracing::debug!(
                        "临时文件删除失败（可能仍被播放占用）: {} ({})",
                        path.display(),
                        e
                    );
                }
            }
        }
        let _ = std::fs::remove_dir(&self.temp_dir);
        self.cache.clear();
        self.cached_bytes = 0;
        self.downloaded.clear();
    }

    /// 清扫临时目录中**未登记**的文件（取消/中断留下的半成品）。
    ///
    /// [`OnlineTrackLoader::prepare`] 的 future 被丢弃（取消）时，正在写入的
    /// 目标文件既不在 [`Self::downloaded`] 里也不会走错误清理路径；调用本方法
    /// 兜底删除。已登记文件不受影响（可能正被播放占用）。
    pub fn sweep_temp_dir(&self) {
        let Ok(entries) = std::fs::read_dir(&self.temp_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if self.downloaded.contains(&path) || Some(&path) == self.active.as_ref() {
                continue;
            }
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::debug!("半成品清扫失败: {} ({})", path.display(), e);
            }
        }
    }
}

/// 设置页「清扫」按钮的结果。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScratchSweep {
    pub dirs_removed: usize,
    pub files_removed: usize,
    pub bytes_freed: u64,
    pub dirs_skipped: usize,
    pub dirs_failed: usize,
}

/// 只清理超过 1 小时的残留：另一个实例可能还在往这些目录里写。
const SCRATCH_STALE_AFTER: Duration = Duration::from_secs(60 * 60);
const SCRATCH_PREFIX: &str = "reimuplayer_online_";

/// 清扫 `%TEMP%` 下异常退出留下的 `reimuplayer_online_<pid>[_n]` 目录。
///
/// 由设置页**用户手动触发**，不在启动时自动跑：清理是破坏性操作，时机交给用户。
/// 跳过本进程的目录、名字不匹配的目录，以及最近 1 小时内还被写过的目录；
/// 被播放引擎占用的文件会让 `remove_dir_all` 失败，只计入失败数，不重试。
pub(crate) fn sweep_stale_scratch() -> ScratchSweep {
    sweep_stale_scratch_in(
        &std::env::temp_dir(),
        SystemTime::now(),
        SCRATCH_STALE_AFTER,
    )
}

fn sweep_stale_scratch_in(root: &Path, now: SystemTime, min_age: Duration) -> ScratchSweep {
    let mut report = ScratchSweep::default();
    let Ok(entries) = std::fs::read_dir(root) else {
        return report;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(pid) = scratch_pid(name) else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        if pid == std::process::id() {
            report.dirs_skipped += 1;
            continue;
        }
        let path = entry.path();
        let Some(newest) = newest_mtime(&path) else {
            report.dirs_skipped += 1;
            continue;
        };
        // 时钟回拨 / 未来时间一律按「刚写过」处理，宁可少删
        if now.duration_since(newest).map_or(true, |age| age < min_age) {
            report.dirs_skipped += 1;
            continue;
        }
        let (files, bytes) = dir_contents(&path);
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                report.dirs_removed += 1;
                report.files_removed += files;
                report.bytes_freed += bytes;
            }
            Err(e) => {
                report.dirs_failed += 1;
                tracing::debug!("临时目录清扫失败: {} ({})", path.display(), e);
            }
        }
    }
    if report.dirs_removed > 0 || report.dirs_failed > 0 {
        tracing::info!(
            "临时文件清扫：删除 {} 个目录 / {} 字节，跳过 {}，失败 {}",
            report.dirs_removed,
            report.bytes_freed,
            report.dirs_skipped,
            report.dirs_failed
        );
    }
    report
}

/// `reimuplayer_online_<pid>` / `..._<pid>_<n>` → pid；其它名字返回 `None`。
///
/// 只认数字后缀，避免把 `reimuplayer_online_test_...` 这类测试目录或碰巧
/// 同前缀的用户目录卷进来。
fn scratch_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix(SCRATCH_PREFIX)?;
    let mut parts = rest.split('_');
    let pid = parts.next()?.parse().ok()?;
    if parts.all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())) {
        Some(pid)
    } else {
        None
    }
}

fn newest_mtime(dir: &Path) -> Option<SystemTime> {
    let mut newest = std::fs::metadata(dir).ok()?.modified().ok()?;
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let modified = entry.metadata().ok()?.modified().ok()?;
        newest = newest.max(modified);
    }
    Some(newest)
}

fn dir_contents(dir: &Path) -> (usize, u64) {
    let mut files = 0;
    let mut bytes = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (files, bytes);
    };
    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata() {
            if meta.is_file() {
                files += 1;
                bytes += meta.len();
            }
        }
    }
    (files, bytes)
}

/// 临时文件名里的 ID 净化：仅保留 ASCII 字母数字，防路径注入。
fn sanitize_id(id: &str) -> String {
    let mut out: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(32)
        .collect();
    if out.is_empty() {
        out.push_str("unknown");
    }
    out
}

/// `Drop` 的约束必须与结构体声明一致（否则 E0367），所以这里不调用带约束的
/// [`Self::discard_pending`]——直接把 `PendingStream` 摘掉就够了。
/// drop 它即 drop 其中的 [`StreamTask`]，那正是「没人再要这些字节」的信号。
impl<P, F> Drop for OnlineTrackLoader<P, F> {
    fn drop(&mut self) {
        let _ = self.pending.take();
        // 兜底清理：调用方忘记 cleanup 时尽量不留垃圾（尽力而为）
        for path in self.downloaded.drain(..) {
            let _ = std::fs::remove_file(&path);
        }
        let _ = std::fs::remove_dir(&self.temp_dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::music::lyrics::Lyrics;
    use crate::music::provider::{ProviderId, Track};
    use std::sync::Mutex;
    use std::time::Duration;

    // ---- 测试替身（离线确定性）----------------------------------------

    /// 桩 Provider：脚本化 `playback_source` 的返回序列，记录调用次数。
    struct FakeProvider {
        script: Mutex<Vec<std::result::Result<PlaybackSource, ProviderError>>>,
        calls: Mutex<u32>,
        /// 当前音质档位（会话缓存的 epoch 依据）。
        quality: Mutex<QualityCeiling>,
    }

    impl FakeProvider {
        fn new(script: Vec<std::result::Result<PlaybackSource, ProviderError>>) -> Self {
            Self {
                script: Mutex::new(script),
                calls: Mutex::new(0),
                quality: Mutex::new(QualityCeiling::default()),
            }
        }
        fn calls(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
        fn set_quality(&self, ceiling: QualityCeiling) {
            *self.quality.lock().unwrap() = ceiling;
        }
    }

    impl MusicProvider for FakeProvider {
        fn id(&self) -> ProviderId {
            ProviderId::Netease
        }

        fn quality(&self) -> QualityCeiling {
            *self.quality.lock().unwrap()
        }

        async fn search_songs(
            &self,
            _keyword: &str,
            _limit: u32,
        ) -> Result<Vec<Track>, ProviderError> {
            Err(ProviderError::EmptyResult)
        }

        async fn song_details(&self, _ids: &[String]) -> Result<Vec<Track>, ProviderError> {
            Err(ProviderError::EmptyResult)
        }

        async fn playlist(
            &self,
            _playlist_id: &str,
            _offset: u32,
            _limit: u32,
        ) -> Result<crate::music::provider::Playlist, ProviderError> {
            Err(ProviderError::EmptyResult)
        }
        async fn daily_songs(&self) -> Result<Vec<Track>, ProviderError> {
            Err(ProviderError::EmptyResult)
        }
        async fn intelligence_list(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: u32,
        ) -> Result<Vec<Track>, ProviderError> {
            Err(ProviderError::EmptyResult)
        }
        async fn search_match(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: u64,
            _: &str,
        ) -> Result<Vec<Track>, ProviderError> {
            Err(ProviderError::EmptyResult)
        }

        async fn featured_playlists(
            &self,
            _kind: crate::music::provider::FeaturedKind,
            _limit: u32,
        ) -> Result<Vec<crate::music::provider::PlaylistSummary>, ProviderError> {
            Err(ProviderError::EmptyResult)
        }

        async fn check_availability(
            &self,
            _song_id: &str,
        ) -> Result<crate::music::provider::TrackAvailability, ProviderError> {
            Ok(crate::music::provider::TrackAvailability::Available)
        }

        async fn playback_source(&self, _song_id: &str) -> Result<PlaybackSource, ProviderError> {
            *self.calls.lock().unwrap() += 1;
            let mut script = self.script.lock().unwrap();
            if script.is_empty() {
                return Err(ProviderError::PlaybackUrlUnavailable);
            }
            script.remove(0)
        }

        async fn lyrics(&self, _song_id: &str) -> Result<Lyrics, ProviderError> {
            Err(ProviderError::EmptyResult)
        }

        async fn login_status(&self) -> Result<LoginState, ProviderError> {
            Ok(LoginState::Anonymous)
        }

        async fn qr_login_start(&self) -> Result<QrLoginSession, ProviderError> {
            Ok(QrLoginSession {
                key: "fake-key".into(),
                qr_url: "https://example.invalid/qr".into(),
            })
        }

        async fn qr_login_poll(&self, _key: &str) -> Result<LoginState, ProviderError> {
            Ok(LoginState::QrWaiting)
        }

        async fn login_cellphone(&self, _: &str, _: &str) -> Result<LoginState, ProviderError> {
            Ok(LoginState::Anonymous)
        }

        async fn login_cookie(&self, _: &str) -> Result<LoginState, ProviderError> {
            Ok(LoginState::Anonymous)
        }

        async fn logout(&self) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    /// 桩 Fetcher：脚本化每次下载结果（写字节 / 报错），记录调用次数。
    struct FakeFetcher {
        script: Mutex<Vec<std::result::Result<Vec<u8>, ProviderError>>>,
        calls: Mutex<u32>,
        urls: Mutex<Vec<String>>,
    }

    impl FakeFetcher {
        fn new(script: Vec<std::result::Result<Vec<u8>, ProviderError>>) -> Self {
            Self {
                script: Mutex::new(script),
                calls: Mutex::new(0),
                urls: Mutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
        fn urls(&self) -> Vec<String> {
            self.urls.lock().unwrap().clone()
        }
    }

    impl AudioFetcher for FakeFetcher {
        async fn fetch_to_file(&self, url: &str, dest: &Path) -> Result<u64, ProviderError> {
            *self.calls.lock().unwrap() += 1;
            self.urls.lock().unwrap().push(url.to_string());
            let mut script = self.script.lock().unwrap();
            let action = if script.is_empty() {
                Err(ProviderError::PlaybackUrlUnavailable)
            } else {
                script.remove(0)
            };
            match action {
                Ok(bytes) => {
                    std::fs::write(dest, &bytes)
                        .map_err(|e| ProviderError::Unknown(format!("写临时文件失败: {}", e)))?;
                    Ok(bytes.len() as u64)
                }
                Err(e) => {
                    let _ = std::fs::remove_file(dest);
                    Err(e)
                }
            }
        }
    }

    // ---- 素材 -----------------------------------------------------------

    fn source(url: &str, expires_at: Option<i64>) -> PlaybackSource {
        PlaybackSource {
            url: url.to_string(),
            expires_at,
            quality: None,
            content_type: Some("mp3".into()),
            bitrate_kbps: None,
        }
    }

    fn mp3_bytes() -> Vec<u8> {
        let mut v = b"ID3\x03\x00".to_vec();
        v.extend_from_slice(&[0u8; 64]);
        v
    }

    fn html_bytes() -> Vec<u8> {
        b"<!DOCTYPE html><html>404</html>".to_vec()
    }

    /// 最小的 FLAC 头（`fLaC` 魔数 + 一点填充），只求魔数识别成立。
    fn flac_bytes() -> Vec<u8> {
        let mut v = b"fLaC\x00\x00\x00\x22".to_vec();
        v.extend_from_slice(&[0u8; 64]);
        v
    }

    /// 远期不过期的时间戳。
    fn far_future() -> Option<i64> {
        Some(now_unix() + 3600)
    }

    fn fast_loader(
        provider: FakeProvider,
        fetcher: FakeFetcher,
        dir: PathBuf,
    ) -> OnlineTrackLoader<FakeProvider, FakeFetcher> {
        OnlineTrackLoader::new(provider, fetcher)
            .with_temp_dir(dir)
            .with_retry(RetryPolicy {
                max_retries: 2,
                base_delay: Duration::ZERO,
            })
    }

    fn unique_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "reimuplayer_online_test_{}_{}",
            std::process::id(),
            n
        ))
    }

    // ---- 过期判断与魔数校验（纯函数）-----------------------------------

    #[test]
    fn is_expired_respects_margin_and_unknown() {
        let s = source("u", Some(1_000));
        assert!(!is_expired(&s, 900, 30), "距过期还有 70s > margin");
        assert!(is_expired(&s, 971, 30), "距过期不足 30s 视为将过期");
        assert!(is_expired(&s, 1_000, 0), "已过期");

        // expires_at 未知 → 不伪造「已过期」
        let unknown = source("u", None);
        assert!(!is_expired(&unknown, i64::MAX, 30));
    }

    #[test]
    fn looks_like_audio_accepts_real_formats_rejects_html() {
        assert!(looks_like_audio(b"ID3\x03\x00rest"));
        assert!(looks_like_audio(&[0xFF, 0xFB, 0x90, 0x00]), "mp3 帧同步");
        assert!(looks_like_audio(b"fLaC\x00\x00\x00\""));
        assert!(looks_like_audio(b"OggS\x00\x02rest"));
        assert!(looks_like_audio(b"RIFF\x24\x00\x00\x00WAVEfmt "));
        assert!(looks_like_audio(b"\x00\x00\x00\x18ftypM4A "));
        assert!(!looks_like_audio(b"<!DOCTYPE html><html>"));
        assert!(!looks_like_audio(b""));
        assert!(!looks_like_audio(b"abc"), "过短/非音频");
    }

    #[test]
    fn container_hint_prefers_magic_bytes_over_the_api_type() {
        // 接口说是 mp3、实际给的是 FLAC：以文件头为准，面板才会显示 FLAC。
        // 反过来信接口 type 的话，"实际在播什么"永远标错。
        assert_eq!(
            container_hint(b"fLaC\x00\x00\x00\x22", Some("mp3")),
            Some("flac")
        );
        assert_eq!(
            container_hint(b"ID3\x03\x00rest", Some("flac")),
            Some("mp3")
        );
        assert_eq!(
            container_hint(b"RIFF\x24\x00\x00\x00WAVEfmt ", Some("m4a")),
            Some("wav")
        );
        assert_eq!(
            container_hint(b"\x00\x00\x00\x18ftypM4A ", None),
            Some("m4a")
        );
    }

    #[test]
    fn container_hint_falls_back_to_the_api_type() {
        // 魔数认不出时才看接口 type，并容错大小写与 `audio/` MIME 前缀
        let junk = b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b";
        assert_eq!(container_hint(junk, Some("audio/flac")), Some("flac"));
        assert_eq!(container_hint(junk, Some("FLAC")), Some("flac"));
        assert_eq!(container_hint(junk, Some("mpeg")), Some("mp3"));
        assert_eq!(container_hint(junk, Some("  x-flac  ")), Some("flac"));
        // 都不认识 → 明确返回 None（不猜）
        assert_eq!(container_hint(junk, Some("application/octet-stream")), None);
        assert_eq!(container_hint(junk, None), None);
    }

    // ---- 暂存目录 -------------------------------------------------------

    #[test]
    fn create_scratch_creates_the_primary_dir() {
        let dir = unique_dir();
        assert_eq!(create_scratch(&dir).unwrap(), dir);
        assert!(dir.is_dir());
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn create_scratch_branches_around_a_blocked_name() {
        let dir = unique_dir();
        // 主名被一个同名普通文件占死——效果等同 Windows 上「删除挂起」的同名目录
        std::fs::write(&dir, b"not a directory").unwrap();
        let branch =
            dir.with_file_name(format!("{}_1", dir.file_name().unwrap().to_str().unwrap()));

        assert_eq!(create_scratch(&dir).unwrap(), branch);
        assert!(branch.is_dir());
        // 幂等：再问一次仍落在同一个分叉上，不会一路往后挪
        assert_eq!(create_scratch(&dir).unwrap(), branch);

        let _ = std::fs::remove_file(&dir);
        let _ = std::fs::remove_dir(&branch);
    }

    // ---- 残留清扫（设置页「清扫」按钮的底层）----------------------------

    /// 给清扫测试造一个干净的根目录（不放在 `%TEMP%` 下直接扫，避免误删）。
    fn sweep_root() -> PathBuf {
        // unique_dir 的后缀不匹配暂存目录命名，用它当根不会被真实清扫命中
        let dir = unique_dir();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_scratch(root: &Path, name: &str, payload: &[u8]) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("track_0.mp3"), payload).unwrap();
        dir
    }

    #[test]
    fn scratch_pid_only_accepts_our_naming_scheme() {
        assert_eq!(scratch_pid("reimuplayer_online_123"), Some(123));
        assert_eq!(scratch_pid("reimuplayer_online_123_1"), Some(123));
        assert_eq!(scratch_pid("reimuplayer_online_test_1"), None);
        assert_eq!(scratch_pid("reimuplayer_online_123_x"), None);
        assert_eq!(scratch_pid("reimuplayer_online_"), None);
        assert_eq!(scratch_pid("reimuplayer"), None);
    }

    #[test]
    fn sweep_removes_only_old_foreign_scratch_dirs() {
        let root = sweep_root();
        let old_other = make_scratch(&root, "reimuplayer_online_999999", b"12345");
        let old_branch = make_scratch(&root, "reimuplayer_online_999998_1", b"1234567");
        let foreign = make_scratch(&root, "some_other_app_cache", b"12345");
        let plain_file = root.join("reimuplayer_online_999996");
        std::fs::write(&plain_file, b"not a dir").unwrap();

        // 「两小时后」再看：两个暂存目录都算残留；别的应用的目录与
        // 同名普通文件不能被卷进来
        let later = SystemTime::now() + Duration::from_secs(2 * 60 * 60);
        let report = sweep_stale_scratch_in(&root, later, SCRATCH_STALE_AFTER);

        assert_eq!(report.dirs_removed, 2);
        assert_eq!(report.files_removed, 2);
        assert_eq!(report.bytes_freed, 12);
        assert!(!old_other.exists());
        assert!(!old_branch.exists());
        assert!(foreign.exists(), "别的应用的目录不能碰");
        assert!(plain_file.exists(), "同名普通文件不能碰");

        // 新鲜目录（刚写过）必须保留：另一个实例可能正在用
        let root_fresh = sweep_root();
        let fresh = make_scratch(&root_fresh, "reimuplayer_online_999995", b"123");
        let report = sweep_stale_scratch_in(
            &root_fresh,
            SystemTime::now() + Duration::from_secs(10),
            SCRATCH_STALE_AFTER,
        );
        assert_eq!(report.dirs_removed, 0);
        assert_eq!(report.dirs_skipped, 1);
        assert!(fresh.exists());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&root_fresh);
    }

    #[test]
    fn sweep_never_touches_the_current_process_dir() {
        let root = sweep_root();
        let own = make_scratch(
            &root,
            &format!("reimuplayer_online_{}", std::process::id()),
            b"1",
        );

        let later = SystemTime::now() + Duration::from_secs(2 * 60 * 60);
        let report = sweep_stale_scratch_in(&root, later, SCRATCH_STALE_AFTER);

        assert_eq!(report.dirs_removed, 0);
        assert_eq!(report.dirs_skipped, 1);
        assert!(own.exists(), "本进程的暂存目录必须留着");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sweep_is_empty_when_root_is_missing_or_unreadable() {
        let missing = unique_dir().join("nope");
        let later = SystemTime::now() + Duration::from_secs(2 * 60 * 60);
        let report = sweep_stale_scratch_in(&missing, later, SCRATCH_STALE_AFTER);
        assert_eq!(report, ScratchSweep::default());
    }

    // ---- 装载主流程 -----------------------------------------------------

    #[tokio::test]
    async fn prepare_downloads_validates_and_registers_file() {
        let dir = unique_dir();
        let p = FakeProvider::new(vec![Ok(source("http://cdn/x.mp3", far_future()))]);
        let f = FakeFetcher::new(vec![Ok(mp3_bytes())]);
        let mut loader = fast_loader(p, f, dir.clone());

        let track = loader.prepare("347230").await.unwrap();
        assert!(track.path.exists());
        assert_eq!(track.source.url, "http://cdn/x.mp3");
        assert_eq!(loader.downloaded_paths(), &[track.path.clone()]);

        loader.cleanup();
        assert!(!track.path.exists(), "cleanup 应删除临时文件");
    }

    #[tokio::test]
    async fn the_temp_file_is_named_after_the_real_container() {
        // 端到端：接口声明 mp3、实际内容是 FLAC —— 文件名与 codec 都要说 FLAC。
        // 原来文件名恒为 `.audio`，`load_media` 把扩展名大写当 codec → `AUDIO`，
        // 面板上永远看不到真实格式。
        let dir = unique_dir();
        let p = FakeProvider::new(vec![Ok(source("http://cdn/x.mp3", far_future()))]);
        let f = FakeFetcher::new(vec![Ok(flac_bytes())]);
        let mut loader = fast_loader(p, f, dir.clone());

        let track = loader.prepare("1").await.unwrap();
        assert_eq!(
            track.path.extension().and_then(|e| e.to_str()),
            Some("flac"),
            "临时文件后缀必须反映真实容器，而不是接口声明的 mp3"
        );
        assert_eq!(track.spec.codec.as_deref(), Some("FLAC"));
        assert!(
            !track.path.to_string_lossy().contains(".audio"),
            "不得再留下那个假的 .audio 后缀"
        );
        assert!(
            !track.path.to_string_lossy().contains(".part"),
            "下载中间名必须已经改名掉"
        );
        loader.cleanup();
    }

    #[tokio::test]
    async fn an_mp3_keeps_its_own_suffix() {
        // 对照组：真实 mp3 不得被误标成 flac
        let dir = unique_dir();
        let p = FakeProvider::new(vec![Ok(source("http://cdn/y.mp3", far_future()))]);
        let f = FakeFetcher::new(vec![Ok(mp3_bytes())]);
        let mut loader = fast_loader(p, f, dir.clone());

        let track = loader.prepare("2").await.unwrap();
        assert_eq!(track.path.extension().and_then(|e| e.to_str()), Some("mp3"));
        assert_eq!(track.spec.codec.as_deref(), Some("MP3"));
        loader.cleanup();
    }

    #[tokio::test]
    async fn preparing_two_songs_keeps_both_files() {
        // 装载器是临时文件的唯一所有者：连续准备两首，两份都要在盘上。
        // 之前会话层会退役旧文件，换台等于把刚下好的字节又扔了。
        let dir = unique_dir();
        let p = FakeProvider::new(vec![
            Ok(source("http://cdn/a.mp3", far_future())),
            Ok(source("http://cdn/b.mp3", far_future())),
        ]);
        let f = FakeFetcher::new(vec![Ok(mp3_bytes()), Ok(flac_bytes())]);
        let mut loader = fast_loader(p, f, dir.clone());

        let a = loader.prepare("a").await.unwrap();
        let b = loader.prepare("b").await.unwrap();
        assert_ne!(a.path, b.path);
        assert!(a.path.exists() && b.path.exists(), "两份文件必须同时在");
        assert_eq!(loader.downloaded_paths().len(), 2);

        loader.cleanup();
        assert!(!a.path.exists() && !b.path.exists(), "cleanup 后一起删净");
    }

    // ---- 会话缓存 + 预取 -------------------------------------------------

    /// `n` 条互不相同的取址脚本（URL 带序号，便于区分）。
    fn sources(n: usize) -> Vec<std::result::Result<PlaybackSource, ProviderError>> {
        (0..n)
            .map(|i| Ok(source(&format!("http://cdn/{i}.mp3"), far_future())))
            .collect()
    }

    #[tokio::test]
    async fn a_second_prepare_for_the_same_song_is_a_cache_hit() {
        let dir = unique_dir();
        let p = FakeProvider::new(sources(1));
        let f = FakeFetcher::new(vec![Ok(mp3_bytes())]);
        let mut loader = fast_loader(p, f, dir.clone());

        let first = loader.prepare("1").await.unwrap();
        let second = loader.prepare("1").await.unwrap();

        assert_eq!(first.path, second.path);
        assert_eq!(loader.provider.calls(), 1, "命中缓存不得再取址");
        assert_eq!(loader.fetcher.calls(), 1, "命中缓存不得再下载");
        assert_eq!(loader.cache_len(), 1);
        loader.cleanup();
    }

    #[tokio::test]
    async fn the_cache_evicts_the_least_recently_used_entry() {
        let dir = unique_dir();
        let p = FakeProvider::new(sources(3));
        let f = FakeFetcher::new(vec![Ok(mp3_bytes()), Ok(mp3_bytes()), Ok(mp3_bytes())]);
        let mut loader = fast_loader(p, f, dir.clone()).with_cache_limits(2, u64::MAX);

        let a = loader.prepare("a").await.unwrap();
        let b = loader.prepare("b").await.unwrap();
        // 再播一次 a：它的 last_used 变成最新，b 成了最久未用
        let _ = loader.prepare("a").await.unwrap();
        let c = loader.prepare("c").await.unwrap();

        assert!(a.path.exists(), "刚用过的那条必须还在");
        assert!(!b.path.exists(), "最久未用的那条应被淘汰");
        assert!(c.path.exists());
        assert_eq!(loader.cache_len(), 2);
        loader.cleanup();
    }

    #[tokio::test]
    async fn eviction_never_deletes_the_file_the_engine_has_loaded() {
        // 不能靠"Windows 会拒绝删除"来兜底：`RodioEngine::reload()` 是按路径
        // 重开的，文件被删掉后下一次 play/seek 就 MediaRead 失败。
        let dir = unique_dir();
        let p = FakeProvider::new(sources(2));
        let f = FakeFetcher::new(vec![Ok(mp3_bytes()), Ok(mp3_bytes())]);
        let mut loader = fast_loader(p, f, dir.clone()).with_cache_limits(1, u64::MAX);

        let a = loader.prepare("a").await.unwrap();
        loader.set_active(Some(a.path.clone()));
        let b = loader.prepare("b").await.unwrap();

        assert!(a.path.exists(), "引擎装载中的文件绝不能被淘汰");
        assert!(b.path.exists());
        assert_eq!(loader.cache_len(), 2, "允许为正在播放的那一条临时超限");
        loader.cleanup();
    }

    #[tokio::test]
    async fn the_cache_has_a_byte_budget() {
        let dir = unique_dir();
        let p = FakeProvider::new(sources(2));
        let f = FakeFetcher::new(vec![Ok(mp3_bytes()), Ok(mp3_bytes())]);
        // mp3_bytes 约 69 字节 → 100 字节预算装不下两条
        let mut loader = fast_loader(p, f, dir.clone()).with_cache_limits(12, 100);

        let a = loader.prepare("a").await.unwrap();
        assert_eq!(loader.cache_len(), 1);
        let _b = loader.prepare("b").await.unwrap();
        assert!(loader.cached_bytes() <= 100, "必须回到预算内");
        assert!(!a.path.exists(), "超预算时淘汰最旧的那条");
        loader.cleanup();
    }

    #[tokio::test]
    async fn an_expired_entry_is_refetched() {
        let dir = unique_dir();
        let p = FakeProvider::new(sources(2));
        let f = FakeFetcher::new(vec![Ok(mp3_bytes()), Ok(mp3_bytes())]);
        let mut loader = fast_loader(p, f, dir.clone()).with_cache_ttl(Duration::ZERO);

        let a = loader.prepare("a").await.unwrap();
        let b = loader.prepare("a").await.unwrap();

        assert_ne!(a.path, b.path, "TTL 到期必须重新下载到新文件");
        assert_eq!(loader.fetcher.calls(), 2);
        loader.cleanup();
    }

    #[tokio::test]
    async fn changing_the_quality_ceiling_drops_the_session_cache() {
        // 不作废的后果：用户把档位从 LOSSLESS 调到 EX-HIGH，重播同一首听到的
        // 仍是缓存里的 FLAC —— 表现为"设置没生效"。
        let dir = unique_dir();
        let p = FakeProvider::new(sources(2));
        let f = FakeFetcher::new(vec![Ok(mp3_bytes()), Ok(mp3_bytes())]);
        let mut loader = fast_loader(p, f, dir.clone());

        let a = loader.prepare("a").await.unwrap();
        assert_eq!(loader.fetcher.calls(), 1);
        loader.provider.set_quality(QualityCeiling::ExtraHigh);

        let b = loader.prepare("a").await.unwrap();
        assert_eq!(loader.fetcher.calls(), 2, "改档后必须重新下载");
        assert_ne!(a.path, b.path);
        loader.cleanup();
    }

    #[tokio::test]
    async fn a_prefetch_never_cold_recovers() {
        // 预取不值得为一次失败押上两倍下载时间：失败就算了，用户真点播时再试
        let dir = unique_dir();
        let p = FakeProvider::new(sources(2));
        let f = FakeFetcher::new(vec![Ok(html_bytes()), Ok(html_bytes())]);
        let mut loader = fast_loader(p, f, dir.clone());

        let err = loader
            .prepare_kind("a", AudioKind::Prefetch)
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::UnsupportedResponse));
        assert_eq!(loader.provider.calls(), 1, "预取不冷恢复：只取址一次");
        assert_eq!(loader.fetcher.calls(), 1);
        loader.cleanup();
    }

    #[tokio::test]
    async fn prefetch_then_play_reuses_the_download() {
        let dir = unique_dir();
        let p = FakeProvider::new(sources(1));
        let f = FakeFetcher::new(vec![Ok(mp3_bytes())]);
        let mut loader = fast_loader(p, f, dir.clone());

        let pre = loader.prepare_kind("a", AudioKind::Prefetch).await.unwrap();
        let play = loader.prepare("a").await.unwrap();

        assert_eq!(pre.path, play.path);
        assert_eq!(loader.fetcher.calls(), 1, "预取下的字节要能直接被点播复用");
        assert_eq!(loader.provider.calls(), 1);
        loader.cleanup();
    }

    #[tokio::test]
    async fn cleanup_removes_unregistered_files_too() {
        let dir = unique_dir();
        let p = FakeProvider::new(sources(1));
        let f = FakeFetcher::new(vec![Ok(mp3_bytes())]);
        let mut loader = fast_loader(p, f, dir.clone());

        let track = loader.prepare("a").await.unwrap();
        let stray = loader.temp_dir().join("stray.part");
        std::fs::write(&stray, b"junk").unwrap();

        loader.cleanup();
        assert!(!track.path.exists(), "缓存里的也要删");
        assert!(!stray.exists(), "未登记的半成品也要删");
        assert!(!dir.exists(), "目录整体清掉");
    }

    #[tokio::test]
    async fn prepare_self_heals_when_temp_dir_name_is_blocked() {
        let blocked = unique_dir();
        std::fs::write(&blocked, b"not a directory").unwrap();
        let p = FakeProvider::new(vec![Ok(source("http://cdn/x.mp3", far_future()))]);
        let f = FakeFetcher::new(vec![Ok(mp3_bytes())]);
        let mut loader = fast_loader(p, f, blocked.clone());

        let track = loader
            .prepare("347230")
            .await
            .expect("主目录不可用不该让装载失败");
        assert_ne!(loader.temp_dir(), blocked.as_path());
        assert!(track.path.starts_with(loader.temp_dir()));

        loader.cleanup();
        assert!(!track.path.exists(), "cleanup 清理的是实际使用的分叉目录");
        let _ = std::fs::remove_file(&blocked);
    }

    #[tokio::test]
    async fn expired_source_triggers_refetch_before_download() {
        let dir = unique_dir();
        let p = FakeProvider::new(vec![
            Ok(source("http://cdn/stale.mp3", Some(now_unix() - 10))),
            Ok(source("http://cdn/fresh.mp3", far_future())),
        ]);
        let f = FakeFetcher::new(vec![Ok(mp3_bytes())]);
        let mut loader = fast_loader(p, f, dir);

        let track = loader.prepare("1").await.unwrap();
        assert_eq!(track.source.url, "http://cdn/fresh.mp3");
        // 取址两次（第二次为新鲜地址），下载一次且用的是新鲜地址
        assert_eq!(loader.provider.calls(), 2);
        assert_eq!(loader.fetcher.calls(), 1);
        assert_eq!(
            loader.fetcher.urls(),
            vec!["http://cdn/fresh.mp3".to_string()]
        );
    }

    #[tokio::test]
    async fn provider_transient_errors_are_retried_then_succeed() {
        let dir = unique_dir();
        let p = FakeProvider::new(vec![
            Err(ProviderError::RequestTimeout),
            Err(ProviderError::NetworkUnavailable),
            Ok(source("http://cdn/x.mp3", far_future())),
        ]);
        let f = FakeFetcher::new(vec![Ok(mp3_bytes())]);
        let mut loader = fast_loader(p, f, dir);

        assert!(loader.prepare("1").await.is_ok());
        assert_eq!(loader.provider.calls(), 3, "1 次原始 + 2 次重试后成功");
    }

    #[tokio::test]
    async fn content_unavailable_is_never_retried_or_recovered() {
        let dir = unique_dir();
        let p = FakeProvider::new(vec![Err(ProviderError::ContentUnavailable)]);
        let f = FakeFetcher::new(vec![]);
        let mut loader = fast_loader(p, f, dir);

        let err = loader.prepare("1").await.unwrap_err();
        assert_eq!(err, ProviderError::ContentUnavailable);
        assert_eq!(loader.provider.calls(), 1, "不可播不得重试");
        assert_eq!(loader.fetcher.calls(), 0, "取址失败不得下载");
    }

    #[tokio::test]
    async fn download_transient_errors_are_retried_then_succeed() {
        let dir = unique_dir();
        let p = FakeProvider::new(vec![Ok(source("http://cdn/x.mp3", far_future()))]);
        let f = FakeFetcher::new(vec![Err(ProviderError::RequestTimeout), Ok(mp3_bytes())]);
        let mut loader = fast_loader(p, f, dir);

        assert!(loader.prepare("1").await.is_ok());
        assert_eq!(loader.fetcher.calls(), 2, "1 次原始 + 1 次重试后成功");
    }

    #[tokio::test]
    async fn garbage_content_recovers_once_with_fresh_url() {
        let dir = unique_dir();
        let p = FakeProvider::new(vec![
            Ok(source("http://cdn/one.mp3", far_future())),
            Ok(source("http://cdn/two.mp3", far_future())),
        ]);
        // 第一次下载到 HTML 错误页，第二次正常
        let f = FakeFetcher::new(vec![Ok(html_bytes()), Ok(mp3_bytes())]);
        let mut loader = fast_loader(p, f, dir.clone());

        let track = loader.prepare("1").await.unwrap();
        assert!(track.path.exists());
        assert_eq!(loader.provider.calls(), 2, "冷恢复共取址两轮");
        assert_eq!(loader.fetcher.calls(), 2);
        assert_eq!(
            loader.fetcher.urls(),
            vec![
                "http://cdn/one.mp3".to_string(),
                "http://cdn/two.mp3".to_string()
            ],
            "冷恢复必须使用全新取址"
        );
    }

    #[tokio::test]
    async fn garbage_content_twice_fails_clearly_and_cleans_up() {
        let dir = unique_dir();
        let p = FakeProvider::new(vec![
            Ok(source("http://cdn/one.mp3", far_future())),
            Ok(source("http://cdn/two.mp3", far_future())),
        ]);
        let f = FakeFetcher::new(vec![Ok(html_bytes()), Ok(html_bytes())]);
        let mut loader = fast_loader(p, f, dir.clone());

        let err = loader.prepare("1").await.unwrap_err();
        assert_eq!(
            err,
            ProviderError::UnsupportedResponse,
            "恢复后仍失败必须清晰报错"
        );
        assert_eq!(loader.provider.calls(), 2, "冷恢复恰好一次，不得无限重试");
        assert_eq!(loader.fetcher.calls(), 2);
        assert!(
            !dir.exists()
                || std::fs::read_dir(&dir)
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(false),
            "失败不得留下半成品文件"
        );
    }

    #[tokio::test]
    async fn zero_byte_download_is_invalid_data() {
        let dir = unique_dir();
        let p = FakeProvider::new(vec![
            Ok(source("http://cdn/one.mp3", far_future())),
            Ok(source("http://cdn/two.mp3", far_future())),
        ]);
        let f = FakeFetcher::new(vec![Ok(vec![]), Ok(vec![])]);
        let mut loader = fast_loader(p, f, dir);

        let err = loader.prepare("1").await.unwrap_err();
        assert_eq!(
            err,
            ProviderError::InvalidData,
            "空内容清晰报错（已冷恢复一次）"
        );
        assert_eq!(loader.fetcher.calls(), 2);
    }

    #[tokio::test]
    async fn oversized_download_is_rejected_without_recovery() {
        let dir = unique_dir();
        let p = FakeProvider::new(vec![Ok(source("http://cdn/big.mp3", far_future()))]);
        // 报告写了 200 字节，但上限压到 100 → 触发体积防线
        let f = FakeFetcher::new(vec![Ok(vec![0xFF; 200])]);
        let mut loader = fast_loader(p, f, dir).with_max_bytes(100);

        let err = loader.prepare("1").await.unwrap_err();
        assert!(
            matches!(err, ProviderError::Unknown(_)),
            "体积超限是策略拒绝，不属冷恢复集合: {:?}",
            err
        );
        assert_eq!(loader.fetcher.calls(), 1, "不冷恢复、不重复下载");
    }

    #[tokio::test]
    async fn url_unavailable_recovers_once_then_reports() {
        let dir = unique_dir();
        // 第一轮下载报 URL 失效；冷恢复第二轮成功
        let p = FakeProvider::new(vec![
            Ok(source("http://cdn/one.mp3", far_future())),
            Ok(source("http://cdn/two.mp3", far_future())),
        ]);
        let f = FakeFetcher::new(vec![
            Err(ProviderError::PlaybackUrlUnavailable),
            Ok(mp3_bytes()),
        ]);
        let mut loader = fast_loader(p, f, dir);

        assert!(loader.prepare("1").await.is_ok());
        assert_eq!(loader.fetcher.calls(), 2);
    }

    #[tokio::test]
    async fn transient_failure_after_retry_budget_is_clear_error_no_recovery() {
        let dir = unique_dir();
        let p = FakeProvider::new(vec![Ok(source("http://cdn/x.mp3", far_future()))]);
        let f = FakeFetcher::new(vec![
            Err(ProviderError::RequestTimeout),
            Err(ProviderError::RequestTimeout),
            Err(ProviderError::RequestTimeout),
        ]);
        let mut loader = fast_loader(p, f, dir);

        let err = loader.prepare("1").await.unwrap_err();
        assert_eq!(err, ProviderError::RequestTimeout);
        assert_eq!(
            loader.fetcher.calls(),
            3,
            "1 + 2 次重试后放弃；瞬时故障不做冷恢复"
        );
    }

    // ---- 边下边播（容器白名单 + 整首回退）-------------------------------
    //
    // 这一组**只用真下载器**（`HttpAudioFetcher` + 本地测试服务器）：
    // 桩 fetcher 走的是 `stream_to_buffer` 的默认实现，一次 `commit` 就是整首，
    // 闸门永远等来 `Complete`，「文件还在长」这件事根本演不出来。
    // 分层仍成立——测试允许引用 `providers`，生产代码不许。

    use crate::providers::test_support::{pattern, requested_offset, spawn_server, Script};
    use crate::providers::HttpAudioFetcher;
    use std::time::Instant;

    /// 流式素材总长：必须**大于**起播闸门（128 kbps 时闸门 = 512 KiB）。
    const STREAM_BODY: usize = 1_200_000;

    /// `SlowSplit` 的第一段字节数：越过闸门、离整首还远。
    const STREAM_CUT: usize = 600_000;

    /// 两段之间的停顿 = 测试的观测窗口。
    ///
    /// 装载必须在这段时间里把「还在长的」缓冲交出去，所以窗口要宽到
    /// 一次 `probe_spec`（lofty 读头）也跑得完；1.5 s 是百倍的余量。
    const STREAM_PAUSE: Duration = Duration::from_millis(1_500);

    fn url(port: u16) -> String {
        format!("http://127.0.0.1:{port}/a.flac")
    }

    /// 接口声明为无损档的播放地址（`bitrate_kbps` 直接决定起播闸门）。
    fn lossless(url: &str, bitrate_kbps: Option<u32>) -> PlaybackSource {
        PlaybackSource {
            url: url.to_string(),
            content_type: Some("flac".into()),
            bitrate_kbps,
            ..source(url, far_future())
        }
    }

    /// `fLaC` 魔数 + 合法 STREAMINFO（44.1 kHz / 16 bit / 立体声）开头的 `n` 字节素材。
    ///
    /// 白名单判据只看魔数，但头部要做得像真的：装载器会在**还没下完**的文件上
    /// 跑一次 `load_media` 探测流参数，头部畸形就等于那条路径没被测到。
    /// 总样本数字段留 0（不伪造时长），后面直接是图案填充。
    fn flac_body(n: usize) -> Vec<u8> {
        let mut v = b"fLaC".to_vec();
        // 元数据块头：is_last=1、类型 0（STREAMINFO）、长度 34（全部大端）
        v.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]);
        v.extend_from_slice(&4096u16.to_be_bytes()); // 最小块
        v.extend_from_slice(&4096u16.to_be_bytes()); // 最大块
        v.extend_from_slice(&[0u8; 6]); // 帧长未知
        let packed: u64 = (44_100u64 << 44) | (1u64 << 41) | (15u64 << 36);
        v.extend_from_slice(&packed.to_be_bytes());
        v.extend_from_slice(&[0u8; 16]); // MD5 未知
        assert_eq!(v.len(), 42);
        v.extend_from_slice(&pattern(n - v.len()));
        v
    }

    /// 真容器是 mp3（ID3 开头）、长度凑到 `n` 的素材。
    fn mp3_body(n: usize) -> Vec<u8> {
        let mut v = b"ID3\x03\x00".to_vec();
        v.extend_from_slice(&pattern(n - v.len()));
        v
    }

    /// 真下载器版装载器：写盘缓冲压到 16 KiB，水位才会一路小步推进
    /// （默认 1 MiB 时「闸门」和「下完」会撞在同一刻，测不出中间态）。
    fn http_loader(
        script: Vec<std::result::Result<PlaybackSource, ProviderError>>,
        dir: PathBuf,
    ) -> OnlineTrackLoader<FakeProvider, HttpAudioFetcher> {
        OnlineTrackLoader::new(
            FakeProvider::new(script),
            HttpAudioFetcher::new()
                .expect("HTTP 下载器构造失败")
                .with_write_buffer(16 * 1024)
                // 慢一分段不算断流：让 `SlowSplit` 的停顿只演「慢」，不演「死」
                .with_idle_timeout(Duration::from_secs(10)),
        )
        .with_temp_dir(dir)
        .with_retry(RetryPolicy {
            max_retries: 2,
            base_delay: Duration::ZERO,
        })
    }

    /// 轮询到条件成立（写线程在另一条线程上，测试只能按事实收口）。
    fn until(mut cond: impl FnMut() -> bool, budget: Duration) -> bool {
        let started = Instant::now();
        while !cond() {
            if started.elapsed() >= budget {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    /// 临时目录已空（写线程收尾后，删除挂起的名字才会消失）。
    fn dir_is_empty(dir: &Path) -> bool {
        std::fs::read_dir(dir)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn a_lossless_track_is_handed_over_while_the_file_is_still_growing() {
        let body = flac_body(STREAM_BODY);
        let (port, seen) = spawn_server(
            body.clone(),
            vec![Script::SlowSplit],
            STREAM_CUT,
            STREAM_PAUSE,
        );
        let dir = unique_dir();
        let mut loader = http_loader(vec![Ok(lossless(&url(port), Some(128)))], dir.clone());

        let track = loader.prepare("347230").await.unwrap();
        let buf = track
            .stream
            .clone()
            .expect("无损档必须带着还在长的缓冲交回来");

        assert_eq!(buf.declared(), Some(body.len() as u64), "总长以响应头为准");
        assert!(
            buf.written() < body.len() as u64,
            "交回时文件必须还没下完（实际 {} B）",
            buf.written()
        );
        assert!(
            buf.written() >= start_gate(Some(128)),
            "交回时水位必须已过起播闸门"
        );
        assert_eq!(buf.phase(), BufferPhase::Growing);
        assert_eq!(track.path, buf.path().to_path_buf());
        assert_eq!(
            track.path.extension().and_then(|e| e.to_str()),
            Some("flac"),
            "后缀在写线程创建文件之前就按声明容器定死（引擎 reload 按路径重开）"
        );
        assert_eq!(track.spec.codec.as_deref(), Some("FLAC"));
        assert_eq!(
            loader.downloaded_paths(),
            &[track.path.clone()],
            "还在长的文件同样是装载器登记的资产，cleanup 要删得到"
        );
        assert_eq!(
            loader.pending_song(),
            Some("347230"),
            "在飞的那条流归装载器所有"
        );
        assert_eq!(loader.cache_len(), 0, "半截文件绝不进会话缓存");

        // 后半段真的来了，而且逐字节对得上：闸门、刷盘边界、set_len 收尾都没错位
        assert!(
            until(|| buf.phase() == BufferPhase::Done, Duration::from_secs(20)),
            "续传段到手后必须收工，停在 {:?}",
            buf.phase()
        );
        assert_eq!(buf.written(), body.len() as u64);
        assert_eq!(std::fs::read(&track.path).unwrap(), body);

        loader.cleanup();
        assert!(!track.path.exists());
        assert_eq!(seen.lock().unwrap().len(), 1, "一条连接就该把整首送完");
    }

    #[tokio::test]
    async fn a_container_outside_the_whitelist_never_starts_a_stream() {
        // 名单外的容器（mp3）**根本不起流**：不是「起流后再改回整首」。
        // 多起一条流意味着多一个临时文件、多一条线程，还可能把声明容器写进名字。
        let body = mp3_body(600);
        let (port, seen) = spawn_server(
            body.clone(),
            vec![Script::Full],
            0,
            Duration::from_millis(0),
        );
        let dir = unique_dir();
        let mut loader = http_loader(vec![Ok(source(&url(port), far_future()))], dir.clone());

        let track = loader.prepare("1").await.unwrap();
        assert!(track.stream.is_none(), "mp3 不该带着流式缓冲");
        assert_eq!(loader.pending_song(), None);
        assert_eq!(track.path.extension().and_then(|e| e.to_str()), Some("mp3"));
        assert_eq!(std::fs::read(&track.path).unwrap(), body);
        assert_eq!(loader.cache_len(), 1, "整首下完就该进缓存");
        assert_eq!(seen.lock().unwrap().len(), 1, "只发一次整首请求");
        loader.cleanup();
    }

    #[tokio::test]
    async fn a_declaration_that_disagrees_with_the_real_container_falls_back_to_the_whole_file() {
        // 接口说 flac、实际给的是 mp3：文件名已经按声明定死，不能硬开流。
        // 回退代价 = 已到手的那段前缀作废，重下一整首（换不了名，也换不回正确性）。
        let body = mp3_body(STREAM_BODY);
        let (port, seen) = spawn_server(
            body.clone(),
            vec![Script::SlowSplit, Script::Full],
            STREAM_CUT,
            STREAM_PAUSE,
        );
        let dir = unique_dir();
        let mut loader = http_loader(vec![Ok(lossless(&url(port), Some(128)))], dir.clone());

        let track = loader.prepare("1").await.unwrap();
        assert!(track.stream.is_none(), "声明与实际不符时必须回退成整首交付");
        assert_eq!(
            track.path.extension().and_then(|e| e.to_str()),
            Some("mp3"),
            "后缀以真实容器为准（整首路径下完才改名，来得及核对）"
        );
        assert_eq!(track.spec.codec.as_deref(), Some("MP3"));
        assert_eq!(std::fs::read(&track.path).unwrap(), body);
        assert_eq!(loader.provider.calls(), 1, "回退不该重新取址");
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 2, "一轮流 + 一轮整首");
        assert_eq!(requested_offset(&requests[1]), None, "整首请求不带 Range");
        drop(requests);

        loader.cleanup();
    }

    #[tokio::test]
    async fn a_chunked_response_falls_back_to_the_whole_file_without_a_stray_file() {
        // 没有 Content-Length = 总长未知：解码器定不出尾、进度条没有分母。
        // 判死要发生在**响应头到手时**——一个字节都没写，所以没有半成品要扫。
        let body = flac_body(600);
        let (port, seen) = spawn_server(
            body.clone(),
            vec![Script::Chunked, Script::Full],
            0,
            Duration::from_millis(0),
        );
        let dir = unique_dir();
        let mut loader = http_loader(vec![Ok(lossless(&url(port), Some(128)))], dir.clone());

        let track = loader.prepare("1").await.unwrap();
        assert!(track.stream.is_none(), "总长未知只能走整首");
        assert_eq!(
            track.path.extension().and_then(|e| e.to_str()),
            Some("flac")
        );
        assert_eq!(std::fs::read(&track.path).unwrap(), body);
        assert_eq!(
            loader.downloaded_paths(),
            &[track.path.clone()],
            "临时目录里只该有交付的那一份"
        );
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requested_offset(&requests[0]), None);
        drop(requests);
        loader.cleanup();
    }

    #[tokio::test]
    async fn a_streaming_track_reaches_the_cache_only_after_the_last_byte_lands() {
        // 转正时机是缓存正确性的关键：半截文件进缓存 = 下次点播同一首把残缺容器
        // 交给解码器，而且 `cached_bytes` 会按残缺长度记账。
        let body = flac_body(STREAM_BODY);
        let (port, seen) = spawn_server(
            body.clone(),
            vec![Script::SlowSplit],
            STREAM_CUT,
            STREAM_PAUSE,
        );
        let dir = unique_dir();
        let mut loader = http_loader(vec![Ok(lossless(&url(port), Some(128)))], dir.clone());

        let first = loader.prepare("1").await.unwrap();
        let buf = first.stream.clone().expect("第一次交出的是还在长的缓冲");
        assert_eq!(loader.cache_len(), 0);

        assert!(
            until(|| buf.phase() == BufferPhase::Done, Duration::from_secs(20)),
            "收工应由写线程落定，停在 {:?}",
            buf.phase()
        );

        // 下一次点播：先转正、再命中缓存 → 整首形态，且不再碰网络
        let again = loader.prepare("1").await.unwrap();
        assert!(
            again.stream.is_none(),
            "已下完的条目按整首交付，调用方可以走普通 load"
        );
        assert_eq!(again.path, first.path);
        assert_eq!(loader.cache_len(), 1);
        assert_eq!(
            loader.cached_bytes(),
            body.len() as u64,
            "记账以声明总长为准，不是转正那一刻的水位"
        );
        assert_eq!(loader.provider.calls(), 1, "命中缓存不得再取址");
        assert_eq!(seen.lock().unwrap().len(), 1, "命中缓存不得再下载");
        loader.cleanup();
    }

    #[tokio::test]
    async fn clicking_the_song_that_is_still_buffering_reuses_the_same_download() {
        // 「点正在缓冲的那首」不该变成两份并行下载抢一条链路。
        let body = flac_body(STREAM_BODY);
        let (port, seen) = spawn_server(
            body.clone(),
            vec![Script::SlowSplit],
            STREAM_CUT,
            STREAM_PAUSE,
        );
        let dir = unique_dir();
        let mut loader = http_loader(vec![Ok(lossless(&url(port), Some(128)))], dir.clone());

        let first = loader.prepare("1").await.unwrap();
        let second = loader.prepare("1").await.unwrap();

        assert!(Arc::ptr_eq(
            first.stream.as_ref().expect("第一条是流"),
            second.stream.as_ref().expect("第二条应复用同一条缓冲")
        ));
        assert_eq!(second.path, first.path);
        assert_eq!(loader.provider.calls(), 1, "复用不该再取址");
        assert_eq!(seen.lock().unwrap().len(), 1, "复用不该再起一条下载");
        loader.cleanup();
    }

    #[tokio::test]
    async fn the_size_limit_bites_when_the_response_header_arrives() {
        // 90 MB 母带档在整首路径下要「下完才知道播不起」；流式路径拿到声明总长
        // 就能判死，用户不该为播不起的曲目等满整个下载。
        let body = flac_body(STREAM_BODY);
        let (port, seen) = spawn_server(
            body.clone(),
            vec![Script::SlowSplit],
            STREAM_CUT,
            STREAM_PAUSE,
        );
        let dir = unique_dir();
        let mut loader = http_loader(vec![Ok(lossless(&url(port), Some(128)))], dir.clone())
            .with_max_bytes(700_000);

        let err = loader.prepare("1").await.unwrap_err();
        assert!(
            matches!(&err, ProviderError::Unknown(text) if text.contains("超过大小上限")),
            "体积超限要说清是策略拒绝，实际 {err:?}"
        );
        assert_eq!(loader.provider.calls(), 1, "策略拒绝不属冷恢复集合");
        assert!(loader.downloaded_paths().is_empty());
        assert_eq!(seen.lock().unwrap().len(), 1, "判死后不再往下下载整首");
        loader.cleanup();
    }

    #[tokio::test]
    async fn dropping_the_prepare_future_stops_the_writer_and_sweeps_its_file() {
        // 换台 = 装载的 future 被 `select!` 丢弃，而不是 `await` 出错误。
        // 没有 `StreamGuard`，那条下载会一直占着带宽和临时目录直到整首下完——
        // 正是本方案要消灭的那种浪费，所以这条必须钉死。
        let body = flac_body(STREAM_BODY);
        // 只给 1 KB 就按住：闸门（512 KiB）永远到不了，装载正卡在等水位的时候
        let (port, seen) = spawn_server(body, vec![Script::Stall], 1_000, Duration::from_secs(6));
        let dir = unique_dir();
        let mut loader = http_loader(vec![Ok(lossless(&url(port), Some(128)))], dir.clone());

        let cancelled = tokio::time::timeout(Duration::from_millis(300), loader.prepare("1")).await;
        assert!(cancelled.is_err(), "闸门没到之前，装载本该还在等水位");

        assert!(loader.downloaded_paths().is_empty(), "被取消的流没有资产");
        assert_eq!(loader.pending_song(), None);
        // 删除挂起的文件名要等写线程收尾才消失，所以按事实轮询而不是立刻断言
        assert!(
            until(|| dir_is_empty(&dir), Duration::from_secs(20)),
            "取消该把半成品从临时目录里清干净"
        );
        assert_eq!(seen.lock().unwrap().len(), 1, "停手之后不该再连第二条");
    }

    #[tokio::test]
    async fn turning_the_stream_switch_off_restores_the_whole_file_path() {
        // A/B 与排障用的总开关：「起播快但声音不对」时先确认问题在哪一层。
        let body = flac_body(600);
        let (port, seen) = spawn_server(body, vec![Script::Full], 0, Duration::from_millis(0));
        let dir = unique_dir();
        let mut loader = http_loader(vec![Ok(lossless(&url(port), Some(128)))], dir.clone())
            .with_streaming(false);

        let track = loader.prepare("1").await.unwrap();
        assert!(track.stream.is_none(), "开关关掉后一律整首");
        assert!(!loader.is_streaming());
        assert_eq!(loader.cache_len(), 1);
        assert_eq!(seen.lock().unwrap().len(), 1);
        loader.cleanup();
    }

    #[tokio::test]
    async fn a_prefetched_track_is_downloaded_whole_even_when_the_container_streams() {
        // 预取的收益是「下完」而不是「早交」：把没人听的曲子中转到交付形态，
        // 等于让它后台占着整首带宽却可能永远不放。
        let body = flac_body(600);
        let (port, seen) = spawn_server(body, vec![Script::Full], 0, Duration::from_millis(0));
        let dir = unique_dir();
        let mut loader = http_loader(vec![Ok(lossless(&url(port), Some(128)))], dir.clone());

        let track = loader.prepare_kind("1", AudioKind::Prefetch).await.unwrap();
        assert!(track.stream.is_none(), "预取只能整首");
        assert_eq!(loader.cache_len(), 1, "预取的产物就是缓存里的那一条");
        assert_eq!(loader.pending_song(), None);
        assert_eq!(seen.lock().unwrap().len(), 1);
        loader.cleanup();
    }

    #[test]
    fn the_stream_plan_only_opens_a_stream_for_a_whitelisted_on_demand_track() {
        let dir = unique_dir();
        let loader = fast_loader(FakeProvider::new(vec![]), FakeFetcher::new(vec![]), dir);

        // 点播 + 白名单容器 = 开流，且后缀就是声明的容器
        assert_eq!(
            loader.stream_plan(&lossless("http://cdn/a.flac", Some(128)), AudioKind::Play),
            Some("flac")
        );
        // 点播 + 名单外：mp3 的元数据不在开头，ogg/m4a 的泵还没在真流上跑通
        assert_eq!(
            loader.stream_plan(&source("http://cdn/a.mp3", far_future()), AudioKind::Play),
            None
        );
        // 预取：再好的容器也不开流
        assert_eq!(
            loader.stream_plan(
                &lossless("http://cdn/a.flac", Some(128)),
                AudioKind::Prefetch
            ),
            None
        );
        // 接口没给能认出的 `type`：认不出来就不赌
        let unknown = PlaybackSource {
            content_type: Some("application/octet-stream".into()),
            ..lossless("http://cdn/a.bin", None)
        };
        assert_eq!(loader.stream_plan(&unknown, AudioKind::Play), None);
        // MIME 写法与大小写都要认（接口给过 `audio/flac`）
        let mime = PlaybackSource {
            content_type: Some("audio/FLAC ".into()),
            ..lossless("http://cdn/a.flac", None)
        };
        assert_eq!(loader.stream_plan(&mime, AudioKind::Play), Some("flac"));
    }

    #[test]
    fn the_whitelist_can_be_widened_by_the_caller() {
        // 试点用的旁路：加条目要付的代价写在 `with_stream_containers` 的文档里，
        // 这里只锁住「判据是容器名，不是音质档」这件事。
        let dir = unique_dir();
        let loader = fast_loader(FakeProvider::new(vec![]), FakeFetcher::new(vec![]), dir)
            .with_stream_containers(&["flac", "wav"]);
        let wav = PlaybackSource {
            content_type: Some("wav".into()),
            ..source("http://cdn/a.wav", far_future())
        };
        assert_eq!(loader.stream_plan(&wav, AudioKind::Play), Some("wav"));
        assert_eq!(
            loader.stream_plan(&source("http://cdn/a.mp3", far_future()), AudioKind::Play),
            None,
            "白名单之外的容器仍然整首"
        );
    }
}
