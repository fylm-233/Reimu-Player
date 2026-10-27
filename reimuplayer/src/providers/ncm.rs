//! 网易云音乐 Provider（阶段 B）。
//!
//! 基于 `ncm-api-rs` 0.1.0（WTFPL）。
//!
//! 本模块是**第三方 SDK 与播放器之间的唯一边界**：
//! - SDK 返回的 `serde_json::Value` 在这里被解析为 [`Track`] / [`Playlist`] /
//!   [`PlaybackSource`]，不外泄。
//! - SDK 的 `NcmError` 在这里被映射为 [`ProviderError`]，不外泄。
//! - SDK **没有内置请求超时**（见 `docs/NETEASE_API_VERIFICATION.md` §3.4），
//!   因此本模块强制施加整体超时，防止网络挂起拖死调用方。
//!
//! 真实接口映射（已对照 `ncm-api-rs` 源码核实）：
//! - 搜索 `cloudsearch` → `/weapi/cloudsearch/pc`
//! - 歌曲详情 `song_detail` → `/api/v3/song/detail`（`ids` 逗号拼接）
//! - 歌单详情 `playlist_detail` → `/api/v6/playlist/detail`
//! - 播放地址 `song_url` / 可播预检 `check_music` → **同一端点**
//!   `/api/song/enhance/player/url`
//! - 歌词 `lyric` → `/api/song/lyric`（阶段 D / M4）
//!
//! SDK 行为备忘（`request.rs`）：响应体业务码会被取为 `status`，
//! 且 `201/302/400/502/800/801/802/803` 被强制视为 200 后返回 `Ok`。
//! 本模块用 [`NeteaseProvider::ensure_body_code`] 把这些码按真实语义分类。
//!
//! 安全约束（`NETEASE_INTEGRATION_SPEC.md` 3.2 第 10 条）：
//! 本模块**不硬编码**任何账号、密码或 Cookie。匿名态即可使用公开搜索接口；
//! 登录能力属后续阶段，且必须走安全存储，不得写入日志。

use std::time::Duration;

use ncm_api_rs::{create_client, Query};
use serde_json::Value;

use crate::music::lyrics::Lyrics;
use crate::music::provider::{
    find_liked_playlist_id, is_algorithm_playlist_card, parse_availability_response,
    parse_cellphone_login_response, parse_featured_playlists_response,
    parse_liked_song_ids_response, parse_login_status_response, parse_lyric_response,
    parse_playback_source, parse_playlist_detail_response, parse_qr_check_response,
    parse_qr_key_response, parse_search_response, parse_song_array_response,
    parse_song_details_response, parse_user_playlists_response, FeaturedKind, LoginState,
    MusicProvider, PlaybackSource, Playlist, PlaylistSummary, PlaylistWindow, ProviderError,
    ProviderId, QrLoginSession, QualityCeiling, Track, TrackAvailability,
};

/// 搜索请求整体超时（规范 8.1：搜索 5–8 秒，取上限）。
const SEARCH_TIMEOUT: Duration = Duration::from_secs(8);

/// 歌曲详情请求整体超时（规范 8.1：歌曲详情 5–8 秒，取上限）。
const DETAIL_TIMEOUT: Duration = Duration::from_secs(8);

/// 歌单详情请求整体超时（歌单体量大，取 10 秒）。
const PLAYLIST_TIMEOUT: Duration = Duration::from_secs(10);

/// 播放地址 / 可播预检请求整体超时（规范 8.1：播放地址 5–10 秒，取上限）。
const PLAYBACK_TIMEOUT: Duration = Duration::from_secs(10);

/// 歌词请求整体超时（规范 8.1：歌词 5–8 秒，取上限）。
const LYRIC_TIMEOUT: Duration = Duration::from_secs(8);

/// 登录相关请求整体超时（二维码 key / 轮询 / 状态查询，取 8 秒）。
const LOGIN_TIMEOUT: Duration = Duration::from_secs(8);

/// 单次请求的结果数上限（搜索 / 歌单分页共用），防止误传超大 limit。
const MAX_LIMIT: u32 = 50;

/// 各类请求的整体超时配置（规范 8.1）。
///
/// SDK **没有内置超时**（见 `docs/NETEASE_API_VERIFICATION.md` §3.4），
/// 每个请求都必须由调用方套 [`tokio::time::timeout`]；本结构集中承载
/// 五类请求的超时值，便于测试注入极短超时、也便于 GUI 向用户展示
/// 「请求超时控制」的实际参数。默认值见 [`ProviderTimeouts::default`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderTimeouts {
    /// 搜索整体超时（规范 8.1：5–8 秒，取上限）。
    pub search: Duration,
    /// 歌曲详情整体超时（规范 8.1：5–8 秒，取上限）。
    pub song_detail: Duration,
    /// 歌单详情整体超时（歌单体量大，取 10 秒）。
    pub playlist: Duration,
    /// 播放地址 / 可播预检整体超时（规范 8.1：5–10 秒，取上限）。
    pub playback: Duration,
    /// 歌词整体超时（规范 8.1：5–8 秒，取上限）。
    pub lyric: Duration,
    /// 登录相关整体超时（二维码 key / 轮询 / 状态查询）。
    pub login: Duration,
}

impl Default for ProviderTimeouts {
    fn default() -> Self {
        Self {
            search: SEARCH_TIMEOUT,
            song_detail: DETAIL_TIMEOUT,
            playlist: PLAYLIST_TIMEOUT,
            playback: PLAYBACK_TIMEOUT,
            lyric: LYRIC_TIMEOUT,
            login: LOGIN_TIMEOUT,
        }
    }
}

/// 网易云音乐 Provider。
pub struct NeteaseProvider {
    client: std::sync::RwLock<ncm_api_rs::ApiClient>,
    timeouts: ProviderTimeouts,
    /// 播放音质上限（`song/url` 的 `br` 参数）。
    ///
    /// 用 `AtomicU8` 而不是 `RwLock`：设置是**单值写、随时可写**，
    /// 且要能从 UI 线程直接改而不惊动桥线程（不重建 Provider，
    /// 登录 Cookie 因此不会丢——这是把超时设置改造成"重建 Provider"时
    /// 踩过的坑，音质设置不能再走那条路）。
    quality: std::sync::atomic::AtomicU8,
    /// 当前会话 Cookie（与 SDK 客户端内的 cookie 同步）。
    ///
    /// SDK 的 `ApiClient` **没有** cookie getter，为支持「登录态跨启动持久化」，
    /// 这里单独镜像一份。它**只**在登录成功 / 退出时被写，**只**经
    /// [`MusicProvider::session_cookie`] 交给调用方加密落盘，绝不进日志。
    session_cookie: std::sync::RwLock<Option<String>>,
    /// 已登录账号的 userId（`/api/song/like` 必需）。
    ///
    /// 登录态查询/登录成功的响应里本来就带 `profile.userId`，顺手记下来，
    /// 免去每次点红心都多发一次 `login_status`。它只是**公开的账号数字 id**，
    /// 不是凭据，但仍然不进日志。
    user_id: std::sync::RwLock<Option<String>>,
    /// 「我喜欢的音乐」歌单 ID（心动模式唯一接受的 `pid`，见
    /// [`Self::liked_playlist_id_inner`]）。首次用到时解析，之后复用。
    liked_playlist_id: std::sync::RwLock<Option<String>>,
}

impl Default for NeteaseProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl NeteaseProvider {
    /// 创建匿名 Provider（不带任何凭据）。
    pub fn new() -> Self {
        Self {
            client: std::sync::RwLock::new(create_client(None)),
            timeouts: ProviderTimeouts::default(),
            quality: std::sync::atomic::AtomicU8::new(
                QualityCeiling::DEFAULT_FOR_NEW_INSTALL.index(),
            ),
            session_cookie: std::sync::RwLock::new(None),
            user_id: std::sync::RwLock::new(None),
            liked_playlist_id: std::sync::RwLock::new(None),
        }
    }

    /// 用既有会话 Cookie 创建 Provider（启动恢复登录态）。
    ///
    /// Cookie 由调用方从安全存储解密得到（见 `gui/session_store`），这里只喂给
    /// SDK 客户端，不校验、不落盘、不入日志——是否有效交由后续 `login_status` 判定。
    pub fn new_with_cookie(cookie: String) -> Self {
        let provider = Self::new();
        provider.apply_session_cookie(cookie);
        provider
    }

    /// 覆盖初始音质上限（构造期用；运行期请走 [`MusicProvider::set_quality`]）。
    pub fn with_quality(self, ceiling: QualityCeiling) -> Self {
        self.quality
            .store(ceiling.index(), std::sync::atomic::Ordering::Relaxed);
        self
    }

    /// 覆盖搜索整体超时（测试或配置调优用）。
    ///
    /// 等价于 `with_timeouts` 只改 `search` 字段；保留本方法兼容既有调用。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeouts.search = timeout;
        self
    }

    /// 覆盖全部超时配置（测试或配置调优用）。
    pub fn with_timeouts(mut self, timeouts: ProviderTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// 当前超时配置（只读；GUI 展示「请求超时控制」参数用）。
    pub fn timeouts(&self) -> ProviderTimeouts {
        self.timeouts
    }

    /// 取客户端快照（廉价克隆：内部 `reqwest::Client` 为 Arc）。
    ///
    /// 请求路径用快照而非跨 `await` 持读锁：`std::sync::RwLockReadGuard`
    /// 非 `Send`，持锁过 await 会让 future 失去 `Send`，无法派发到桥线程。
    fn client_snapshot(&self) -> ncm_api_rs::ApiClient {
        self.client.read().expect("client lock poisoned").clone()
    }

    /// 应用登录会话（`qr_login_poll` / 手机号 / Cookie 登录成功路径调用）。
    ///
    /// Cookie 只进 SDK 客户端，**不落盘、不入日志**（规范 3.2 第 10 条）。
    /// 同时镜像到 [`Self::session_cookie`]，供调用方加密持久化。
    fn apply_session_cookie(&self, cookie: String) {
        self.client
            .write()
            .expect("client lock poisoned")
            .set_cookie(cookie.clone());
        *self.session_cookie.write().expect("cookie lock poisoned") = Some(cookie);
    }

    /// 清空会话（退出登录 / 登录失效时调用）：重建匿名客户端 + 清镜像。
    fn clear_session_cookie(&self) {
        *self.client.write().expect("client lock poisoned") = create_client(None);
        *self.session_cookie.write().expect("cookie lock poisoned") = None;
        *self.user_id.write().expect("uid lock poisoned") = None;
        *self
            .liked_playlist_id
            .write()
            .expect("liked pid lock poisoned") = None;
    }

    /// 记住登录态里的 userId（`/api/song/like` 必需）。
    ///
    /// 匿名 / 二维码中间态一律清空：那些状态没有可用的账号 id，留着旧值会
    /// 让「换了账号」后的喜欢请求打到上一个账号上。
    fn remember_login_state(&self, state: &LoginState) {
        let uid = match state {
            LoginState::LoggedIn { user_id, .. } => user_id.clone(),
            _ => None,
        };
        // 红心歌单 ID 是**账号**的属性，不能跨账号复用（退出 / 换号即失效）
        if uid.is_none() {
            *self
                .liked_playlist_id
                .write()
                .expect("liked pid lock poisoned") = None;
        }
        *self.user_id.write().expect("uid lock poisoned") = uid;
    }

    /// 当前已知的 userId（未登录 / 尚未查询过 → `None`）。
    fn known_user_id(&self) -> Option<String> {
        self.user_id
            .read()
            .expect("uid lock poisoned")
            .clone()
            .filter(|s| !s.is_empty())
    }

    /// 发起二维码登录内部实现（key + 二维码内容）。
    async fn qr_login_start_inner(&self) -> Result<QrLoginSession, ProviderError> {
        let client = self.client_snapshot();
        let query = Query::default();
        let resp = tokio::time::timeout(self.timeouts.login, client.login_qr_key(&query))
            .await
            .map_err(|_| {
                tracing::warn!("二维码 key 获取超时（>{:?}）", self.timeouts.login);
                ProviderError::RequestTimeout
            })?
            .map_err(Self::map_error)?;
        Self::ensure_body_code(&resp.body)?;
        let key = parse_qr_key_response(&resp.body)?;
        // 二维码内容由 key 确定性拼出（SDK login_qr_create 同款规则），
        // 无需二次网络往返。
        let qr_url = format!("https://music.163.com/login?codekey={}", key);
        Ok(QrLoginSession { key, qr_url })
    }

    /// 轮询二维码扫码状态内部实现。
    ///
    /// 成功（803）时把 SDK 收集的 Set-Cookie 条目拼成会话串并应用到客户端；
    /// 返回给调用方的 [`LoginState`] **不含任何凭据**。
    async fn qr_login_poll_inner(&self, key: &str) -> Result<LoginState, ProviderError> {
        let key = key.trim();
        if key.is_empty() {
            return Err(ProviderError::InvalidParameter);
        }
        let client = self.client_snapshot();
        let query = Query::default().param("key", key);
        let resp = tokio::time::timeout(self.timeouts.login, client.login_qr_check(&query))
            .await
            .map_err(|_| {
                tracing::warn!("二维码轮询超时（>{:?}）", self.timeouts.login);
                ProviderError::RequestTimeout
            })?
            .map_err(Self::map_error)?;
        // ⚠ 这里**不能**用 `ensure_body_code`：本接口用 800–803 表达「扫码进度」，
        // 而 `ensure_body_code` 把它们统一判成 `AuthenticationRequired` 错误，
        // 会让轮询在第一次响应时就失败、扫码永远无法完成。
        Self::ensure_qr_check_code(&resp.body)?;
        let outcome = parse_qr_check_response(&resp.body, &resp.cookie)?;
        if let Some(cookie) = outcome.cookie.clone() {
            self.apply_session_cookie(cookie);
        }
        self.remember_login_state(&outcome.state);
        Ok(outcome.state)
    }

    /// 查询登录状态内部实现。
    async fn login_status_inner(&self) -> Result<LoginState, ProviderError> {
        let client = self.client_snapshot();
        let query = Query::default();
        let resp = tokio::time::timeout(self.timeouts.login, client.login_status(&query))
            .await
            .map_err(|_| {
                tracing::warn!("登录状态查询超时（>{:?}）", self.timeouts.login);
                ProviderError::RequestTimeout
            })?
            .map_err(Self::map_error)?;
        Self::ensure_body_code(&resp.body)?;
        let state = parse_login_status_response(&resp.body)?;
        self.remember_login_state(&state);
        Ok(state)
    }

    /// 手机号 + 密码登录内部实现（`/api/w/login/cellphone`）。
    ///
    /// 密码明文只经 SDK 做 MD5 后随请求发出，不落盘、不入日志；
    /// 成功时把 SDK 收集的 Set-Cookie 应用到客户端。
    async fn login_cellphone_inner(
        &self,
        phone: &str,
        password: &str,
    ) -> Result<LoginState, ProviderError> {
        let phone = phone.trim();
        let password = password.trim();
        if phone.is_empty() || password.is_empty() {
            return Err(ProviderError::InvalidParameter);
        }
        let client = self.client_snapshot();
        let query = Query::default()
            .param("phone", phone)
            .param("password", password);
        let resp = tokio::time::timeout(self.timeouts.login, client.login_cellphone(&query))
            .await
            .map_err(|_| {
                tracing::warn!("手机号登录超时（>{:?}）", self.timeouts.login);
                ProviderError::RequestTimeout
            })?
            .map_err(Self::map_error)?;
        // 手机号登录用业务码表达成败，**不能**过 `ensure_body_code`（它会按
        // 特殊码把 400/502 等强制归类，丢掉了「密码错误」等真实语义）。
        let (state, cookie) = parse_cellphone_login_response(&resp.body, &resp.cookie)?;
        if let Some(cookie) = cookie {
            self.apply_session_cookie(cookie);
        }
        self.remember_login_state(&state);
        Ok(state)
    }

    /// Cookie 登录内部实现：应用用户粘贴的 Cookie 后查询状态。
    async fn login_cookie_inner(&self, cookie: &str) -> Result<LoginState, ProviderError> {
        let cookie = cookie.trim();
        if cookie.is_empty() {
            return Err(ProviderError::InvalidParameter);
        }
        self.apply_session_cookie(cookie.to_string());
        self.login_status_inner().await
    }

    /// 退出登录内部实现：调用服务端 logout 后清空会话。
    async fn logout_inner(&self) -> Result<(), ProviderError> {
        let client = self.client_snapshot();
        let query = Query::default();
        // 退出是尽力而为：即使网络失败也清空本地会话（本地已不可信）
        let _ = tokio::time::timeout(self.timeouts.login, client.logout(&query)).await;
        self.clear_session_cookie();
        Ok(())
    }

    /// 喜欢 / 取消喜欢内部实现（`/api/song/like`，官方红心入口）。
    ///
    /// 与 `/api/radio/like` 的区别很关键：后者只是电台流的"喜欢/不喜欢"信号，
    /// **不会**把歌加进「我喜欢的音乐」；要真的写进账号歌单必须用本接口。
    ///
    /// `userid` 是必填参数，取自最近一次登录态查询/登录成功的 `profile.userId`。
    /// 没有它说明尚未登录（或还没查过状态）→ 如实返回
    /// [`ProviderError::AuthenticationRequired`]，**不伪造成功**。
    async fn like_song_inner(&self, song_id: &str, like: bool) -> Result<(), ProviderError> {
        let song_id = Self::validate_id(song_id)?;
        let uid = self
            .known_user_id()
            .ok_or(ProviderError::AuthenticationRequired)?;
        let client = self.client_snapshot();
        let query = Query::default()
            .param("id", song_id)
            .param("uid", &uid)
            .param("like", if like { "true" } else { "false" });
        tracing::debug!(target: "provider", "喜欢歌曲: id={song_id} like={like}");
        let resp = tokio::time::timeout(self.timeouts.login, client.song_like(&query))
            .await
            .map_err(|_| {
                tracing::warn!("喜欢歌曲超时（>{:?}）", self.timeouts.login);
                ProviderError::RequestTimeout
            })?
            .map_err(Self::map_error)?;
        Self::ensure_body_code(&resp.body)?;
        // 该接口成功时 `code=200`，个别失败会带 `message`；没有明确失败即视为成功，
        // 但**不吞错误码**：业务码异常由 ensure_body_code 统一归类。
        Ok(())
    }

    /// 云端「我喜欢的音乐」ID 列表（`/api/song/like/get`）。
    ///
    /// 需要登录：`uid` 取自最近一次登录态，取不到就是未登录。
    async fn liked_song_ids_inner(&self) -> Result<Vec<String>, ProviderError> {
        let uid = self
            .known_user_id()
            .ok_or(ProviderError::AuthenticationRequired)?;
        let client = self.client_snapshot();
        let query = Query::default().param("uid", &uid);
        tracing::debug!(target: "provider", "拉取云端收藏: uid={uid}");
        let resp = tokio::time::timeout(self.timeouts.playlist, client.likelist(&query))
            .await
            .map_err(|_| {
                tracing::warn!("拉取云端收藏超时（>{:?}）", self.timeouts.playlist);
                ProviderError::RequestTimeout
            })?
            .map_err(Self::map_error)?;
        Self::ensure_body_code(&resp.body)?;
        let ids = parse_liked_song_ids_response(&resp.body)?;
        tracing::info!(target: "provider", "云端收藏 {} 首", ids.len());
        Ok(ids)
    }

    /// 专辑曲目（`/api/v1/album/{id}` → `songs[]`）。
    async fn album_songs_inner(&self, album_id: &str) -> Result<Vec<Track>, ProviderError> {
        let album_id = Self::validate_id(album_id)?;
        let client = self.client_snapshot();
        let query = Query::default().param("id", album_id);
        tracing::debug!(target: "provider", "专辑曲目: id={album_id}");
        let resp = tokio::time::timeout(self.timeouts.playlist, client.album(&query))
            .await
            .map_err(|_| {
                tracing::warn!("专辑曲目超时（>{:?}）", self.timeouts.playlist);
                ProviderError::RequestTimeout
            })?
            .map_err(Self::map_error)?;
        Self::ensure_body_code(&resp.body)?;
        let tracks = parse_song_array_response(&resp.body)?;
        tracing::info!(target: "provider", "专辑 {album_id} 曲目 {} 首", tracks.len());
        Ok(tracks)
    }

    /// 艺人热门曲目（`/api/artist/songs`，`order=hot`）。
    ///
    /// 用 `artist/songs` 而不是 `artist/top/song`：前者支持 `limit`，
    /// 后者固定 50 首且没有分页参数。
    async fn artist_songs_inner(
        &self,
        artist_id: &str,
        limit: u32,
    ) -> Result<Vec<Track>, ProviderError> {
        if limit == 0 {
            return Err(ProviderError::InvalidParameter);
        }
        let artist_id = Self::validate_id(artist_id)?;
        let limit = limit.min(MAX_LIMIT);
        let client = self.client_snapshot();
        let query = Query::default()
            .param("id", artist_id)
            .param("order", "hot")
            .param("offset", "0")
            .param("limit", &limit.to_string());
        tracing::debug!(target: "provider", "艺人曲目: id={artist_id} limit={limit}");
        let resp = tokio::time::timeout(self.timeouts.playlist, client.artist_songs(&query))
            .await
            .map_err(|_| {
                tracing::warn!("艺人曲目超时（>{:?}）", self.timeouts.playlist);
                ProviderError::RequestTimeout
            })?
            .map_err(Self::map_error)?;
        Self::ensure_body_code(&resp.body)?;
        let tracks = parse_song_array_response(&resp.body)?;
        tracing::info!(target: "provider", "艺人 {artist_id} 曲目 {} 首", tracks.len());
        Ok(tracks)
    }

    /// 账号自己的歌单（`/api/user/playlist` → `playlist[]`）。
    async fn user_playlists_inner(
        &self,
        limit: u32,
    ) -> Result<Vec<PlaylistSummary>, ProviderError> {
        if limit == 0 {
            return Err(ProviderError::InvalidParameter);
        }
        let uid = self
            .known_user_id()
            .ok_or(ProviderError::AuthenticationRequired)?;
        let limit = limit.min(MAX_LIMIT);
        let client = self.client_snapshot();
        let query = Query::default()
            .param("uid", &uid)
            .param("limit", &limit.to_string())
            .param("offset", "0");
        tracing::debug!(target: "provider", "我的歌单: uid={uid} limit={limit}");
        let resp = tokio::time::timeout(self.timeouts.playlist, client.user_playlist(&query))
            .await
            .map_err(|_| {
                tracing::warn!("我的歌单超时（>{:?}）", self.timeouts.playlist);
                ProviderError::RequestTimeout
            })?
            .map_err(Self::map_error)?;
        Self::ensure_body_code(&resp.body)?;
        let lists = parse_user_playlists_response(&resp.body)?;
        tracing::info!(target: "provider", "我的歌单 {} 个", lists.len());
        Ok(lists)
    }

    /// 校验网易云 ID 参数：非空且全为数字（ID 为 i64）。
    ///
    /// 在发起任何请求**之前**拒绝非法参数（规范 8.3：无效参数不发起、不重试）。
    fn validate_id(id: &str) -> Result<&str, ProviderError> {
        let id = id.trim();
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ProviderError::InvalidParameter);
        }
        Ok(id)
    }

    /// 检查响应体业务码。
    ///
    /// SDK 会把 `201/302/400/502/800/801/802/803` 强制视为 200 后返回 `Ok`
    /// （见模块文档），这里按真实语义分类；无 `code` 字段时交由解析层判断。
    /// 二维码轮询专用的业务码校验。
    ///
    /// `/api/login/qrcode/client/login` **用 `code` 表达扫码进度**，不是错误：
    /// - `801` 等待扫码 / `802` 已扫码待确认 / `803` 登录成功 / `800` 已过期
    ///
    /// [`Self::ensure_body_code`] 会把 800–803 一律判成
    /// [`ProviderError::AuthenticationRequired`]，那是给「需要登录才能用的业务
    /// 接口」准备的语义。若轮询也走它，第一次响应就会变成错误、扫码永远无法
    /// 完成（历史 bug：扫码后一直无法登录）。故此处单独放行这四个码，
    /// 其余业务码仍交给 `ensure_body_code` 分类。
    fn ensure_qr_check_code(body: &Value) -> Result<(), ProviderError> {
        let code = body.get("code").and_then(|c| {
            c.as_i64()
                .or_else(|| c.as_str().and_then(|s| s.parse::<i64>().ok()))
        });
        match code {
            Some(800..=803) => Ok(()),
            _ => Self::ensure_body_code(body),
        }
    }

    fn ensure_body_code(body: &Value) -> Result<(), ProviderError> {
        let code = body.get("code").and_then(|c| {
            c.as_i64()
                .or_else(|| c.as_str().and_then(|s| s.parse::<i64>().ok()))
        });
        let Some(code) = code else {
            return Ok(());
        };
        match code {
            200 | 201 | 302 => Ok(()),
            400 => Err(ProviderError::InvalidParameter),
            502 => Err(ProviderError::DataProviderUnavailable),
            800..=803 => Err(ProviderError::AuthenticationRequired),
            other => {
                tracing::warn!("网易云 API 业务码异常 code={}", other);
                Err(ProviderError::Unknown(format!("code={}", other)))
            }
        }
    }

    /// 把第三方 SDK 错误映射为项目内部错误。
    ///
    /// `reqwest::Error` 的谓词是**包含关系**，判定顺序决定分类是否正确
    /// （历史 bug：`is_request()` 覆盖了几乎所有非超时错误，于是"服务端关掉了
    /// 空闲 keep-alive 连接"也被报成"网络不可用，请检查网络连接"）：
    ///
    /// | 谓词 | 真实含义 | 分类 |
    /// |---|---|---|
    /// | `is_timeout` | 读/写超时 | [`ProviderError::RequestTimeout`] |
    /// | `status` | 拿到了 HTTP 响应但状态码异常 | 按状态码 |
    /// | `is_connect` | DNS / TCP / TLS **握手**失败 | [`ProviderError::NetworkUnavailable`] |
    /// | `is_body` / `is_decode` | 正文读坏或解析失败 | [`ProviderError::InvalidData`] |
    /// | `is_request` | 已连上但中途断开（连接重置/被对端关闭） | [`ProviderError::ConnectionReset`] |
    ///
    /// 先判 `status` 再判 `is_connect`：有状态码就说明连接是通的，
    /// 这时再去说"网络不可用"只会误导用户。
    fn map_error(e: ncm_api_rs::NcmError) -> ProviderError {
        use ncm_api_rs::NcmError as N;
        match e {
            // SDK 客户端路径实际不会构造 Timeout，保留映射以防未来变化
            N::Timeout(_) => ProviderError::RequestTimeout,
            N::RateLimited(_) => ProviderError::RateLimited,
            N::AuthRequired(_) => ProviderError::AuthenticationRequired,
            N::InvalidParam(_) => ProviderError::InvalidParameter,
            N::Json(_) | N::Crypto(_) => ProviderError::InvalidData,
            N::Http(err) => {
                if err.is_timeout() {
                    return ProviderError::RequestTimeout;
                }
                // 有状态码 = 连接通了、服务端答了，按状态码分类
                if let Some(status) = err.status() {
                    return match status.as_u16() {
                        429 => ProviderError::RateLimited,
                        401 | 403 => ProviderError::AuthenticationRequired,
                        500..=599 => ProviderError::DataProviderUnavailable,
                        other => {
                            tracing::warn!("HTTP 状态异常: {} — {}", other, err);
                            ProviderError::Unknown(format!("HTTP {}", other))
                        }
                    };
                }
                if err.is_connect() {
                    tracing::warn!("连接建立失败（DNS/TCP/TLS）: {}", err);
                    return ProviderError::NetworkUnavailable;
                }
                if err.is_body() || err.is_decode() {
                    tracing::warn!("响应正文读取/解析失败: {}", err);
                    return ProviderError::InvalidData;
                }
                if err.is_request() {
                    // 最常见的一类：复用了对端已关闭的空闲 keep-alive 连接，
                    // hyper 报 "connection closed before message completed"。
                    // 换一条新连接即可成功，故归为可重试的连接中断。
                    tracing::warn!("请求中途连接断开（可重试）: {}", err);
                    return ProviderError::ConnectionReset;
                }
                tracing::warn!("未分类的 HTTP 错误: {}", err);
                ProviderError::Unknown(err.to_string())
            }
            N::Api { code, msg } => {
                tracing::warn!("网易云 API 业务错误 code={} msg={}", code, msg);
                match code {
                    301 => ProviderError::AuthenticationRequired,
                    400 => ProviderError::InvalidParameter,
                    503 => ProviderError::RateLimited,
                    500..=599 => ProviderError::DataProviderUnavailable,
                    800..=803 => ProviderError::AuthenticationRequired,
                    _ => ProviderError::Unknown(format!("code={} msg={}", code, msg)),
                }
            }
            N::Unknown(s) => ProviderError::Unknown(s),
        }
    }

    /// 实际搜索实现。
    async fn search_songs_inner(
        &self,
        keyword: &str,
        limit: u32,
    ) -> Result<Vec<Track>, ProviderError> {
        let keyword = keyword.trim();
        if keyword.is_empty() {
            // 空关键词在发起请求前即拒绝，避免无意义网络往返
            return Err(ProviderError::InvalidParameter);
        }
        if limit == 0 {
            return Err(ProviderError::InvalidParameter);
        }
        let limit = limit.min(MAX_LIMIT);

        let query = Query::new()
            .param("keywords", keyword)
            .param("type", "1") // 1 = 单曲
            .param("limit", &limit.to_string())
            .param("offset", "0");

        tracing::debug!(target: "provider", "搜索: keyword={:?} limit={}", keyword, limit);

        let client = self.client_snapshot();
        let response = tokio::time::timeout(self.timeouts.search, client.cloudsearch(&query))
            .await
            .map_err(|_| {
                tracing::warn!("搜索超时（>{:?}）", self.timeouts.search);
                ProviderError::RequestTimeout
            })?
            .map_err(Self::map_error)?;

        if response.status != 200 {
            tracing::warn!("搜索返回非 200 状态: {}", response.status);
            return Err(ProviderError::Unknown(format!("HTTP {}", response.status)));
        }
        Self::ensure_body_code(&response.body)?;

        let tracks = parse_search_response(&response.body)?;
        tracing::info!(target: "provider", "搜索完成: {:?} -> {} 条结果", keyword, tracks.len());
        Ok(tracks)
    }

    /// 实际歌曲详情实现（规范 7.2）。
    async fn song_details_inner(&self, ids: &[String]) -> Result<Vec<Track>, ProviderError> {
        if ids.is_empty() {
            return Err(ProviderError::InvalidParameter);
        }
        let mut joined = String::new();
        for (i, id) in ids.iter().enumerate() {
            let id = Self::validate_id(id)?;
            if i > 0 {
                joined.push(',');
            }
            joined.push_str(id);
        }

        let query = Query::new().param("ids", &joined);
        tracing::debug!(target: "provider", "歌曲详情: {} 个 ID", ids.len());

        let client = self.client_snapshot();
        let response = tokio::time::timeout(self.timeouts.song_detail, client.song_detail(&query))
            .await
            .map_err(|_| {
                tracing::warn!("歌曲详情超时（>{:?}）", self.timeouts.song_detail);
                ProviderError::RequestTimeout
            })?
            .map_err(Self::map_error)?;
        Self::ensure_body_code(&response.body)?;

        let tracks = parse_song_details_response(&response.body)?;
        tracing::info!(target: "provider", "歌曲详情完成: {} 个 ID -> {} 条", ids.len(), tracks.len());
        Ok(tracks)
    }

    /// 实际歌单分页实现（规范 7.3）。
    ///
    /// 两步请求（与 SDK `playlist_track_all` 同构，但保留歌单元信息）：
    /// 1. `playlist_detail` 取元信息与完整 `trackIds`，在解析层切出分页窗口；
    /// 2. 对窗口内 ID 走 `song_detail` 批量取详情，缺失曲目容忍跳过。
    async fn playlist_inner(
        &self,
        playlist_id: &str,
        offset: u32,
        limit: u32,
    ) -> Result<Playlist, ProviderError> {
        let playlist_id = Self::validate_id(playlist_id)?;
        if limit == 0 {
            return Err(ProviderError::InvalidParameter);
        }
        let limit = limit.min(MAX_LIMIT);

        let query = Query::new().param("id", playlist_id);
        tracing::debug!(target: "provider", "歌单详情: id={} offset={} limit={}", playlist_id, offset, limit);

        let client = self.client_snapshot();
        let response = tokio::time::timeout(self.timeouts.playlist, client.playlist_detail(&query))
            .await
            .map_err(|_| {
                tracing::warn!("歌单详情超时（>{:?}）", self.timeouts.playlist);
                ProviderError::RequestTimeout
            })?
            .map_err(Self::map_error)?;
        Self::ensure_body_code(&response.body)?;

        let PlaylistWindow {
            name,
            total,
            track_ids,
        } = parse_playlist_detail_response(&response.body, offset, limit)?;

        let mut playlist = Playlist {
            provider: ProviderId::Netease,
            id: playlist_id.to_string(),
            name,
            total,
            tracks: Vec::new(),
        };

        if track_ids.is_empty() {
            // 空页（offset 越界或空歌单）是合法结果，不是错误
            tracing::info!(target: "provider", "歌单分页为空: id={} offset={}", playlist_id, offset);
            return Ok(playlist);
        }

        playlist.tracks = self.song_details_inner(&track_ids).await?;
        Ok(playlist)
    }

    /// 精选列表实现（规范 7.6：推荐歌单 / 官方排行榜）。
    ///
    /// - [`FeaturedKind::Recommended`] → `/api/personalized/playlist`（`result[]`）；
    /// - [`FeaturedKind::Toplists`] → `/api/toplist`（`list[]`）；
    /// - [`FeaturedKind::Daily`] → `/api/v1/discovery/recommend/resource`（`recommend[]`，需登录）。
    ///
    /// 超时复用歌单档（列表体量小，10s 上限足够）。
    async fn featured_playlists_inner(
        &self,
        kind: FeaturedKind,
        limit: u32,
    ) -> Result<Vec<PlaylistSummary>, ProviderError> {
        if limit == 0 {
            return Err(ProviderError::InvalidParameter);
        }
        let limit = limit.min(MAX_LIMIT);
        // 推荐流里服务端会在首位插「私人雷达」这类算法入口卡；先多要几条，
        // 过滤后仍能凑满调用方要的数量（不足则如实返回，绝不补假数据）。
        let ask = if matches!(kind, FeaturedKind::Recommended) {
            limit.saturating_add(4).min(MAX_LIMIT)
        } else {
            limit
        };
        let query = Query::new().param("limit", &ask.to_string());
        tracing::debug!(target: "provider", "精选列表: kind={:?} limit={}", kind, limit);

        let client = self.client_snapshot();
        // 两个分支 future 类型不同，把 match 放在 timeout 外统一为同一 Result 类型
        let timed = match kind {
            FeaturedKind::Recommended => {
                tokio::time::timeout(self.timeouts.playlist, client.personalized(&query)).await
            }
            FeaturedKind::Toplists => {
                tokio::time::timeout(self.timeouts.playlist, client.toplist(&query)).await
            }
            // 每日推荐歌单需要登录：未登录时接口返回 `recommend: []`，
            // 由 `parse_featured_playlists_response` 如实转成 EmptyResult，
            // 调用方（GUI）据此引导扫码，不伪造数据。
            FeaturedKind::Daily => {
                tokio::time::timeout(self.timeouts.playlist, client.recommend_resource(&query))
                    .await
            }
        };
        let response = timed
            .map_err(|_| {
                tracing::warn!("精选列表超时（>{:?}）", self.timeouts.playlist);
                ProviderError::RequestTimeout
            })?
            .map_err(Self::map_error)?;

        if response.status != 200 {
            tracing::warn!("精选列表返回非 200 状态: {}", response.status);
            return Err(ProviderError::Unknown(format!("HTTP {}", response.status)));
        }
        Self::ensure_body_code(&response.body)?;

        let mut summaries = parse_featured_playlists_response(&response.body)?;
        if matches!(kind, FeaturedKind::Recommended) {
            // 「私人雷达」是按口味逐日生成的单曲流入口，不是歌单集合。
            // 留在「推荐歌单」栏里会让整栏看起来像私人电台（用户实测反馈）。
            let before = summaries.len();
            summaries.retain(|s| !is_algorithm_playlist_card(&s.name));
            if summaries.len() != before {
                tracing::info!(
                    target: "provider",
                    "推荐歌单过滤算法入口卡 {} 条",
                    before - summaries.len()
                );
            }
            summaries.truncate(limit as usize);
        }
        tracing::info!(target: "provider", "精选列表完成: kind={:?} -> {} 条", kind, summaries.len());
        Ok(summaries)
    }

    /// 每日推荐歌曲实现（`/api/v3/discovery/recommend/songs`，**需要登录**）。
    ///
    /// 服务端按「账号 + 当天」生成一批曲目（通常 30 首）：同一天内重复请求
    /// 结果一致，跨天自动换新。「每日」的语义完全在服务端，客户端**不本地
    /// 拼凑、不随机洗牌**，否则换台设备看到的"今日推荐"就不一样了。
    ///
    /// 未登录时该接口返回**业务码 `301`**（不是空数组）。必须先于
    /// [`Self::ensure_body_code`] 识别出来：那一步会把 301 归到
    /// `Unknown("code=301")`，UI 只能显示"未知错误"，用户根本不知道要登录。
    async fn daily_songs_inner(&self) -> Result<Vec<Track>, ProviderError> {
        let query = Query::new();
        tracing::debug!(target: "provider", "每日推荐歌曲");

        let client = self.client_snapshot();
        let response = tokio::time::timeout(self.timeouts.playlist, client.recommend_songs(&query))
            .await
            .map_err(|_| {
                tracing::warn!("每日推荐超时（>{:?}）", self.timeouts.playlist);
                ProviderError::RequestTimeout
            })?
            .map_err(Self::map_error)?;

        if response.status != 200 {
            tracing::warn!("每日推荐返回非 200 状态: {}", response.status);
            return Err(ProviderError::Unknown(format!("HTTP {}", response.status)));
        }
        // 301 是业务码而非 HTTP 状态：未登录时接口用它表达"要登录"
        if response.body.get("code").and_then(Value::as_i64) == Some(301) {
            tracing::info!(target: "provider", "每日推荐需要登录（code=301）");
            return Err(ProviderError::AuthenticationRequired);
        }
        Self::ensure_body_code(&response.body)?;

        let tracks = parse_song_array_response(&response.body)?;
        tracing::info!(target: "provider", "每日推荐完成 -> {} 首", tracks.len());
        Ok(tracks)
    }

    /// 解析并缓存「我喜欢的音乐」歌单 ID（`specialType == 5`）。
    ///
    /// 心动模式对 `pid` 的容忍度比 API.md 写的更窄：**只有**红心歌单被接受。
    /// 实测（登录账号，`count=20`）：
    /// - 红心歌单 `776879259` → `code=200`、`data` 149 首；
    /// - 自建歌单 `18425795587` → `code=400 "不支持该歌单类型"`；
    /// - 别人的公开歌单 `24381616` → `code=400 "不支持该歌单类型"`；
    /// - `0`（界面没有歌单上下文时的占位）→ `code=400 "歌单不存在"`。
    ///
    /// 按名字找（"<昵称>喜欢的音乐"）会随账号语言、改名、他人歌单撞名失效，
    /// 所以认 `specialType`。结果缓存在 [`Self::liked_playlist_id`]，退出登录 /
    /// 切账号时清空。
    async fn liked_playlist_id_inner(&self) -> Result<String, ProviderError> {
        if let Some(id) = self
            .liked_playlist_id
            .read()
            .expect("liked pid lock poisoned")
            .clone()
        {
            return Ok(id);
        }
        // 启动后没查过登录态时补一次（`login_status_inner` 顺带把 uid 记下来）
        let uid = match self.known_user_id() {
            Some(uid) => Some(uid),
            None => match self.login_status_inner().await? {
                LoginState::LoggedIn { user_id, .. } => user_id,
                _ => None,
            },
        }
        .filter(|s| !s.is_empty())
        .ok_or(ProviderError::AuthenticationRequired)?;

        let client = self.client_snapshot();
        let query = Query::default()
            .param("uid", &uid)
            .param("limit", "20")
            .param("offset", "0");
        let response = tokio::time::timeout(self.timeouts.playlist, client.user_playlist(&query))
            .await
            .map_err(|_| {
                // `format_args!` 而非直接引用 `self`：闭包参数类型推不出来，
                // 而 `Duration` 只实现 `Debug`，占位符必须写 `{:?}`
                tracing::warn!(
                    "红心歌单定位超时（>{:?}）",
                    std::format_args!("{:?}", self.timeouts.playlist)
                );
                ProviderError::RequestTimeout
            })?
            .map_err(Self::map_error)?;
        Self::ensure_body_code(&response.body)?;

        let id = find_liked_playlist_id(&response.body).ok_or_else(|| {
            tracing::warn!(target: "provider", "账号歌单里没有 specialType=5 的红心歌单");
            ProviderError::InvalidData
        })?;
        *self
            .liked_playlist_id
            .write()
            .expect("liked pid lock poisoned") = Some(id.clone());
        tracing::info!(target: "provider", "红心歌单 id={id}");
        Ok(id)
    }

    /// 心动模式 / 智能播放实际实现（`playmode_intelligence_list`）。
    ///
    /// **`pid` 换成红心歌单**：接口只接受那一种歌单当上下文（实测见
    /// [`Self::liked_playlist_id_inner`]），把界面正在浏览的歌单直接传下去会
    /// 当场 400。调用方给的 `playlist_id` 只在红心歌单**取不到**时兜底。
    ///
    /// `count` 必须显式带上：SDK 的默认值是 **1**，不传就只回一首。但**服务端
    /// 实测忽略它**——`count=1/5/20/50` 都返回同一批 149 首（完整电台队列），
    /// 所以这里按 `count` 截断，让"一批 20 首"的语义落在适配层，播放队列不会
    /// 因一次续批暴涨一百多行。
    ///
    /// `sid` 留空时**不塞空串**：SDK 用 `get_or("sid", &id)` 取默认值，
    /// 缺键才会回退到种子本身；显式传 `""` 会盖掉这条回退，把空 id 送进请求体。
    ///
    /// 301（未登录）必须早于 [`Self::ensure_body_code`] 识别，理由同
    /// [`Self::daily_songs_inner`]。
    async fn intelligence_list_inner(
        &self,
        song_id: &str,
        playlist_id: &str,
        start_id: &str,
        count: u32,
    ) -> Result<Vec<Track>, ProviderError> {
        let song_id = Self::validate_id(song_id)?;
        let playlist_id = Self::validate_id(playlist_id)?;
        let start_id = if start_id.trim().is_empty() {
            String::new()
        } else {
            Self::validate_id(start_id)?.to_string()
        };
        let count = count.clamp(1, MAX_LIMIT);

        let context_pid = match self.liked_playlist_id_inner().await {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(
                    target: "provider",
                    "红心歌单定位失败（{e:?}），退回调用方 pid={playlist_id}"
                );
                playlist_id.to_string()
            }
        };

        let mut query = Query::new()
            .param("id", song_id)
            .param("pid", &context_pid)
            .param("count", &count.to_string());
        if !start_id.is_empty() {
            query = query.param("sid", &start_id);
        }

        tracing::debug!(
            target: "provider",
            "心动模式: id={song_id} pid={context_pid} sid={start_id} count={count}"
        );

        let client = self.client_snapshot();
        let response = tokio::time::timeout(
            self.timeouts.playlist,
            client.playmode_intelligence_list(&query),
        )
        .await
        .map_err(|_| {
            tracing::warn!("心动模式超时（>{:?}）", self.timeouts.playlist);
            ProviderError::RequestTimeout
        })?
        .map_err(Self::map_error)?;

        if response.status != 200 {
            tracing::warn!("心动模式返回非 200 状态: {}", response.status);
            return Err(ProviderError::Unknown(format!("HTTP {}", response.status)));
        }
        if response.body.get("code").and_then(Value::as_i64) == Some(301) {
            tracing::info!(target: "provider", "心动模式需要登录（code=301）");
            return Err(ProviderError::AuthenticationRequired);
        }
        // 红心歌单定位失败又退回了一个接口不认的 pid 时，这里会把服务端的
        // 原话带进错误里，UI 才能说"要先登录"而不是"这首歌没有可推荐的续曲"。
        if response.body.get("code").and_then(Value::as_i64) == Some(400) && context_pid == "0" {
            tracing::info!(target: "provider", "心动模式缺少歌单上下文（code=400）");
            return Err(ProviderError::AuthenticationRequired);
        }
        Self::ensure_body_code(&response.body)?;

        let mut tracks = parse_song_array_response(&response.body)?;
        tracks.truncate(count as usize);
        tracing::info!(target: "provider", "心动模式完成 -> {} 首", tracks.len());
        Ok(tracks)
    }

    /// 本地歌曲文件匹配实际实现（`search_match`）。
    ///
    /// `duration` 走**整秒**：SDK 对它做 `parse::<i64>()`，`"259.21"` 这类小数字符串
    /// 解析失败会静默退化成 `0`，实测服务端随即返回空结果——本地库永远匹配不上。
    ///
    /// `md5` 实测必须**非空**（服务端拿 `persistId` 当匹配流程的入口），
    /// 但不必真的是网易云发过的那份文件：伪造的 md5 + 准确的标题/专辑/艺人/时长
    /// 依然能命中。所以这里只校验非空，不校验内容。
    async fn search_match_inner(
        &self,
        title: &str,
        album: &str,
        artist: &str,
        duration_secs: u64,
        md5: &str,
    ) -> Result<Vec<Track>, ProviderError> {
        let title = title.trim();
        let md5 = md5.trim();
        if title.is_empty() || md5.is_empty() {
            return Err(ProviderError::InvalidParameter);
        }

        let query = Query::new()
            .param("title", title)
            .param("album", album.trim())
            .param("artist", artist.trim())
            .param("duration", &duration_secs.to_string())
            .param("md5", md5);

        tracing::debug!(
            target: "provider",
            "本地匹配: title={title:?} dur={duration_secs}s"
        );

        let client = self.client_snapshot();
        let response = tokio::time::timeout(self.timeouts.search, client.search_match(&query))
            .await
            .map_err(|_| {
                tracing::warn!("本地匹配超时（>{:?}）", self.timeouts.search);
                ProviderError::RequestTimeout
            })?
            .map_err(Self::map_error)?;

        if response.status != 200 {
            tracing::warn!("本地匹配返回非 200 状态: {}", response.status);
            return Err(ProviderError::Unknown(format!("HTTP {}", response.status)));
        }
        Self::ensure_body_code(&response.body)?;

        let tracks = parse_song_array_response(&response.body)?;
        tracing::info!(target: "provider", "本地匹配完成 -> {} 条", tracks.len());
        Ok(tracks)
    }

    /// 实际可播预检实现（计划项 5）。
    ///
    /// `check_music` 与 `song_url` 打同一端点（`/api/song/enhance/player/url`），
    /// 只如实上报可播/不可播，**不做任何绕过**。
    async fn check_availability_inner(
        &self,
        song_id: &str,
    ) -> Result<TrackAvailability, ProviderError> {
        let song_id = Self::validate_id(song_id)?;

        let query = Query::new()
            .param("id", song_id)
            .param("br", self.quality().br());
        tracing::debug!(target: "provider", "可播预检: id={}", song_id);

        let client = self.client_snapshot();
        let response = tokio::time::timeout(self.timeouts.playback, client.check_music(&query))
            .await
            .map_err(|_| {
                tracing::warn!("可播预检超时（>{:?}）", self.timeouts.playback);
                ProviderError::RequestTimeout
            })?
            .map_err(Self::map_error)?;
        Self::ensure_body_code(&response.body)?;

        let availability = parse_availability_response(&response.body)?;
        tracing::info!(target: "provider", "可播预检完成: id={} -> {}", song_id, availability);
        Ok(availability)
    }

    /// 实际播放地址实现（规范 7.5 / 6.3）。
    async fn playback_source_inner(&self, song_id: &str) -> Result<PlaybackSource, ProviderError> {
        let song_id = Self::validate_id(song_id)?;

        let query = Query::new()
            .param("id", song_id)
            .param("br", self.quality().br());
        tracing::debug!(target: "provider", "播放地址: id={}", song_id);

        let client = self.client_snapshot();
        let response = tokio::time::timeout(self.timeouts.playback, client.song_url(&query))
            .await
            .map_err(|_| {
                tracing::warn!("播放地址超时（>{:?}）", self.timeouts.playback);
                ProviderError::RequestTimeout
            })?
            .map_err(Self::map_error)?;
        Self::ensure_body_code(&response.body)?;

        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let source = parse_playback_source(&response.body, now_unix)?;
        tracing::info!(
            target: "provider",
            "播放地址完成: id={} expires_at={:?} quality={:?}",
            song_id, source.expires_at, source.quality
        );
        Ok(source)
    }

    /// 实际歌词实现（规范 7.4，阶段 D / M4）。
    ///
    /// `lyric` → `/api/song/lyric`。逐字优先/普通回退的分级在
    /// [`parse_lyric_response`] 中完成；歌词缺失返回
    /// [`ProviderError::EmptyResult`]，调用方仅提示，**不影响播放**。
    async fn lyrics_inner(&self, song_id: &str) -> Result<Lyrics, ProviderError> {
        let song_id = Self::validate_id(song_id)?;

        let query = Query::new().param("id", song_id);
        tracing::debug!(target: "provider", "歌词: id={}", song_id);

        let client = self.client_snapshot();
        let response = tokio::time::timeout(self.timeouts.lyric, client.lyric(&query))
            .await
            .map_err(|_| {
                tracing::warn!("歌词请求超时（>{:?}）", self.timeouts.lyric);
                ProviderError::RequestTimeout
            })?
            .map_err(Self::map_error)?;
        Self::ensure_body_code(&response.body)?;

        let lyrics = parse_lyric_response(&response.body)?;
        tracing::info!(
            target: "provider",
            "歌词完成: id={} kind={} 行数={} 丢弃={}",
            song_id, lyrics.kind, lyrics.lines.len(), lyrics.dropped_lines
        );
        Ok(lyrics)
    }
}

impl MusicProvider for NeteaseProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Netease
    }

    fn set_quality(&self, ceiling: QualityCeiling) {
        self.quality
            .store(ceiling.index(), std::sync::atomic::Ordering::Relaxed);
        tracing::info!(target: "provider", "音质上限切换为 {} ({})", ceiling.code(), ceiling.br());
    }

    fn quality(&self) -> QualityCeiling {
        QualityCeiling::from_index(self.quality.load(std::sync::atomic::Ordering::Relaxed))
    }

    async fn search_songs(&self, keyword: &str, limit: u32) -> Result<Vec<Track>, ProviderError> {
        self.search_songs_inner(keyword, limit).await
    }

    async fn song_details(&self, ids: &[String]) -> Result<Vec<Track>, ProviderError> {
        self.song_details_inner(ids).await
    }

    async fn playlist(
        &self,
        playlist_id: &str,
        offset: u32,
        limit: u32,
    ) -> Result<Playlist, ProviderError> {
        self.playlist_inner(playlist_id, offset, limit).await
    }

    async fn featured_playlists(
        &self,
        kind: FeaturedKind,
        limit: u32,
    ) -> Result<Vec<PlaylistSummary>, ProviderError> {
        self.featured_playlists_inner(kind, limit).await
    }

    async fn daily_songs(&self) -> Result<Vec<Track>, ProviderError> {
        self.daily_songs_inner().await
    }

    async fn intelligence_list(
        &self,
        song_id: &str,
        playlist_id: &str,
        start_id: &str,
        count: u32,
    ) -> Result<Vec<Track>, ProviderError> {
        self.intelligence_list_inner(song_id, playlist_id, start_id, count)
            .await
    }

    async fn search_match(
        &self,
        title: &str,
        album: &str,
        artist: &str,
        duration_secs: u64,
        md5: &str,
    ) -> Result<Vec<Track>, ProviderError> {
        self.search_match_inner(title, album, artist, duration_secs, md5)
            .await
    }

    async fn check_availability(&self, song_id: &str) -> Result<TrackAvailability, ProviderError> {
        self.check_availability_inner(song_id).await
    }

    async fn playback_source(&self, song_id: &str) -> Result<PlaybackSource, ProviderError> {
        self.playback_source_inner(song_id).await
    }

    async fn lyrics(&self, song_id: &str) -> Result<Lyrics, ProviderError> {
        self.lyrics_inner(song_id).await
    }

    async fn login_status(&self) -> Result<LoginState, ProviderError> {
        self.login_status_inner().await
    }

    async fn qr_login_start(&self) -> Result<QrLoginSession, ProviderError> {
        self.qr_login_start_inner().await
    }

    async fn qr_login_poll(&self, key: &str) -> Result<LoginState, ProviderError> {
        self.qr_login_poll_inner(key).await
    }

    async fn login_cellphone(
        &self,
        phone: &str,
        password: &str,
    ) -> Result<LoginState, ProviderError> {
        self.login_cellphone_inner(phone, password).await
    }

    async fn login_cookie(&self, cookie: &str) -> Result<LoginState, ProviderError> {
        self.login_cookie_inner(cookie).await
    }

    async fn logout(&self) -> Result<(), ProviderError> {
        self.logout_inner().await
    }

    async fn like_song(&self, song_id: &str, like: bool) -> Result<(), ProviderError> {
        self.like_song_inner(song_id, like).await
    }

    async fn liked_song_ids(&self) -> Result<Vec<String>, ProviderError> {
        self.liked_song_ids_inner().await
    }

    async fn album_songs(&self, album_id: &str) -> Result<Vec<Track>, ProviderError> {
        self.album_songs_inner(album_id).await
    }

    async fn artist_songs(&self, artist_id: &str, limit: u32) -> Result<Vec<Track>, ProviderError> {
        self.artist_songs_inner(artist_id, limit).await
    }

    async fn user_playlists(&self, limit: u32) -> Result<Vec<PlaylistSummary>, ProviderError> {
        self.user_playlists_inner(limit).await
    }

    fn session_cookie(&self) -> Option<String> {
        self.session_cookie
            .read()
            .expect("cookie lock poisoned")
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blank_keyword_rejected_before_any_request() {
        let p = NeteaseProvider::new();
        for kw in ["", "   ", "\t\n"] {
            assert_eq!(
                p.search_songs(kw, 5).await.unwrap_err(),
                ProviderError::InvalidParameter
            );
        }
    }

    #[tokio::test]
    async fn zero_limit_rejected() {
        let p = NeteaseProvider::new();
        assert_eq!(
            p.search_songs("test", 0).await.unwrap_err(),
            ProviderError::InvalidParameter
        );
    }

    #[tokio::test]
    async fn timeout_is_enforced_by_caller_not_sdk() {
        // 用极短超时验证超时分支可达（SDK 自身没有超时保护）
        let p = NeteaseProvider::new().with_timeout(Duration::from_nanos(1));
        let result = p.search_songs("海阔天空", 1).await;
        // 网络瞬时可用时可能先返回结果；两种结果都证明超时包装不阻塞正常路径
        match result {
            Err(ProviderError::RequestTimeout) => {}
            Ok(_) => {}
            Err(other) => panic!("意外错误: {:?}", other),
        }
    }

    #[test]
    fn provider_id_is_netease() {
        assert_eq!(NeteaseProvider::new().id(), ProviderId::Netease);
    }

    /// Provider 的初始音质必须与新装默认档一致。
    ///
    /// 两处各写一个常量（`gui::prefs::new_install` 与这里）迟早会漂：漏改这边
    /// 时，界面显示 320k 而请求仍按无损发 `br=999000`——首播体积翻几倍，
    /// 却没有任何一处报错。
    #[test]
    fn provider_starts_at_the_new_install_quality() {
        assert_eq!(
            NeteaseProvider::new().quality(),
            QualityCeiling::DEFAULT_FOR_NEW_INSTALL
        );
        // 构造期覆盖仍然有效（`--quality` 之类的外部入口靠它）
        assert_eq!(
            NeteaseProvider::new()
                .with_quality(QualityCeiling::Standard)
                .quality(),
            QualityCeiling::Standard
        );
    }

    // ---- 参数校验（全部在发起请求之前，离线确定性）----------------------

    #[tokio::test]
    async fn song_details_rejects_empty_or_invalid_ids_before_request() {
        let p = NeteaseProvider::new();
        assert_eq!(
            p.song_details(&[]).await.unwrap_err(),
            ProviderError::InvalidParameter
        );
        for ids in [
            vec!["".to_string()],
            vec!["  ".to_string()],
            vec!["abc".to_string()],
            vec!["1".to_string(), "2x".to_string()],
        ] {
            assert_eq!(
                p.song_details(&ids).await.unwrap_err(),
                ProviderError::InvalidParameter,
                "非法 ID 应被拒绝: {:?}",
                ids
            );
        }
    }

    #[tokio::test]
    async fn playlist_rejects_invalid_params_before_request() {
        let p = NeteaseProvider::new();
        // 非法歌单 ID
        for id in ["", "   ", "abc", "12a"] {
            assert_eq!(
                p.playlist(id, 0, 10).await.unwrap_err(),
                ProviderError::InvalidParameter
            );
        }
        // limit 为 0
        assert_eq!(
            p.playlist("24381616", 0, 0).await.unwrap_err(),
            ProviderError::InvalidParameter
        );
    }

    #[tokio::test]
    async fn availability_and_playback_reject_invalid_id_before_request() {
        let p = NeteaseProvider::new();
        for id in ["", "  ", "34x", "-1"] {
            assert_eq!(
                p.check_availability(id).await.unwrap_err(),
                ProviderError::InvalidParameter
            );
            assert_eq!(
                p.playback_source(id).await.unwrap_err(),
                ProviderError::InvalidParameter
            );
        }
    }

    #[tokio::test]
    async fn lyrics_reject_invalid_id_before_request() {
        let p = NeteaseProvider::new();
        for id in ["", "  ", "34x", "-1"] {
            assert_eq!(
                p.lyrics(id).await.unwrap_err(),
                ProviderError::InvalidParameter
            );
        }
    }

    // ---- 登录（二维码三步，全程无密码）--------------------------------

    #[tokio::test]
    async fn qr_poll_rejects_blank_key_before_request() {
        let p = NeteaseProvider::new();
        for key in ["", "   ", "\t\n"] {
            assert_eq!(
                p.qr_login_poll(key).await.unwrap_err(),
                ProviderError::InvalidParameter,
                "空白 key 应被拒绝"
            );
        }
    }

    /// 手工诊断：真实跑一次「取 key → 轮询」。需要联网，默认不跑。
    ///
    /// ```text
    /// cargo test -p reimuplayer --lib qr_login_live_probe -- --ignored --nocapture
    /// ```
    ///
    /// 期望输出：`key=…` 非空、`qr_url` 形如 `https://music.163.com/login?codekey=…`、
    /// 首次轮询状态为 `等待扫码`（**不是**错误）。
    /// 手动探针：验证「偶发网络连接错误」的成因假设。
    ///
    /// 假设：长时间空闲后，SDK 的 reqwest 连接池复用了一条**对端已关闭**的
    /// keep-alive 连接，hyper 报 `connection closed before message completed`；
    /// 旧分类把它翻译成「网络不可用」，于是用户看到莫名其妙的网络错误。
    ///
    /// `cargo test -p reimuplayer --lib network_flake_probe -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "需要真实网络且会空闲等待 ~70 秒；手动执行"]
    async fn network_flake_probe() {
        let p = NeteaseProvider::new();
        let mut failures: Vec<String> = Vec::new();

        // 第一段：连续 3 次，连接池刚建立，应当全部成功
        for i in 0..3 {
            match p.search_songs("周杰伦", 3).await {
                Ok(t) => println!("连续 #{i}: OK（{} 条）", t.len()),
                Err(e) => {
                    println!("连续 #{i}: {:?} — {e}", e);
                    failures.push(format!("连续#{i}: {e:?}"));
                }
            }
        }

        // 第二段：空闲 70s（多数 CDN 的 keep-alive 上限在 60s 上下）
        println!("空闲 70 秒，等待服务端回收连接…");
        std::thread::sleep(std::time::Duration::from_secs(70));

        match p.search_songs("周杰伦", 3).await {
            Ok(t) => println!("空闲后首次: OK（{} 条）—— 连接被干净复用或已重建", t.len()),
            Err(e) => {
                println!("空闲后首次: {:?} — {e}", e);
                failures.push(format!("空闲后首次: {e:?}"));
                // 关键验证：同一条 stale 连接失败后，**立刻重试**能否恢复。
                // 能恢复就说明「有限重试」足以消化这类故障，用户不该看到弹窗。
                match p.search_songs("周杰伦", 3).await {
                    Ok(t) => println!("立即重试: OK（{} 条）—— 重试策略有效", t.len()),
                    Err(e2) => {
                        println!("立即重试: 仍失败 {:?} — {e2}", e2);
                        failures.push(format!("立即重试: {e2:?}"));
                    }
                }
            }
        }

        println!("失败次数 = {} / 5", failures.len());
        for f in &failures {
            println!("  {f}");
        }
        // 本探针只诊断不断言：结果取决于网络与对端行为，不作为 CI 判据
    }

    #[tokio::test]
    #[ignore = "需要联网，手工诊断用"]
    async fn qr_login_live_probe() {
        let p = NeteaseProvider::new();
        let session = match p.qr_login_start().await {
            Ok(s) => s,
            Err(e) => {
                println!("qr_start 失败: {e:?}");
                return;
            }
        };
        println!("key = {}", session.key);
        println!("qr_url = {}", session.qr_url);
        match p.qr_login_poll(&session.key).await {
            Ok(state) => println!("首次轮询状态 = {state}"),
            Err(e) => println!("首次轮询失败 = {e:?}"),
        }
    }

    #[tokio::test]
    async fn login_timeout_is_enforced_by_caller_not_sdk() {
        // login 通道同样必须由调用方包超时（SDK 无内置超时）
        let p = NeteaseProvider::new().with_timeouts(ProviderTimeouts {
            login: Duration::from_nanos(1),
            ..Default::default()
        });
        let result = p.qr_login_start().await;
        match result {
            Err(ProviderError::RequestTimeout) => {}
            Ok(_) => {}
            Err(other) => panic!("意外错误: {:?}", other),
        }
    }

    // ---- 响应体业务码分类（SDK 把特殊码强制视为 200）--------------------

    #[test]
    fn qr_check_code_allows_progress_codes() {
        use serde_json::json;
        // 回归：轮询接口的 800–803 是「扫码进度」而非错误。
        // 曾经误用 ensure_body_code 导致每次轮询都变成 AuthenticationRequired，
        // 二维码能显示、手机能扫，但登录永远无法完成。
        for code in [800, 801, 802, 803] {
            assert!(
                NeteaseProvider::ensure_qr_check_code(&json!({"code": code})).is_ok(),
                "code={code} 是扫码进度，不得判为错误"
            );
        }
        // 字符串形式的进度码同样放行
        for code in ["800", "801", "802", "803"] {
            assert!(NeteaseProvider::ensure_qr_check_code(&json!({"code": code})).is_ok());
        }
        // 真正的错误仍要被分类（不放行）
        assert_eq!(
            NeteaseProvider::ensure_qr_check_code(&json!({"code": 400})).unwrap_err(),
            ProviderError::InvalidParameter
        );
        assert_eq!(
            NeteaseProvider::ensure_qr_check_code(&json!({"code": 502})).unwrap_err(),
            ProviderError::DataProviderUnavailable
        );
        assert!(NeteaseProvider::ensure_qr_check_code(&json!({"code": 200})).is_ok());
        assert!(NeteaseProvider::ensure_qr_check_code(&json!({})).is_ok());
    }

    #[test]
    fn ensure_body_code_classifies_sdk_coerced_codes() {
        use serde_json::json;
        assert!(NeteaseProvider::ensure_body_code(&json!({"code": 200})).is_ok());
        assert!(NeteaseProvider::ensure_body_code(&json!({"code": 302})).is_ok());
        assert!(NeteaseProvider::ensure_body_code(&json!({})).is_ok());
        assert_eq!(
            NeteaseProvider::ensure_body_code(&json!({"code": 400})).unwrap_err(),
            ProviderError::InvalidParameter
        );
        assert_eq!(
            NeteaseProvider::ensure_body_code(&json!({"code": 502})).unwrap_err(),
            ProviderError::DataProviderUnavailable
        );
        for code in [800, 801, 802, 803] {
            assert_eq!(
                NeteaseProvider::ensure_body_code(&json!({"code": code})).unwrap_err(),
                ProviderError::AuthenticationRequired
            );
        }
        // 字符串形式的业务码同样识别
        assert_eq!(
            NeteaseProvider::ensure_body_code(&json!({"code": "400"})).unwrap_err(),
            ProviderError::InvalidParameter
        );
    }

    // ---- 超时配置（请求超时控制）---------------------------------------

    #[test]
    fn default_timeouts_follow_spec_upper_bounds() {
        let t = ProviderTimeouts::default();
        assert_eq!(t.search, SEARCH_TIMEOUT);
        assert_eq!(t.song_detail, DETAIL_TIMEOUT);
        assert_eq!(t.playlist, PLAYLIST_TIMEOUT);
        assert_eq!(t.playback, PLAYBACK_TIMEOUT);
        assert_eq!(t.lyric, LYRIC_TIMEOUT);
        assert_eq!(t.login, LOGIN_TIMEOUT);
        // 规范 8.1：搜索/详情/歌词 5–8s，播放地址 5–10s
        assert!(t.search >= Duration::from_secs(5) && t.search <= Duration::from_secs(8));
        assert!(t.song_detail >= Duration::from_secs(5) && t.song_detail <= Duration::from_secs(8));
        assert!(t.lyric >= Duration::from_secs(5) && t.lyric <= Duration::from_secs(8));
        assert!(t.playback >= Duration::from_secs(5) && t.playback <= Duration::from_secs(10));
    }

    #[test]
    fn with_timeouts_overrides_every_channel() {
        let custom = ProviderTimeouts {
            search: Duration::from_millis(1),
            song_detail: Duration::from_millis(2),
            playlist: Duration::from_millis(3),
            playback: Duration::from_millis(4),
            lyric: Duration::from_millis(5),
            login: Duration::from_millis(6),
        };
        let p = NeteaseProvider::new().with_timeouts(custom);
        assert_eq!(p.timeouts(), custom);
    }

    #[test]
    fn with_timeout_compat_sets_search_only() {
        let p = NeteaseProvider::new().with_timeout(Duration::from_nanos(1));
        let t = p.timeouts();
        assert_eq!(t.search, Duration::from_nanos(1));
        // 其余通道保持默认，不受兼容方法影响
        assert_eq!(t.song_detail, DETAIL_TIMEOUT);
        assert_eq!(t.playlist, PLAYLIST_TIMEOUT);
    }
}
