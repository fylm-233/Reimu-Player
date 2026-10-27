//! 在线音乐 Provider 抽象（阶段 B）。
//!
//! 架构约束（`NETEASE_INTEGRATION_SPEC.md` 3.2）：
//! - UI 不得直接调用第三方 API，也不得接触第三方原始 JSON。
//! - 所有网络错误必须转换为项目内部错误类型（[`ProviderError`]）。
//! - 更换 Provider 不得要求重写 UI。
//!
//! 实现方式说明：使用 `async fn in trait`（Rust 1.75+ 稳定），
//! 配合泛型绑定而非 `dyn`，避免不必要的动态分发与 `async-trait` 依赖
//! （规范 6.1 明确允许）。
//!
//! 运行时替换（阶段 E）：泛型 trait（RPITIT）不是对象安全的，UI 层
//! 无法直接持有 `dyn MusicProvider`。为此在**不改动原 trait** 的前提下
//! 增加对象安全视图 [`DynMusicProvider`]（BoxFuture 装箱）+ 全称
//! blanket impl——任何 `MusicProvider` 实现自动获得 dyn 能力，UI 持有
//! [`std::sync::Arc<dyn DynMusicProvider>`] 即可运行时替换 Provider，
//! 满足「接口统一、可替换」且**更换 Provider 不重写 UI**（规范 3.2）。

use std::fmt;

use serde_json::Value;

use super::lyrics::{parse_lrc, parse_yrc, Lyrics};
use crate::i18n::{pick, Lang};

/// Provider 标识。歌曲 ID 必须与 Provider 绑定（规范 6.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderId {
    /// 本地文件
    Local,
    /// 网易云音乐
    Netease,
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderId::Local => write!(f, "local"),
            ProviderId::Netease => write!(f, "netease"),
        }
    }
}

/// 统一歌曲模型（`NETEASE_INTEGRATION_SPEC.md` 6.2）。
///
/// 设计要求（已在结构上落实）：
/// - `id` 与 [`ProviderId`] 绑定，本地路径与在线 ID **不共用语义字段**。
/// - `album` / `duration_ms` / `cover_url` 允许未知，**不伪造**。
/// - `artists` 允许多名，允许为空。
/// - 播放 URL **不是**本模型的字段——它易失效，不作为歌曲标识。
/// - 登录 Cookie **不是**本模型的字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub provider: ProviderId,
    pub id: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    /// 专辑 ID（接口 `al.id`）。点击专辑名跳专辑页要用它；
    /// 本地曲目与接口未给时是 `None`——**不伪造**，UI 就不把该行做成可点。
    pub album_id: Option<String>,
    /// 艺人 ID（接口 `ar[].id`），与 [`Self::artists`] **按下标一一对应**。
    ///
    /// 长度恒等于 `artists.len()`；某一位接口没给 ID 时该位是空字符串
    /// （UI 判空即视为不可跳转）。用空串占位而不是直接丢掉，是因为一旦丢掉，
    /// 后面所有人的 ID 都会错位到前一个人身上——宁可空，不可错。
    pub artist_ids: Vec<String>,
    pub duration_ms: Option<u64>,
    pub cover_url: Option<String>,
}

impl Default for Track {
    /// 占位用的空曲目（本地来源、字段全空）。
    ///
    /// 存在的唯一理由是让测试与批量构造能写 `..Track::default()`——新增字段
    /// （如 `album_id` / `artist_ids`）时不用改几十处字面量。**业务代码不得**
    /// 用它当"未知曲目"，那会让空 ID 流进播放队列。
    fn default() -> Self {
        Self {
            provider: ProviderId::Local,
            id: String::new(),
            title: String::new(),
            artists: Vec::new(),
            album: None,
            album_id: None,
            artist_ids: Vec::new(),
            duration_ms: None,
            cover_url: None,
        }
    }
}

impl Track {
    /// 展示用「艺人 - 歌名」。艺人未知时只显示歌名，不伪造占位符。
    pub fn display_line(&self) -> String {
        if self.artists.is_empty() {
            self.title.clone()
        } else {
            format!("{} - {}", self.artists.join("/"), self.title)
        }
    }

    /// 展示用时长 `mm:ss`（超过 1 小时 `h:mm:ss`），数字等宽由 UI 层保证。
    ///
    /// 时长未知返回 `"--:--"` 占位（不伪造 00:00）；纯展示用途，不回写模型。
    pub fn duration_text(&self) -> String {
        let Some(ms) = self.duration_ms else {
            return "--:--".to_string();
        };
        let total_secs = ms / 1000;
        let h = total_secs / 3600;
        let m = (total_secs % 3600) / 60;
        let s = total_secs % 60;
        if h > 0 {
            format!("{}:{:02}:{:02}", h, m, s)
        } else {
            format!("{:02}:{:02}", m, s)
        }
    }
}

/// 音质等级（`NETEASE_INTEGRATION_SPEC.md` 6.3 的 `AudioQuality`）。
///
/// 只反映接口 `level` 字段的真实取值：已知取值映射为对应变体，
/// **未知取值原样保留**在 [`AudioQuality::Other`] 中，不擅自归类、不伪造。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioQuality {
    /// `standard`：标准
    Standard,
    /// `higher`：较高
    Higher,
    /// `exhigh`：极高
    ExtraHigh,
    /// `lossless`：无损
    Lossless,
    /// `hires`：高解析度
    HiRes,
    /// 其他取值（如 `dolby` / `jymaster` / `sky` 等），保留接口原值
    Other(String),
}

impl AudioQuality {
    /// 从接口 `level` 字段解析。
    pub fn from_api_level(level: &str) -> Self {
        match level {
            "standard" => AudioQuality::Standard,
            "higher" => AudioQuality::Higher,
            "exhigh" => AudioQuality::ExtraHigh,
            "lossless" => AudioQuality::Lossless,
            "hires" => AudioQuality::HiRes,
            other => AudioQuality::Other(other.to_string()),
        }
    }
}

impl fmt::Display for AudioQuality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioQuality::Standard => write!(f, "standard"),
            AudioQuality::Higher => write!(f, "higher"),
            AudioQuality::ExtraHigh => write!(f, "exhigh"),
            AudioQuality::Lossless => write!(f, "lossless"),
            AudioQuality::HiRes => write!(f, "hires"),
            AudioQuality::Other(s) => write!(f, "{}", s),
        }
    }
}

/// 播放音质**上限**（请求侧偏好）。
///
/// 与 [`AudioQuality`] 的区别（两者不能合并）：
/// - [`AudioQuality`] 是**响应侧**事实——"接口说它给了哪一档"，带 `Other(String)`
///   兜底未知取值，不可 `Copy`，也无法穷举；
/// - 本枚举是**请求侧**意愿——"我希望不超过哪一档"，四档穷举、`Copy`、
///   可直接落到 `song/url` 的 `br` 参数。
///
/// 语义是**上限而非强制**：接口按当前身份返回「可用且不高于该档」的最佳结果。
/// 因此选 [`QualityCeiling::Lossless`] 并不等于绕过会员限制——无权限时接口
/// 自己会降级或返回不可播，本枚举不改变这一点（规范硬约束）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum QualityCeiling {
    /// 标准（128 kbps）。
    Standard,
    /// 较高（192 kbps）。
    Higher,
    /// 极高（320 kbps）。
    ExtraHigh,
    /// 无损（≤ 999 kbps）。默认值与阶段 F 的固定取值一致，避免行为回退。
    #[default]
    Lossless,
}

impl QualityCeiling {
    /// **新装**默认档（320 kbps）。
    ///
    /// 刻意与 [`Default`] 分开：`Default` 的语义是"不知道用户想要什么时的保守
    /// 回退"（配置损坏 / 未知 code / 越界 index），那个必须保持 [`Self::Lossless`]
    /// ——否则一份坏掉的 `prefs.json` 会把老用户从无损静默降级。
    /// 本常量只用于"磁盘上压根没有偏好文件"这一种情形（见 `gui::prefs::load_from`）。
    ///
    /// 为什么不默认无损：无损单曲动辄 30–60 MB，首播要下的字节是无损的好几倍，
    /// 而多数设备/耳朵在 320k 上分辨不出差别。用户想要无损，面板上一键就能调上去。
    pub const DEFAULT_FOR_NEW_INSTALL: Self = Self::ExtraHigh;

    /// 全部档位，由低到高（设置面板按此顺序排按钮）。
    pub const ALL: [QualityCeiling; 4] = [
        QualityCeiling::Standard,
        QualityCeiling::Higher,
        QualityCeiling::ExtraHigh,
        QualityCeiling::Lossless,
    ];

    /// 请求参数 `br` 的取值。
    pub fn br(self) -> &'static str {
        match self {
            QualityCeiling::Standard => "128000",
            QualityCeiling::Higher => "192000",
            QualityCeiling::ExtraHigh => "320000",
            QualityCeiling::Lossless => "999000",
        }
    }

    /// 持久化键（`prefs.json` 的 `quality` 字段）。
    ///
    /// ⚠ 与 [`Self::label`] 分开：这是**数据格式**，不随界面语言变。
    /// 早先 `prefs.rs` 直接拿 `label()` 当键，一旦 label 双语化就会
    /// 把用户偏好读坏——所以键必须是稳定英文常量。
    pub fn code(self) -> &'static str {
        match self {
            QualityCeiling::Standard => "STANDARD",
            QualityCeiling::Higher => "HIGHER",
            QualityCeiling::ExtraHigh => "EX-HIGH",
            QualityCeiling::Lossless => "LOSSLESS",
        }
    }

    /// 面板显示名（跟界面语言；英文侧保持 System 6 全大写短标签风格）。
    pub fn label(self, lang: crate::i18n::Lang) -> &'static str {
        use crate::i18n::pick;
        match self {
            QualityCeiling::Standard => pick(lang, "标准", "STANDARD"),
            QualityCeiling::Higher => pick(lang, "较高", "HIGHER"),
            QualityCeiling::ExtraHigh => pick(lang, "极高", "EX-HIGH"),
            QualityCeiling::Lossless => pick(lang, "无损", "LOSSLESS"),
        }
    }

    /// 面板说明用的一句话（含码率）。
    pub fn detail(self) -> &'static str {
        match self {
            QualityCeiling::Standard => "128 kbps",
            QualityCeiling::Higher => "192 kbps",
            QualityCeiling::ExtraHigh => "320 kbps",
            QualityCeiling::Lossless => "FLAC / <=999 kbps",
        }
    }

    /// 序号（`AtomicU8` 存储用）。
    pub fn index(self) -> u8 {
        match self {
            QualityCeiling::Standard => 0,
            QualityCeiling::Higher => 1,
            QualityCeiling::ExtraHigh => 2,
            QualityCeiling::Lossless => 3,
        }
    }

    /// 由序号还原；越界回落到默认档（不 panic——原子量可能被外部写脏）。
    pub fn from_index(index: u8) -> Self {
        Self::ALL.get(index as usize).copied().unwrap_or_default()
    }
}

/// 播放地址模型（`NETEASE_INTEGRATION_SPEC.md` 6.3）。
///
/// - `expires_at` **仅**在接口确实提供有效期时填写，未知不伪造。
/// - 播放 URL 只用于播放：不作歌曲标识、不默认持久缓存。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackSource {
    pub url: String,
    /// 过期时间（Unix 秒）。仅由接口 `expi`（有效期秒数）推导，无则 `None`。
    pub expires_at: Option<i64>,
    /// 音质等级。仅取接口 `level` 字段，未知不伪造。
    pub quality: Option<AudioQuality>,
    /// 内容类型标记。取接口 `type` 字段原值（如 `mp3` / `flac`），不改写为 MIME。
    pub content_type: Option<String>,
    /// 接口声明的音频码率（kbps）。取响应 `br`（单位 bps）÷ 1000，缺失或过低则 `None`。
    ///
    /// 用途只有一个，但很关键：边下边播要知道「10 秒音频是多少字节」才能定起播
    /// 闸门（见 [`crate::music::stream_buffer::start_gate`]）。码率的可信来源只有
    /// 两个——接口这个字段，和「Content-Length ÷ 时长」；**不能**从来下载一半的
    /// 文件推：lofty 对不完整文件给出的 `bitrate_kbps` 是拿磁盘实时长度算的，
    /// 实测为错值（`docs/NETEASE_PHASE3_PLAN.md` 方案选型 ②b）。
    pub bitrate_kbps: Option<u32>,
}

/// 歌单模型（`NETEASE_INTEGRATION_SPEC.md` 7.3）。
///
/// `tracks` 只含**当前分页**的曲目；单条畸形或缺失的曲目被容忍跳过。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Playlist {
    pub provider: ProviderId,
    pub id: String,
    pub name: Option<String>,
    /// 曲目总数（取接口 `trackCount`，缺失时回退完整 ID 列表长度）。
    pub total: Option<u64>,
    /// 当前分页窗口内的曲目。
    pub tracks: Vec<Track>,
}

/// 歌单分页窗口（解析中间产物）：元信息 + 当前页曲目 ID。
///
/// 解析层只负责按 `offset`/`limit` 切出窗口，不发网络请求；
/// 窗口内曲目的详细信息由 Provider 实现二次获取。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistWindow {
    pub name: Option<String>,
    pub total: Option<u64>,
    /// 已按 `offset`/`limit` 切片的曲目 ID；越界时为空（空页不是错误）。
    pub track_ids: Vec<String>,
}

/// 精选列表类型（推荐 / 排行榜，规范 7.6 接口族）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeaturedKind {
    /// 推荐歌单（网易云 `/personalized`，**无需登录**）。
    Recommended,
    /// 官方排行榜（网易云 `/toplist`）。
    Toplists,
    /// 每日推荐歌单（网易云 `/recommend/resource`，**需要登录**）。
    ///
    /// 与 [`FeaturedKind::Recommended`] 的区别：前者是"猜你喜欢"的公共推荐，
    /// 后者是账号维度的每日 30 张，未登录时接口返回空 → 如实上抛
    /// [`ProviderError::LoginRequired`]，由 UI 引导登录，**不伪造数据**。
    Daily,
}

/// 歌单摘要（精选列表条目）：只含列表展示所需元信息，不含曲目。
///
/// 与 [`Playlist`] 的区别：摘要用于「发现」入口（推荐卡 / 榜单行），
/// 点击后才按 `id` 走 [`MusicProvider::playlist`] 分页取曲目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistSummary {
    pub provider: ProviderId,
    pub id: String,
    pub name: String,
    /// 封面图 URL（缺失为 `None`，不伪造）。
    pub cover_url: Option<String>,
    /// 曲目总数（缺失为 `None`）。
    pub track_count: Option<u64>,
    /// 播放量（缺失为 `None`）。
    pub play_count: Option<u64>,
    /// 一句描述（推荐语 / 榜单更新频率等），缺失为 `None`。
    pub description: Option<String>,
}

/// 曲目可播状态（计划项 5：明确可播/不可播，**不做绕过**）。
///
/// [`TrackAvailability::Unavailable`] 携带可选线索（接口给出的消息或业务码）；
/// 接口没给线索时为 `None`，**不编造**具体原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackAvailability {
    /// 可播放
    Available,
    /// 不可播放（VIP / 版权下架 / 地区限制等），附可选线索
    Unavailable(Option<String>),
}

impl TrackAvailability {
    pub fn is_available(&self) -> bool {
        matches!(self, TrackAvailability::Available)
    }
}

impl fmt::Display for TrackAvailability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrackAvailability::Available => write!(f, "可播放"),
            TrackAvailability::Unavailable(None) => write!(f, "不可播放"),
            TrackAvailability::Unavailable(Some(reason)) => write!(f, "不可播放（{}）", reason),
        }
    }
}

/// 登录状态（会话维度）。
///
/// 匿名态即可使用公开接口（搜索 / 详情 / 歌单 / 公共歌单）；
/// 登录只解锁用户自有数据（红心歌单、用户歌单、VIP 音质等自身权限），
/// **不构成**对会员 / 付费 / 地区限制的绕过。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginState {
    /// 未登录（匿名使用）。
    Anonymous,
    /// 二维码登录进行中：等待扫码。
    QrWaiting,
    /// 二维码已被扫描，等待手机端确认。
    QrScanned,
    /// 二维码已过期（key 失效），需重新发起。
    QrExpired,
    /// 已登录。
    LoggedIn {
        nickname: Option<String>,
        user_id: Option<String>,
    },
}

impl fmt::Display for LoginState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoginState::Anonymous => write!(f, "未登录"),
            LoginState::QrWaiting => write!(f, "等待扫码"),
            LoginState::QrScanned => write!(f, "已扫码，待确认"),
            LoginState::QrExpired => write!(f, "二维码已过期"),
            LoginState::LoggedIn { nickname, .. } => match nickname {
                Some(n) => write!(f, "已登录：{}", n),
                None => write!(f, "已登录"),
            },
        }
    }
}

impl LoginState {
    /// 界面文案（跟界面语言）。
    ///
    /// [`fmt::Display`] 保持中文稳定输出（日志 / tracing 需要可 grep），
    /// UI 侧一律走这里——同 [`ProviderError::message`] 的分层纪律。
    pub fn message(&self, lang: Lang) -> String {
        match self {
            LoginState::Anonymous => pick(lang, "未登录", "Signed out").to_string(),
            LoginState::QrWaiting => pick(lang, "等待扫码", "Waiting for scan").to_string(),
            LoginState::QrScanned => {
                pick(lang, "已扫码，待确认", "Scanned — confirm on phone").to_string()
            }
            LoginState::QrExpired => pick(lang, "二维码已过期", "QR code expired").to_string(),
            LoginState::LoggedIn { nickname, .. } => match nickname {
                Some(n) => format!(
                    "{}{}{}",
                    pick(lang, "已登录", "Signed in"),
                    pick(lang, "：", ": "),
                    n
                ),
                None => pick(lang, "已登录", "Signed in").to_string(),
            },
        }
    }
}

///
/// `key` 是轮询凭据（交给 [`MusicProvider::qr_login_poll`]）；
/// `qr_url` 是二维码内容（UI 自行编码为图形）。两者都不是登录凭证——
/// 真正的会话凭据只在扫码确认后产生，且**不外泄**给 UI（见 [`QrPollOutcome`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrLoginSession {
    /// 轮询凭据。
    pub key: String,
    /// 二维码内容（形如 `https://music.163.com/login?codekey=…`）。
    pub qr_url: String,
}

/// 二维码轮询的解析结果（含会话材料，**仅供 Provider 内部使用**）。
///
/// `cookie` 是登录成功后的会话凭据：只允许喂给 SDK 客户端，
/// **不得**写入日志 / 错误信息 / 展示给 UI。为此本类型自定义
/// [`Debug`](std::fmt::Debug) 实现，对 cookie 字段脱敏（`<redacted>`），
/// 杜绝 `{:?}` 误打日志时泄露凭据（规范 3.2 第 10 条）。
pub struct QrPollOutcome {
    /// 对外可见的状态（不含任何凭据）。
    pub state: LoginState,
    /// 会话 Cookie（仅登录成功时为 `Some`；**不外泄**）。
    pub(crate) cookie: Option<String>,
}

impl QrPollOutcome {
    /// 构造一个纯状态结果（无凭据）。
    pub fn state_only(state: LoginState) -> Self {
        Self {
            state,
            cookie: None,
        }
    }

    /// 构造登录成功结果（携带会话 Cookie）。
    pub fn success(cookie: String, nickname: Option<String>, user_id: Option<String>) -> Self {
        Self {
            state: LoginState::LoggedIn { nickname, user_id },
            cookie: Some(cookie),
        }
    }

    /// 取出会话 Cookie（所有权移交调用方；`None` 表示本次不产生会话）。
    pub fn into_cookie(self) -> Option<String> {
        self.cookie
    }
}

impl fmt::Debug for QrPollOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QrPollOutcome")
            .field("state", &self.state)
            .field("cookie", &self.cookie.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl Clone for QrPollOutcome {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            cookie: self.cookie.clone(),
        }
    }
}

impl PartialEq for QrPollOutcome {
    fn eq(&self, other: &Self) -> bool {
        self.state == other.state && self.cookie.is_some() == other.cookie.is_some()
    }
}

/// 在线服务错误分类（`NETEASE_INTEGRATION_SPEC.md` 8.2 完整集合）。
///
/// UI 只根据该枚举展示简短可读信息，**不显示原始 JSON 或内部堆栈**。
/// `EmptyResult` / `InvalidParameter` 是项目在规范集合之上的补充分类：
/// 前者区分「没有结果」与「解析失败」，后者对应规范 8.3「无效参数不得重试」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// 网络不可用 / DNS 失败 / 连接被拒
    NetworkUnavailable,
    /// 连接已建立但**中途被断开**（服务端回收空闲 keep-alive、连接被重置）。
    ///
    /// 与 [`Self::NetworkUnavailable`] 的区别很关键：这里用户的网络是好的，
    /// 只是复用到了一条对端已关闭的持久连接。属于**瞬时**故障，可安全重试；
    /// 文案不能说成"网络不可用"，否则用户会去排查根本没坏的网络。
    ConnectionReset,
    /// 请求超时
    RequestTimeout,
    /// 触发限流
    RateLimited,
    /// 需要登录
    AuthenticationRequired,
    /// 登录已失效
    AuthenticationExpired,
    /// 内容不可播放（版权 / VIP / 地区限制等），**不重试、不绕过**
    ContentUnavailable,
    /// 未获得播放地址
    PlaybackUrlUnavailable,
    /// 服务返回了不支持的响应（如 HTML 错误页而非 JSON）
    UnsupportedResponse,
    /// 服务返回数据无法解析
    InvalidData,
    /// 空结果（不是错误，但需要区分于「解析失败」）
    EmptyResult,
    /// 请求参数非法
    InvalidParameter,
    /// 在线数据服务不可用
    DataProviderUnavailable,
    /// 用户主动取消
    Cancelled,
    /// 其他，携带可读描述
    Unknown(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::NetworkUnavailable => write!(f, "网络不可用，请检查网络连接"),
            ProviderError::ConnectionReset => write!(f, "连接被中断，请重试"),
            ProviderError::RequestTimeout => write!(f, "请求超时，请稍后重试"),
            ProviderError::RateLimited => write!(f, "请求过于频繁，请稍后重试"),
            ProviderError::AuthenticationRequired => write!(f, "该功能需要登录"),
            ProviderError::AuthenticationExpired => write!(f, "登录已过期，请重新登录"),
            ProviderError::ContentUnavailable => {
                write!(f, "该内容当前不可播放（可能受版权、VIP 或地区限制）")
            }
            ProviderError::PlaybackUrlUnavailable => write!(f, "未获得播放地址，无法播放"),
            ProviderError::UnsupportedResponse => write!(f, "服务返回了不支持的响应格式"),
            ProviderError::InvalidData => write!(f, "服务返回数据无法解析"),
            ProviderError::EmptyResult => write!(f, "没有找到匹配结果"),
            ProviderError::InvalidParameter => write!(f, "请求参数无效"),
            ProviderError::DataProviderUnavailable => write!(f, "在线服务暂不可用"),
            ProviderError::Cancelled => write!(f, "请求已取消"),
            ProviderError::Unknown(s) => write!(f, "未知错误: {}", s),
        }
    }
}

impl std::error::Error for ProviderError {}

impl ProviderError {
    /// 按界面语言渲染人话（与 [`fmt::Display`] 同义，仅英文侧不同）。
    ///
    /// 为什么不用 `Display` 直接出文案：`Display` 会被日志/tracing 使用，
    /// 那些地方需要**稳定、可 grep 的中文**；用户可见的错误框才需要跟语言走。
    pub fn message(&self, lang: crate::i18n::Lang) -> String {
        use crate::i18n::pick;
        match self {
            ProviderError::NetworkUnavailable => pick(
                lang,
                "网络不可用，请检查网络连接",
                "Network unavailable — please check your connection",
            ),
            ProviderError::ConnectionReset => pick(
                lang,
                "连接被中断，请重试",
                "Connection interrupted — please retry",
            ),
            ProviderError::RequestTimeout => pick(
                lang,
                "请求超时，请稍后重试",
                "Request timed out — please retry",
            ),
            ProviderError::RateLimited => pick(
                lang,
                "请求过于频繁，请稍后重试",
                "Too many requests — please wait a moment",
            ),
            ProviderError::AuthenticationRequired => {
                pick(lang, "该功能需要登录", "This feature requires sign-in")
            }
            ProviderError::AuthenticationExpired => pick(
                lang,
                "登录已过期，请重新登录",
                "Sign-in expired — please sign in again",
            ),
            ProviderError::ContentUnavailable => pick(
                lang,
                "该内容当前不可播放（可能受版权、VIP 或地区限制）",
                "This content cannot be played (copyright, VIP or region restriction)",
            ),
            ProviderError::PlaybackUrlUnavailable => pick(
                lang,
                "未获得播放地址，无法播放",
                "No playback URL was provided",
            ),
            ProviderError::UnsupportedResponse => pick(
                lang,
                "服务返回了不支持的响应格式",
                "The service returned an unsupported response",
            ),
            ProviderError::InvalidData => pick(
                lang,
                "服务返回数据无法解析",
                "The service response could not be parsed",
            ),
            ProviderError::EmptyResult => pick(lang, "没有找到匹配结果", "No matching results"),
            ProviderError::InvalidParameter => {
                pick(lang, "请求参数无效", "Invalid request parameter")
            }
            ProviderError::DataProviderUnavailable => pick(
                lang,
                "在线服务暂不可用",
                "The online service is unavailable",
            ),
            ProviderError::Cancelled => pick(lang, "请求已取消", "Request cancelled"),
            ProviderError::Unknown(s) => {
                return crate::i18n::pick(
                    lang,
                    &format!("未知错误: {s}"),
                    &format!("Unknown error: {s}"),
                )
                .to_string();
            }
        }
        .to_string()
    }
}

impl ProviderError {
    /// 该错误是否值得有限次重试（规范 8.3）。
    ///
    /// - 超时 / 网络不可用 / 数据服务不可用：可重试。
    /// - 限流：应遵守服务端重试建议，这里保守地**不**自动重试。
    /// - 需要登录 / 登录失效：要求重新认证，不重试。
    /// - 内容不可播 / 无播放地址（版权、VIP、地区限制）：**不重试、不绕过**。
    /// - 参数非法 / 空结果 / 响应不支持 / 用户取消：不得重试。
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            ProviderError::NetworkUnavailable
                | ProviderError::ConnectionReset
                | ProviderError::RequestTimeout
                | ProviderError::DataProviderUnavailable
        )
    }
}

/// 在线音乐 Provider 契约。
///
/// 实现方**必须**把第三方响应解析为 [`Track`] / [`Playlist`] /
/// [`PlaybackSource`] 等项目模型，不得把 `serde_json::Value` 泄漏给调用方。
pub trait MusicProvider {
    /// Provider 标识。
    fn id(&self) -> ProviderId;

    /// 搜索歌曲（规范 7.1）。
    ///
    /// - `keyword` 为空时应返回 [`ProviderError::InvalidParameter`]，不发起请求。
    /// - 无结果时返回 [`ProviderError::EmptyResult`]，与「解析失败」区分。
    ///
    /// 返回 `impl Future + Send` 而非 `async fn`：网络请求必须能被派发到
    /// 工作线程而不阻塞 UI（`NETEASE_INTEGRATION_SPEC.md` 7.1），
    /// 因此 future 需要显式 `Send` 保证。
    fn search_songs(
        &self,
        keyword: &str,
        limit: u32,
    ) -> impl std::future::Future<Output = Result<Vec<Track>, ProviderError>> + Send;

    /// 按 ID 批量取歌曲详情（规范 7.2）。
    ///
    /// - `ids` 为空或含非法 ID 时返回 [`ProviderError::InvalidParameter`]。
    /// - 批量响应**容忍**缺失/畸形条目（跳过），整批为空 → [`ProviderError::EmptyResult`]。
    fn song_details(
        &self,
        ids: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<Track>, ProviderError>> + Send;

    /// 取歌单的指定分页（规范 7.3）。
    ///
    /// - `limit` 为 0 或 ID 非法时返回 [`ProviderError::InvalidParameter`]。
    /// - `offset` 越界返回空页（不是错误）。
    fn playlist(
        &self,
        playlist_id: &str,
        offset: u32,
        limit: u32,
    ) -> impl std::future::Future<Output = Result<Playlist, ProviderError>> + Send;

    /// 精选列表（规范 7.6：推荐歌单 / 官方排行榜）。
    ///
    /// - `limit` 为 0 时返回 [`ProviderError::InvalidParameter`]，不发起请求。
    /// - 无结果返回 [`ProviderError::EmptyResult`]。
    /// - 返回的是歌单**摘要**（[`PlaylistSummary`]），点开后按 `id` 走
    ///   [`MusicProvider::playlist`] 取曲目。
    fn featured_playlists(
        &self,
        kind: FeaturedKind,
        limit: u32,
    ) -> impl std::future::Future<Output = Result<Vec<PlaylistSummary>, ProviderError>> + Send;

    /// 每日推荐歌曲（网易云 `/api/v3/discovery/recommend/songs`，**需要登录**）。
    ///
    /// 「每日」的语义**完全在服务端**：它按「账号 + 当天」生成一批固定曲目
    /// （通常 30 首），同一天内重复请求结果一致，跨天自动换新。客户端只负责
    /// 展示，**不做本地拼凑、不随机洗牌**——否则换台设备/重开程序看到的
    /// "今日推荐"就不一样了，那就不叫每日推荐。
    ///
    /// 与 [`MusicProvider::featured_playlists`] 的 `FeaturedKind::Daily` 是
    /// **两件不同的事**：
    /// - `FeaturedKind::Daily` = `/recommend/resource`，返回当天推荐的**歌单**
    ///   （30 张歌单的摘要，点开还要再取曲目）；
    /// - 本方法 = `/recommend/songs`，返回当天推荐的**歌曲**本身。
    ///
    /// 契约：
    /// - 未登录 → [`ProviderError::AuthenticationRequired`]（接口业务码 301）。
    ///   **不返回空列表**：空列表会让 UI 显示"今天没有推荐"，与事实不符，
    ///   用户也不会想到要去登录。
    /// - 已登录但当天确实为空 → [`ProviderError::EmptyResult`]。
    /// - 不绕过任何会员 / 版权 / 地区限制，接口给什么就是什么。
    fn daily_songs(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<Track>, ProviderError>> + Send;

    /// 心动模式 / 智能播放列表（网易云 `playmode_intelligence_list`，**需要登录**）。
    ///
    /// 语义与「随机」完全不同，别混用：
    /// - 随机 = 在**当前歌单内**打乱顺序，池子封闭；
    /// - 心动 = 以 `song_id` + `playlist_id` 为种子，让服务端推荐**歌单外**的
    ///   相似曲目，池子开放、可以无限续。
    ///
    /// 参数（对应接口 `id` / `pid` / `sid` / `count`）：
    /// - `song_id`：种子歌曲 id，必须是数字串，否则 [`ProviderError::InvalidParameter`]。
    /// - `playlist_id`：种子所属歌单 id。**网易云只接受账号的「我喜欢的音乐」**
    ///   （`specialType==5`）当上下文：实测普通自建歌单与别人的歌单都回
    ///   `400 不支持该歌单类型`，`0` 回 `400 歌单不存在`。适配层会自动换成红心
    ///   歌单，UI 这里传来的值只在定位不到红心歌单时兜底——所以**照实传**即可，
    ///   不必去猜接口认哪张。
    /// - `start_id`：真正开始播放的那一首；传空串则接口默认取 `song_id`。
    /// - `count`：一次要几首。**必须显式给**：SDK 默认 `count=1`。服务端实测**忽略
    ///   该参数**（一次返回整条电台队列），适配层按 `count` 截断后再返回。
    ///
    /// 契约：未登录 → [`ProviderError::AuthenticationRequired`]（业务码 301，
    /// 必须在通用业务码检查**之前**识别）；服务端无推荐 → [`ProviderError::EmptyResult`]。
    /// 响应布局实测是 `data[]`，每项为 `{alg, id, recommended, songInfo}` 包装，
    /// 曲目字段在 `songInfo` 里（见 [`parse_song_array_response`]）。
    fn intelligence_list(
        &self,
        song_id: &str,
        playlist_id: &str,
        start_id: &str,
        count: u32,
    ) -> impl std::future::Future<Output = Result<Vec<Track>, ProviderError>> + Send;

    /// 本地歌曲文件匹配网易云曲目信息（网易云 `search_match`）。
    ///
    /// 用途：本地文件只有文件名/标签，没有封面、没有歌曲 id，因此既画不出
    /// 真封面，也**拿不到心动模式要的种子上游数据**。匹配成功后返回的
    /// [`Track`] 带网易云 `id` 与 `cover_url`，把本地曲目接进在线生态。
    ///
    /// 参数（对应接口 `title` / `album` / `artist` / `duration` / `md5`）：
    /// - `duration_secs`：**整秒**。接口按秒做时长指纹，SDK 会把该参数
    ///   `parse::<i64>()`，传 `"259.21"` 这样的小数会解析失败退化成 `0`，
    ///   于是任何文件都匹配不上。调用方须先四舍五入。
    /// - `md5`：文件内容 md5（32 位小写十六进制）。实测服务端要求该字段
    ///   **非空**才走匹配流程，为空直接返回空结果。
    ///
    /// 契约：无匹配 → [`ProviderError::EmptyResult`]（不是错误，UI 静默回退到
    /// 本地标签即可）；参数全空 → [`ProviderError::InvalidParameter`]。
    fn search_match(
        &self,
        title: &str,
        album: &str,
        artist: &str,
        duration_secs: u64,
        md5: &str,
    ) -> impl std::future::Future<Output = Result<Vec<Track>, ProviderError>> + Send;

    /// 预检曲目是否可播（计划项 5）。
    ///
    /// 只如实上报接口返回的可播/不可播状态，**不做任何绕过**。
    fn check_availability(
        &self,
        song_id: &str,
    ) -> impl std::future::Future<Output = Result<TrackAvailability, ProviderError>> + Send;

    /// 获取播放地址（规范 7.5 / 6.3）。
    ///
    /// 不可播（VIP、版权下架、地区限制）时返回 [`ProviderError::ContentUnavailable`] /
    /// [`ProviderError::PlaybackUrlUnavailable`]，**不重试、不绕过**。
    fn playback_source(
        &self,
        song_id: &str,
    ) -> impl std::future::Future<Output = Result<PlaybackSource, ProviderError>> + Send;

    /// 设置播放音质上限（**同步、无网络**，随时可调用）。
    ///
    /// 默认实现为空操作：本地文件 Provider 没有"音质档位"这回事，
    /// 不该被迫实现一个它无法尊重的设置。在线 Provider 覆盖它即可。
    fn set_quality(&self, _ceiling: QualityCeiling) {}

    /// 当前生效的音质上限（默认档；供测试与面板回读）。
    fn quality(&self) -> QualityCeiling {
        QualityCeiling::default()
    }

    /// 获取歌词（规范 7.4，阶段 D / M4）。
    ///
    /// - `song_id` 非法时返回 [`ProviderError::InvalidParameter`]，不发起请求。
    /// - 逐字歌词优先，缺失或解析失败自动回退普通歌词（[`super::lyrics::LyricKind`]）。
    /// - 歌词缺失（纯音乐等）返回 [`ProviderError::EmptyResult`]——
    ///   **歌词缺失不得影响播放**，调用方仅提示即可。
    fn lyrics(
        &self,
        song_id: &str,
    ) -> impl std::future::Future<Output = Result<Lyrics, ProviderError>> + Send;

    /// 查询当前登录状态（规范 3.2：登录只解锁用户自有数据）。
    ///
    /// 匿名态返回 [`LoginState::Anonymous`]——**不是错误**，UI 据此展示
    /// 「未登录」而非弹错误框。
    fn login_status(
        &self,
    ) -> impl std::future::Future<Output = Result<LoginState, ProviderError>> + Send;

    /// 发起二维码登录：返回轮询凭据 + 二维码内容。
    ///
    /// 二维码流程**不接触任何账号密码**（用户用手机 App 扫码确认），
    /// 从根源上规避凭据硬编码 / 落盘 / 入日志的风险。
    fn qr_login_start(
        &self,
    ) -> impl std::future::Future<Output = Result<QrLoginSession, ProviderError>> + Send;

    /// 轮询二维码扫码状态。
    ///
    /// - [`LoginState::QrWaiting`]：等待扫码（继续轮询）。
    /// - [`LoginState::QrScanned`]：已扫码待确认（继续轮询）。
    /// - [`LoginState::QrExpired`]：key 过期（重新 `qr_login_start`）。
    /// - [`LoginState::LoggedIn`]：登录成功，会话已由 Provider 内部应用；
    ///   返回值**不含**任何凭据。
    ///
    /// `key` 非法（空）时返回 [`ProviderError::InvalidParameter`]，不发起请求。
    fn qr_login_poll(
        &self,
        key: &str,
    ) -> impl std::future::Future<Output = Result<LoginState, ProviderError>> + Send;

    /// 手机号 + 密码登录（网易云 `/api/w/login/cellphone`）。
    ///
    /// 密码只经 SDK 做 MD5 后发送，**不落盘、不入日志**；成功时会话由
    /// Provider 内部应用，返回的 [`LoginState`] **不含**任何凭据。
    /// `phone`/`password` 为空时返回 [`ProviderError::InvalidParameter`]，不发起请求。
    fn login_cellphone(
        &self,
        phone: &str,
        password: &str,
    ) -> impl std::future::Future<Output = Result<LoginState, ProviderError>> + Send;

    /// 用现有 Cookie 登录：应用 Cookie 后查询登录状态。
    ///
    /// 用户粘贴的 Cookie（`MUSIC_U=…; __csrf=…`）经此方法进入 SDK 客户端，
    /// **不落盘、不入日志**；状态查询结果决定登录是否生效。空 Cookie 返回
    /// [`ProviderError::InvalidParameter`]，不发起请求。
    fn login_cookie(
        &self,
        cookie: &str,
    ) -> impl std::future::Future<Output = Result<LoginState, ProviderError>> + Send;

    /// 退出登录：清空会话 Cookie，回到匿名态。
    fn logout(&self) -> impl std::future::Future<Output = Result<(), ProviderError>> + Send;

    /// 喜欢 / 取消喜欢一首歌（写入账号「我喜欢的音乐」）。
    ///
    /// 这是**官方入口**：网易云 `/api/song/like`，与客户端点红心等价，
    /// 会真的写进账号的歌单，换设备/换客户端都能看到。不是本地标记。
    ///
    /// 契约：
    /// - 需要登录，未登录返回 [`ProviderError::AuthenticationRequired`]，
    ///   **绝不伪造成功**（否则用户以为存进云端了）；
    /// - 服务端拒绝（曲目下架、账号无权限）如实返回错误；
    /// - 本地文件没有云端入口，默认实现返回 `Ok(())` 由调用方自行落本地收藏。
    fn like_song(
        &self,
        _song_id: &str,
        _like: bool,
    ) -> impl std::future::Future<Output = Result<(), ProviderError>> + Send {
        async { Ok(()) }
    }

    /// 账号「我喜欢的音乐」里的**全部歌曲 ID**（需要登录）。
    ///
    /// 用于**云端 → 本地**的收藏同步：登录后拉一次，把云端有而本地没有的
    /// 曲目补进本地收藏。未登录返回 [`ProviderError::AuthenticationRequired`]，
    /// 不返回空列表——空列表会让调用方误以为"云端确实一首都没有"。
    ///
    /// 默认实现返回空列表：本地 Provider 没有云端收藏这回事。
    fn liked_song_ids(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<String>, ProviderError>> + Send {
        async { Ok(Vec::new()) }
    }

    /// 某张专辑的曲目（`/api/v1/album/{id}`）。
    ///
    /// 「点击专辑名跳专辑页」用它取曲目。默认实现返回空列表（本地曲目没有专辑页）。
    fn album_songs(
        &self,
        _album_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<Track>, ProviderError>> + Send {
        async { Ok(Vec::new()) }
    }

    /// 某位艺人的热门曲目（`/api/artist/songs`，按热度排序）。
    ///
    /// 「点击歌手名跳歌手页」用它取曲目。`limit` 为 0 视为非法参数。
    fn artist_songs(
        &self,
        _artist_id: &str,
        _limit: u32,
    ) -> impl std::future::Future<Output = Result<Vec<Track>, ProviderError>> + Send {
        async { Ok(Vec::new()) }
    }

    /// 账号自己创建/收藏的**歌单列表**（需要登录）。
    ///
    /// 未登录返回 [`ProviderError::AuthenticationRequired`]；歌单本身按
    /// [`PlaylistSummary::id`] 走 [`MusicProvider::playlist`] 取曲目。
    fn user_playlists(
        &self,
        _limit: u32,
    ) -> impl std::future::Future<Output = Result<Vec<PlaylistSummary>, ProviderError>> + Send {
        async { Ok(Vec::new()) }
    }

    /// 当前会话 Cookie（仅登录成功后 `Some`；供调用方**加密持久化**，
    /// 不得写入日志 / 错误信息 / 展示给 UI）。
    ///
    /// 默认实现返回 `None`（本地 Provider 没有会话这回事）。
    fn session_cookie(&self) -> Option<String> {
        None
    }
}

// ---------------------------------------------------------------------------
// 对象安全视图（阶段 E：接口统一、可替换）
//
// [`MusicProvider`] 用 RPITIT 返回 `impl Future`，不是对象安全的，UI 层
// 无法持有 `dyn MusicProvider`。本节提供行为完全一致的对象安全投影：
// - [`DynMusicProvider`]：BoxFuture 装箱的同签名方法集；
// - blanket impl：任意 `T: MusicProvider + Send + Sync` 自动实现之。
//
// UI/测试代码持有 `Arc<dyn DynMusicProvider>` 即可**运行时替换**实现
// （真实网易云 ↔ 测试 Mock），更换 Provider 不重写 UI（规范 3.2）。
// 两个 trait 的方法语义、错误分类完全一致——这是同一契约的两种分发方式。
// ---------------------------------------------------------------------------

/// 装箱的 `Send` future，用于对象安全的异步方法签名。
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// [`MusicProvider`] 的对象安全视图。
///
/// 方法集合、参数、返回值与错误语义和 [`MusicProvider`] **逐一对应**；
/// blanket impl 保证二者永不漂移。UI 层依赖本 trait（或 `Arc<dyn …>`），
/// 具体 Provider 仍实现 [`MusicProvider`]（泛型零成本路径）。
pub trait DynMusicProvider: Send + Sync {
    /// Provider 标识。
    fn id(&self) -> ProviderId;

    /// 搜索歌曲（语义同 [`MusicProvider::search_songs`]）。
    fn search_songs<'a>(
        &'a self,
        keyword: &'a str,
        limit: u32,
    ) -> BoxFuture<'a, Result<Vec<Track>, ProviderError>>;

    /// 按 ID 批量取歌曲详情（语义同 [`MusicProvider::song_details`]）。
    fn song_details<'a>(
        &'a self,
        ids: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<Track>, ProviderError>>;

    /// 取歌单分页（语义同 [`MusicProvider::playlist`]）。
    fn playlist<'a>(
        &'a self,
        playlist_id: &'a str,
        offset: u32,
        limit: u32,
    ) -> BoxFuture<'a, Result<Playlist, ProviderError>>;

    /// 精选列表（语义同 [`MusicProvider::featured_playlists`]）。
    fn featured_playlists<'a>(
        &'a self,
        kind: FeaturedKind,
        limit: u32,
    ) -> BoxFuture<'a, Result<Vec<PlaylistSummary>, ProviderError>>;

    /// 每日推荐歌曲（语义同 [`MusicProvider::daily_songs`]）。
    fn daily_songs<'a>(&'a self) -> BoxFuture<'a, Result<Vec<Track>, ProviderError>>;

    /// 心动模式列表（语义同 [`MusicProvider::intelligence_list`]）。
    fn intelligence_list<'a>(
        &'a self,
        song_id: &'a str,
        playlist_id: &'a str,
        start_id: &'a str,
        count: u32,
    ) -> BoxFuture<'a, Result<Vec<Track>, ProviderError>>;

    /// 本地文件匹配（语义同 [`MusicProvider::search_match`]）。
    fn search_match<'a>(
        &'a self,
        title: &'a str,
        album: &'a str,
        artist: &'a str,
        duration_secs: u64,
        md5: &'a str,
    ) -> BoxFuture<'a, Result<Vec<Track>, ProviderError>>;

    /// 可播预检（语义同 [`MusicProvider::check_availability`]）。
    fn check_availability<'a>(
        &'a self,
        song_id: &'a str,
    ) -> BoxFuture<'a, Result<TrackAvailability, ProviderError>>;

    /// 获取播放地址（语义同 [`MusicProvider::playback_source`]）。
    fn playback_source<'a>(
        &'a self,
        song_id: &'a str,
    ) -> BoxFuture<'a, Result<PlaybackSource, ProviderError>>;

    /// 设置播放音质上限（语义同 [`MusicProvider::set_quality`]）。
    fn set_quality(&self, ceiling: QualityCeiling);

    /// 当前音质上限（语义同 [`MusicProvider::quality`]）。
    fn quality(&self) -> QualityCeiling;

    /// 获取歌词（语义同 [`MusicProvider::lyrics`]）。
    fn lyrics<'a>(&'a self, song_id: &'a str) -> BoxFuture<'a, Result<Lyrics, ProviderError>>;

    /// 查询登录状态（语义同 [`MusicProvider::login_status`]）。
    fn login_status<'a>(&'a self) -> BoxFuture<'a, Result<LoginState, ProviderError>>;

    /// 发起二维码登录（语义同 [`MusicProvider::qr_login_start`]）。
    fn qr_login_start<'a>(&'a self) -> BoxFuture<'a, Result<QrLoginSession, ProviderError>>;

    /// 轮询二维码扫码状态（语义同 [`MusicProvider::qr_login_poll`]）。
    fn qr_login_poll<'a>(
        &'a self,
        key: &'a str,
    ) -> BoxFuture<'a, Result<LoginState, ProviderError>>;

    /// 手机号登录（语义同 [`MusicProvider::login_cellphone`]）。
    fn login_cellphone<'a>(
        &'a self,
        phone: &'a str,
        password: &'a str,
    ) -> BoxFuture<'a, Result<LoginState, ProviderError>>;

    /// Cookie 登录（语义同 [`MusicProvider::login_cookie`]）。
    fn login_cookie<'a>(
        &'a self,
        cookie: &'a str,
    ) -> BoxFuture<'a, Result<LoginState, ProviderError>>;

    /// 退出登录（语义同 [`MusicProvider::logout`]）。
    fn logout<'a>(&'a self) -> BoxFuture<'a, Result<(), ProviderError>>;

    /// 喜欢 / 取消喜欢（语义同 [`MusicProvider::like_song`]）。
    fn like_song<'a>(
        &'a self,
        song_id: &'a str,
        like: bool,
    ) -> BoxFuture<'a, Result<(), ProviderError>>;

    /// 云端「我喜欢的音乐」ID 列表（语义同 [`MusicProvider::liked_song_ids`]）。
    fn liked_song_ids<'a>(&'a self) -> BoxFuture<'a, Result<Vec<String>, ProviderError>>;

    /// 专辑曲目（语义同 [`MusicProvider::album_songs`]）。
    fn album_songs<'a>(
        &'a self,
        album_id: &'a str,
    ) -> BoxFuture<'a, Result<Vec<Track>, ProviderError>>;

    /// 艺人热门曲目（语义同 [`MusicProvider::artist_songs`]）。
    fn artist_songs<'a>(
        &'a self,
        artist_id: &'a str,
        limit: u32,
    ) -> BoxFuture<'a, Result<Vec<Track>, ProviderError>>;

    /// 账号歌单列表（语义同 [`MusicProvider::user_playlists`]）。
    fn user_playlists<'a>(
        &'a self,
        limit: u32,
    ) -> BoxFuture<'a, Result<Vec<PlaylistSummary>, ProviderError>>;

    /// 当前会话 Cookie（语义同 [`MusicProvider::session_cookie`]）。
    fn session_cookie(&self) -> Option<String>;
}

/// 全称 blanket impl：任何同步可发送的 [`MusicProvider`] 自动可作
/// [`DynMusicProvider`] 使用。两套接口行为一致，无需每个实现重复适配。
impl<T> DynMusicProvider for T
where
    T: MusicProvider + Send + Sync,
{
    fn id(&self) -> ProviderId {
        MusicProvider::id(self)
    }

    fn search_songs<'a>(
        &'a self,
        keyword: &'a str,
        limit: u32,
    ) -> BoxFuture<'a, Result<Vec<Track>, ProviderError>> {
        Box::pin(T::search_songs(self, keyword, limit))
    }

    fn song_details<'a>(
        &'a self,
        ids: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<Track>, ProviderError>> {
        Box::pin(T::song_details(self, ids))
    }

    fn playlist<'a>(
        &'a self,
        playlist_id: &'a str,
        offset: u32,
        limit: u32,
    ) -> BoxFuture<'a, Result<Playlist, ProviderError>> {
        Box::pin(T::playlist(self, playlist_id, offset, limit))
    }

    fn featured_playlists<'a>(
        &'a self,
        kind: FeaturedKind,
        limit: u32,
    ) -> BoxFuture<'a, Result<Vec<PlaylistSummary>, ProviderError>> {
        Box::pin(T::featured_playlists(self, kind, limit))
    }

    fn daily_songs<'a>(&'a self) -> BoxFuture<'a, Result<Vec<Track>, ProviderError>> {
        Box::pin(T::daily_songs(self))
    }

    fn intelligence_list<'a>(
        &'a self,
        song_id: &'a str,
        playlist_id: &'a str,
        start_id: &'a str,
        count: u32,
    ) -> BoxFuture<'a, Result<Vec<Track>, ProviderError>> {
        Box::pin(T::intelligence_list(
            self,
            song_id,
            playlist_id,
            start_id,
            count,
        ))
    }

    fn search_match<'a>(
        &'a self,
        title: &'a str,
        album: &'a str,
        artist: &'a str,
        duration_secs: u64,
        md5: &'a str,
    ) -> BoxFuture<'a, Result<Vec<Track>, ProviderError>> {
        Box::pin(T::search_match(
            self,
            title,
            album,
            artist,
            duration_secs,
            md5,
        ))
    }

    fn check_availability<'a>(
        &'a self,
        song_id: &'a str,
    ) -> BoxFuture<'a, Result<TrackAvailability, ProviderError>> {
        Box::pin(T::check_availability(self, song_id))
    }

    fn playback_source<'a>(
        &'a self,
        song_id: &'a str,
    ) -> BoxFuture<'a, Result<PlaybackSource, ProviderError>> {
        Box::pin(T::playback_source(self, song_id))
    }

    fn set_quality(&self, ceiling: QualityCeiling) {
        T::set_quality(self, ceiling)
    }

    fn quality(&self) -> QualityCeiling {
        T::quality(self)
    }

    fn lyrics<'a>(&'a self, song_id: &'a str) -> BoxFuture<'a, Result<Lyrics, ProviderError>> {
        Box::pin(T::lyrics(self, song_id))
    }

    fn login_status<'a>(&'a self) -> BoxFuture<'a, Result<LoginState, ProviderError>> {
        Box::pin(T::login_status(self))
    }

    fn qr_login_start<'a>(&'a self) -> BoxFuture<'a, Result<QrLoginSession, ProviderError>> {
        Box::pin(T::qr_login_start(self))
    }

    fn qr_login_poll<'a>(
        &'a self,
        key: &'a str,
    ) -> BoxFuture<'a, Result<LoginState, ProviderError>> {
        Box::pin(T::qr_login_poll(self, key))
    }

    fn login_cellphone<'a>(
        &'a self,
        phone: &'a str,
        password: &'a str,
    ) -> BoxFuture<'a, Result<LoginState, ProviderError>> {
        Box::pin(T::login_cellphone(self, phone, password))
    }

    fn login_cookie<'a>(
        &'a self,
        cookie: &'a str,
    ) -> BoxFuture<'a, Result<LoginState, ProviderError>> {
        Box::pin(T::login_cookie(self, cookie))
    }

    fn logout<'a>(&'a self) -> BoxFuture<'a, Result<(), ProviderError>> {
        Box::pin(T::logout(self))
    }

    fn like_song<'a>(
        &'a self,
        song_id: &'a str,
        like: bool,
    ) -> BoxFuture<'a, Result<(), ProviderError>> {
        Box::pin(T::like_song(self, song_id, like))
    }

    fn liked_song_ids<'a>(&'a self) -> BoxFuture<'a, Result<Vec<String>, ProviderError>> {
        Box::pin(T::liked_song_ids(self))
    }

    fn album_songs<'a>(
        &'a self,
        album_id: &'a str,
    ) -> BoxFuture<'a, Result<Vec<Track>, ProviderError>> {
        Box::pin(T::album_songs(self, album_id))
    }

    fn artist_songs<'a>(
        &'a self,
        artist_id: &'a str,
        limit: u32,
    ) -> BoxFuture<'a, Result<Vec<Track>, ProviderError>> {
        Box::pin(T::artist_songs(self, artist_id, limit))
    }

    fn user_playlists<'a>(
        &'a self,
        limit: u32,
    ) -> BoxFuture<'a, Result<Vec<PlaylistSummary>, ProviderError>> {
        Box::pin(T::user_playlists(self, limit))
    }

    fn session_cookie(&self) -> Option<String> {
        T::session_cookie(self)
    }
}

// ---------------------------------------------------------------------------
// 第三方响应 → 项目模型 的解析边界
//
// 这是「不把未经校验的 JSON 传入 UI」的具体落实点。
// 与阶段一 ncm_probe 的实现一致，迁移为正式代码。
//
// 解析规则（不变式）：
// - 搜索解析**严格**：缺 `id`/`name` → InvalidData（无法定位的条目不交给 UI）。
// - 歌单/批量详情解析**容忍**：单条畸形跳过，整批为空 → EmptyResult。
// - `expires_at` 仅由接口 `expi` 推导，无则 None，不伪造。
// - 响应不是 JSON 对象（如 HTML 错误页）→ UnsupportedResponse。
// ---------------------------------------------------------------------------

/// 响应体必须是 JSON 对象；否则视为不支持的响应（如 HTML 错误页）。
fn ensure_json_object(body: &Value) -> Result<(), ProviderError> {
    if body.is_object() {
        Ok(())
    } else {
        Err(ProviderError::UnsupportedResponse)
    }
}

/// 把单条歌曲 JSON 解析为 [`Track`]；缺关键字段返回 `None`。
///
/// 兼容已知的多种返回路径与新旧字段名（`ar`/`artists`、`al`/`album`、`dt`/`duration`）。
fn parse_track_entry(song: &Value) -> Option<Track> {
    let id = song
        .get("id")
        .and_then(Value::as_i64)
        .map(|v| v.to_string())?;

    let title = song
        .get("name")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .map(str::to_string)?;

    // 兼容 "ar"（新版）与 "artists"（旧版）。
    //
    // 名字与 ID 必须**同一趟**收：分两趟用不同的 filter 收集会让两边下标错位，
    // 点"A"却跳到"C"的歌手页。没给 ID 的那位留空串占位。
    let mut artists: Vec<String> = Vec::new();
    let mut artist_ids: Vec<String> = Vec::new();
    if let Some(arr) = song
        .get("ar")
        .or_else(|| song.get("artists"))
        .and_then(Value::as_array)
    {
        for entry in arr {
            let Some(name) = entry
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.trim().is_empty())
            else {
                continue;
            };
            artists.push(name.to_string());
            artist_ids.push(
                entry
                    .get("id")
                    .and_then(json_id_to_string)
                    .unwrap_or_default(),
            );
        }
    }

    let album = song
        .get("al")
        .or_else(|| song.get("album"))
        .and_then(|a| a.get("name"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);

    let duration_ms = song
        .get("dt")
        .or_else(|| song.get("duration"))
        .and_then(Value::as_u64);

    let cover_url = song
        .get("al")
        .or_else(|| song.get("album"))
        .and_then(|a| a.get("picUrl"))
        .and_then(Value::as_str)
        .map(str::to_string);

    // 专辑 ID：跳「专辑曲子」页用。接口给的是数字，也可能给字符串。
    let album_id = song
        .get("al")
        .or_else(|| song.get("album"))
        .and_then(|a| a.get("id"))
        .and_then(json_id_to_string);

    Some(Track {
        provider: ProviderId::Netease,
        id,
        title,
        artists,
        album,
        album_id,
        artist_ids,
        duration_ms,
        cover_url,
    })
}

/// 从搜索响应体解析歌曲列表（**严格**模式）。
///
/// 缺 `id` 或 `name` 视为 [`ProviderError::InvalidData`]——无法定位的条目不得交给 UI。
pub fn parse_search_response(body: &Value) -> Result<Vec<Track>, ProviderError> {
    ensure_json_object(body)?;

    let songs = body
        .pointer("/result/songs")
        .or_else(|| body.pointer("/data/songs"))
        .or_else(|| body.pointer("/songs"))
        .and_then(Value::as_array)
        .ok_or(ProviderError::InvalidData)?;

    let mut tracks = Vec::with_capacity(songs.len());
    for song in songs {
        tracks.push(parse_track_entry(song).ok_or(ProviderError::InvalidData)?);
    }

    if tracks.is_empty() {
        Err(ProviderError::EmptyResult)
    } else {
        Ok(tracks)
    }
}

/// 从歌曲详情响应体解析歌曲列表（**容忍**模式，规范 7.2）。
///
/// 单条畸形/缺失条目跳过（如已删除曲目）；整批为空 → [`ProviderError::EmptyResult`]。
pub fn parse_song_details_response(body: &Value) -> Result<Vec<Track>, ProviderError> {
    ensure_json_object(body)?;

    let songs = body
        .pointer("/songs")
        .or_else(|| body.pointer("/result/songs"))
        .or_else(|| body.pointer("/data/songs"))
        .and_then(Value::as_array)
        .ok_or(ProviderError::InvalidData)?;

    let tracks: Vec<Track> = songs.iter().filter_map(parse_track_entry).collect();
    if tracks.is_empty() {
        Err(ProviderError::EmptyResult)
    } else {
        Ok(tracks)
    }
}

/// 把 JSON 里的 ID 字段转为字符串（兼容数值与字符串两种表示）。
fn json_id_to_string(v: &Value) -> Option<String> {
    v.as_i64().map(|n| n.to_string()).or_else(|| {
        v.as_str()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
    })
}

/// 从歌单详情响应体解析分页窗口（规范 7.3）。
///
/// - 完整曲目 ID 优先取 `trackIds`，缺失时回退 `tracks`。
/// - `total` 取 `trackCount`，缺失时回退完整 ID 列表长度。
/// - 按 `offset`/`limit` 切出窗口；越界窗口为空（空页不是错误）。
pub fn parse_playlist_detail_response(
    body: &Value,
    offset: u32,
    limit: u32,
) -> Result<PlaylistWindow, ProviderError> {
    ensure_json_object(body)?;

    let playlist = body
        .get("playlist")
        .and_then(Value::as_object)
        .ok_or(ProviderError::InvalidData)?;

    let name = playlist
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);

    let ids_from = |key: &str| -> Option<Vec<String>> {
        playlist.get(key).and_then(Value::as_array).map(|arr| {
            arr.iter()
                .filter_map(|t| t.get("id"))
                .filter_map(json_id_to_string)
                .collect()
        })
    };

    let ids = ids_from("trackIds")
        .or_else(|| ids_from("tracks"))
        .ok_or(ProviderError::InvalidData)?;

    let total = playlist
        .get("trackCount")
        .and_then(Value::as_u64)
        .or(Some(ids.len() as u64));

    let start = (offset as usize).min(ids.len());
    let end = start.saturating_add(limit as usize).min(ids.len());

    Ok(PlaylistWindow {
        name,
        total,
        track_ids: ids[start..end].to_vec(),
    })
}

/// 是否为「算法入口卡」：网易云在推荐歌单流里插入的「私人雷达」。
///
/// 它不是一份歌单集合，而是"按你口味逐日生成"的单曲流入口。混在
/// 「推荐歌单」栏里，整栏看起来就像"私人电台"，与栏目标题（Recommended）
/// 不符，用户会以为推荐功能坏了。
///
/// 只能按名字识别：`/api/personalized/playlist` 的条目结构里没有稳定的
/// 类型字段（`alg` 描述的是召回策略，普通歌单同样带）。因此这里做**保守
/// 匹配**：只认明确带「私人雷达」字样的名字，宁可漏过滤也不误杀正常歌单。
pub fn is_algorithm_playlist_card(name: &str) -> bool {
    name.contains("私人雷达")
}

/// 从精选列表响应体解析歌单摘要（规范 7.6：推荐歌单 / 官方排行榜）。
///
/// **容忍**模式：单条畸形/缺 `id` 条目跳过；整批为空 → [`ProviderError::EmptyResult`]。
/// 字段兼容两种布局：
/// - 推荐 `/personalized` → `result[]`（`picUrl` / `trackCount` / `playCount` / `copywriter`）；
/// - 榜单 `/toplist` → `list[]`（`coverImgUrl` / `trackCount` / `playCount` / `updateFrequency`）；
/// - 每日推荐歌单 `/recommend/resource` → `recommend[]`（`picUrl` / `trackCount` / `playCount`）。
pub fn parse_featured_playlists_response(
    body: &Value,
) -> Result<Vec<PlaylistSummary>, ProviderError> {
    ensure_json_object(body)?;

    let items = body
        .pointer("/result")
        .or_else(|| body.pointer("/recommend"))
        .or_else(|| body.pointer("/list"))
        .or_else(|| body.pointer("/data"))
        .and_then(Value::as_array)
        .ok_or(ProviderError::InvalidData)?;

    let mut out = Vec::with_capacity(items.len());
    for item in items {
        if let Some(summary) = parse_playlist_summary_entry(item) {
            out.push(summary);
        }
    }

    if out.is_empty() {
        Err(ProviderError::EmptyResult)
    } else {
        Ok(out)
    }
}

/// 单条歌单 JSON → [`PlaylistSummary`]；缺 ID 或缺名字返回 `None`（容忍跳过）。
///
/// 抽出来是因为「精选列表」与「我的歌单」两种响应的**条目结构一致**，
/// 只有外层容器不同（`result[]` / `list[]` / `playlist[]`）。
fn parse_playlist_summary_entry(item: &Value) -> Option<PlaylistSummary> {
    let id = item.get("id").and_then(json_id_to_string)?;
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)?;
    // `/api/user/playlist` 用的是 `coverUrl`，精选/搜索类端点用 `picUrl`/`coverImgUrl`；
    // 少认一个键名，账号歌单卡就整列拿不到封面 URL（只剩程序化占位）。
    let cover_url = item
        .get("picUrl")
        .or_else(|| item.get("coverImgUrl"))
        .or_else(|| item.get("coverUrl"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    let track_count = item.get("trackCount").and_then(Value::as_u64);
    let play_count = item.get("playCount").and_then(Value::as_u64);
    let description = item
        .get("copywriter")
        .or_else(|| item.get("updateFrequency"))
        .or_else(|| item.get("description"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    Some(PlaylistSummary {
        provider: ProviderId::Netease,
        id,
        name,
        cover_url,
        track_count,
        play_count,
        description,
    })
}

/// 从「我的歌单」响应体解析歌单摘要（`/api/user/playlist` → `playlist[]`）。
///
/// 空歌单是**合法**结果（新账号一首都没收藏），因此空列表返回 `Ok(vec![])`
/// 而不是 [`ProviderError::EmptyResult`]——UI 要能区分"加载完了，确实没有"
/// 与"加载失败"。
pub fn parse_user_playlists_response(body: &Value) -> Result<Vec<PlaylistSummary>, ProviderError> {
    ensure_json_object(body)?;

    let items = body
        .pointer("/playlist")
        .or_else(|| body.pointer("/data/playlist"))
        .and_then(Value::as_array)
        .ok_or(ProviderError::InvalidData)?;

    Ok(items
        .iter()
        .filter_map(parse_playlist_summary_entry)
        .collect())
}

/// 找账号的「我喜欢的音乐」歌单 ID（`specialType == 5`）。
///
/// 心动模式接口只认这一种歌单当上下文：实测传普通自建歌单或别人的歌单，
/// 服务端回 `{"code":400,"message":"不支持该歌单类型"}`，传 `0` 回「歌单不存在」。
/// 用名字匹配（"<昵称>喜欢的音乐"）会随账号语言/改名失效，所以认 `specialType`。
pub fn find_liked_playlist_id(body: &Value) -> Option<String> {
    body.pointer("/playlist")
        .or_else(|| body.pointer("/data/playlist"))
        .and_then(Value::as_array)?
        .iter()
        .find(|p| p.get("specialType").and_then(Value::as_i64) == Some(5))
        .and_then(|p| p.get("id"))
        .and_then(json_id_to_string)
}

/// 从「我喜欢的音乐」响应体解析歌曲 ID 列表（`/api/song/like/get` → `ids[]`）。
///
/// ID 可能是数字也可能是字符串，统一成字符串；缺 `ids` 节点视为解析失败
/// （而不是"零首"），否则一次接口改版会静默清空用户的收藏。
pub fn parse_liked_song_ids_response(body: &Value) -> Result<Vec<String>, ProviderError> {
    ensure_json_object(body)?;
    let ids = body
        .get("ids")
        .or_else(|| body.pointer("/data/ids"))
        .and_then(Value::as_array)
        .ok_or(ProviderError::InvalidData)?;
    Ok(ids.iter().filter_map(json_id_to_string).collect())
}

/// 从「歌曲数组」型响应体解析曲目列表（每日推荐、心动模式等接口共用）。
///
/// 已知布局（心动模式那条是**实测**结果，API.md 只写了参数没写返回结构）：
/// - 每日推荐歌曲 `/recommend/songs` → `data.dailySongs[]`；
/// - 心动模式 `/playmode/intelligence/list` → `data[]`，每项是
///   `{alg, id, recommended, songInfo}` **包装对象**，曲目字段在 `songInfo` 里；
/// - 兼容 `data.result[]` / `data.songs[]` / `songs[]` / `result.songs[]`。
///
/// **容忍模式**：单条畸形跳过（接口偶有下架曲目），整批为空 →
/// [`ProviderError::EmptyResult`]。未登录时网易云对这几个接口返回空数组，
/// 调用方据此提示登录（不伪造条目）。
pub fn parse_song_array_response(body: &Value) -> Result<Vec<Track>, ProviderError> {
    ensure_json_object(body)?;

    let songs = body
        .pointer("/data/dailySongs")
        .or_else(|| body.pointer("/data/songs"))
        .or_else(|| body.pointer("/data/result"))
        .or_else(|| body.pointer("/result/songs"))
        .or_else(|| body.pointer("/songs"))
        // `/data` 必须放最后：它是「data 本身就是歌曲数组」的布局。
        // 放在前面会让 `{"data":{"songs":[…]}}` 命中
        // 那个**对象**，`as_array()` 失败后 `or_else` 已经不会再回退，
        // 整批被误判成 [`ProviderError::InvalidData`]。
        .or_else(|| body.pointer("/data"))
        .and_then(Value::as_array)
        .ok_or(ProviderError::InvalidData)?;

    let tracks: Vec<Track> = songs
        .iter()
        .map(unwrap_song_payload)
        .filter_map(parse_track_entry)
        .collect();
    if tracks.is_empty() {
        Err(ProviderError::EmptyResult)
    } else {
        Ok(tracks)
    }
}

/// 数组条目 → 曲目对象本身：把 `{…, "songInfo": {…}}` 这类包装剥掉。
///
/// 不剥的话 `name` / `ar` / `al` 全在下一层，`parse_track_entry` 会把**每一条**
/// 判成畸形并跳过，整批变成空 → [`ProviderError::EmptyResult`]。心动模式因此
/// 表现为「点了没反应、提示没有可推荐的续曲」，而服务端其实给了 149 首。
fn unwrap_song_payload(item: &Value) -> &Value {
    item.get("songInfo")
        .or_else(|| item.get("song"))
        .filter(|v| v.is_object())
        .unwrap_or(item)
}

/// 从播放地址响应体解析 [`PlaybackSource`]（规范 7.5 / 6.3）。
///
/// - 条目业务码非 200（版权下架等）→ [`ProviderError::ContentUnavailable`]，
///   **如实上报、不重试、不绕过**。
/// - `url` 为空 → [`ProviderError::PlaybackUrlUnavailable`]。
/// - `expires_at` 仅由 `expi`（有效期秒数，须为正数）推导：`now_unix + expi`。
pub fn parse_playback_source(body: &Value, now_unix: i64) -> Result<PlaybackSource, ProviderError> {
    ensure_json_object(body)?;

    let entries = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or(ProviderError::InvalidData)?;

    let Some(entry) = entries.first() else {
        // 响应结构正常但没有任何播放地址条目
        return Err(ProviderError::PlaybackUrlUnavailable);
    };

    if let Some(code) = entry.get("code").and_then(Value::as_i64) {
        if code != 200 {
            tracing::debug!("播放地址条目业务码非 200: code={}", code);
            return Err(ProviderError::ContentUnavailable);
        }
    }

    let url = entry
        .get("url")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .ok_or(ProviderError::PlaybackUrlUnavailable)?;

    // 仅在接口确实提供有效期时填写；缺失或非正数 → None，不伪造
    let expires_at = entry
        .get("expi")
        .and_then(Value::as_i64)
        .filter(|secs| *secs > 0)
        .map(|secs| now_unix.saturating_add(secs));

    let quality = entry
        .get("level")
        .and_then(Value::as_str)
        .map(AudioQuality::from_api_level);

    let content_type = entry
        .get("type")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);

    // `br` 是 bps（如 320000 / 962277）；低于 1 kbps 的取值按「没给」处理，
    // 不留下一个会让起播闸门归零的假码率。
    let bitrate_kbps = entry
        .get("br")
        .and_then(Value::as_u64)
        .filter(|bps| *bps >= 1_000)
        .map(|bps| (bps / 1_000) as u32);

    Ok(PlaybackSource {
        url,
        expires_at,
        quality,
        content_type,
        bitrate_kbps,
    })
}

/// 从播放地址响应体解析可播状态（`check_music` 与 `song_url` 打同一端点）。
///
/// - 有非空 `url` → [`TrackAvailability::Available`]。
/// - 否则 [`TrackAvailability::Unavailable`]：线索取接口 `message`/`msg`，
///   其次为业务码文本；都没有则 `None`，**不编造**原因。
pub fn parse_availability_response(body: &Value) -> Result<TrackAvailability, ProviderError> {
    ensure_json_object(body)?;

    let entries = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or(ProviderError::InvalidData)?;

    let Some(entry) = entries.first() else {
        return Ok(TrackAvailability::Unavailable(None));
    };

    let has_url = entry
        .get("url")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .is_some();
    if has_url {
        return Ok(TrackAvailability::Available);
    }

    let reason = entry
        .get("message")
        .or_else(|| entry.get("msg"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            entry
                .get("code")
                .and_then(Value::as_i64)
                .map(|c| format!("接口返回 code={}", c))
        });

    Ok(TrackAvailability::Unavailable(reason))
}

/// 从歌词响应体解析歌词（规范 7.4）。
///
/// 分级规则：
/// - `yrc.lyric`（逐字）非空且能解析出有效行 → [`super::lyrics::LyricKind::WordByWord`]；
/// - 否则回退 `lrc.lyric`（普通）→ [`super::lyrics::LyricKind::Plain`]；
/// - 逐字解析整体失败（无有效行）**自动回退**普通歌词；
/// - 两级都无有效歌词 → [`ProviderError::EmptyResult`]（歌词缺失，不伪造）；
/// - 响应不是 JSON 对象 → [`ProviderError::UnsupportedResponse`]。
pub fn parse_lyric_response(body: &Value) -> Result<Lyrics, ProviderError> {
    ensure_json_object(body)?;

    let section_text = |key: &str| -> Option<&str> {
        body.get(key)
            .and_then(|v| v.get("lyric"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };

    // 逐字优先
    if let Some(yrc) = section_text("yrc") {
        let lyrics = parse_yrc(yrc);
        if !lyrics.is_empty() {
            return Ok(lyrics);
        }
        tracing::warn!("逐字歌词解析无有效行，回退普通歌词");
    }

    // 普通回退
    if let Some(lrc) = section_text("lrc") {
        let lyrics = parse_lrc(lrc);
        if !lyrics.is_empty() {
            return Ok(lyrics);
        }
        tracing::warn!("普通歌词解析无有效行");
    }

    Err(ProviderError::EmptyResult)
}

// ---------------------------------------------------------------------------
// 登录响应解析（阶段 G）
//
// 二维码登录三步的类型化边界：
// - `parse_qr_key_response`：unikey 提取；
// - `parse_qr_check_response`：轮询状态码 → [`LoginState`]（803 成功时
//   原样移交 Set-Cookie 材料，由 Provider 内部应用，**不外泄**）；
// - `parse_login_status_response`：账号信息 → [`LoginState`]。
// ---------------------------------------------------------------------------

/// 从二维码 key 响应提取 `unikey`。
///
/// 缺失 / 非字符串 / 空串 → [`ProviderError::InvalidData`]。
pub fn parse_qr_key_response(body: &Value) -> Result<String, ProviderError> {
    ensure_json_object(body)?;
    body.get("unikey")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or(ProviderError::InvalidData)
}

/// 从二维码轮询响应里取出会话 Cookie。
///
/// 网易云有**两条**投递路径，必须都认：
/// 1. `Set-Cookie` 响应头（SDK 收集成 `cookie_parts`）；
/// 2. **响应体 `cookie` 字段**——803 成功时网易云常把
///    `"MUSIC_U=…; __csrf=…"` 直接塞进 JSON body，此时响应头可能一个都没有。
///
/// 只看头会得到「没有会话材料的登录成功」，进而判定失败——这正是
/// 「手机已确认授权、程序仍显示未登录」的成因之一。
fn extract_session_cookie(body: &Value, cookie_parts: &[String]) -> Option<String> {
    join_set_cookie(cookie_parts)
        .or_else(|| {
            body.get("cookie")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        // 部分版本包一层 data
        .or_else(|| {
            body.get("data")
                .and_then(|d| d.get("cookie"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
}

/// 解析二维码轮询响应。
///
/// 接口语义（`code` 字段）：
/// - `801` 等待扫码 → [`LoginState::QrWaiting`]
/// - `802` 已扫码待确认 → [`LoginState::QrScanned`]
/// - `803` 登录成功 → [`LoginState::LoggedIn`]（携带会话 Cookie）
/// - `800` 二维码过期 → [`LoginState::QrExpired`]
/// - 其他 / 缺失 → [`ProviderError::InvalidData`]（不编造状态）
///
/// `cookie_parts` 为 SDK [`ApiResponse`](ncm_api_rs::request::ApiResponse)
/// 收集的 Set-Cookie 条目，这里提取 `name=value` 拼成 Cookie 串；
/// 无有效 Cookie 条目时即使 `code=803` 也按 [`ProviderError::InvalidData`] 处理——
/// **没有会话材料的「登录成功」不可信**。
pub fn parse_qr_check_response(
    body: &Value,
    cookie_parts: &[String],
) -> Result<QrPollOutcome, ProviderError> {
    ensure_json_object(body)?;
    let code = body
        .get("code")
        .and_then(|c| {
            c.as_i64()
                .or_else(|| c.as_str().and_then(|s| s.parse::<i64>().ok()))
        })
        .ok_or(ProviderError::InvalidData)?;

    match code {
        801 => Ok(QrPollOutcome::state_only(LoginState::QrWaiting)),
        802 => Ok(QrPollOutcome::state_only(LoginState::QrScanned)),
        800 => Ok(QrPollOutcome::state_only(LoginState::QrExpired)),
        803 => {
            let cookie =
                extract_session_cookie(body, cookie_parts).ok_or(ProviderError::InvalidData)?;
            let nickname = body
                .get("nickname")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .or_else(|| {
                    body.get("profile")
                        .and_then(|p| p.get("nickname"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                        .map(str::to_string)
                });
            let user_id = body
                .get("userId")
                .and_then(|v| v.as_i64().map(|n| n.to_string()))
                .or_else(|| {
                    body.get("profile")
                        .and_then(|p| p.get("userId"))
                        .and_then(|v| v.as_i64().map(|n| n.to_string()))
                });
            Ok(QrPollOutcome::success(cookie, nickname, user_id))
        }
        other => {
            tracing::warn!("二维码轮询未知码 code={}", other);
            Err(ProviderError::InvalidData)
        }
    }
}

/// 解析登录状态响应（`/api/w/nuser/account/get`）。
///
/// `profile` 为 `null` 或缺失 → [`LoginState::Anonymous`]（匿名不是错误）；
/// 有 `profile` → [`LoginState::LoggedIn`]。
pub fn parse_login_status_response(body: &Value) -> Result<LoginState, ProviderError> {
    ensure_json_object(body)?;
    let profile = match body.get("profile") {
        Some(p) if !p.is_null() => p,
        _ => return Ok(LoginState::Anonymous),
    };
    let nickname = profile
        .get("nickname")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    let user_id = profile
        .get("userId")
        .and_then(|v| v.as_i64().map(|n| n.to_string()));
    Ok(LoginState::LoggedIn { nickname, user_id })
}

/// 解析「手机号 / Cookie」登录的响应。
///
/// 与二维码轮询不同，这两个接口用**业务码**表达成败（`code=200` 成功），
/// 成功响应通常携带 `profile` / `token` / `cookie`。返回
/// `(LoginState, Option<cookie>)`：cookie 仅在服务端确实回传会话材料时为 `Some`，
/// 由 Provider 内部应用，**不外泄**。
///
/// 失败码（如 501 账号不存在 / 502 密码错误 / 503 频率限制）如实映射为
/// 可读错误，不编造、不重试。
pub fn parse_cellphone_login_response(
    body: &Value,
    cookie_parts: &[String],
) -> Result<(LoginState, Option<String>), ProviderError> {
    ensure_json_object(body)?;
    let code = body
        .get("code")
        .and_then(|c| {
            c.as_i64()
                .or_else(|| c.as_str().and_then(|s| s.parse::<i64>().ok()))
        })
        .ok_or(ProviderError::InvalidData)?;

    if code != 200 {
        let msg = body
            .get("message")
            .or_else(|| body.get("msg"))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("code={}", code));
        return match code {
            // 503 = 频繁请求；501/502 = 账号或密码错误；400 = 参数错误
            503 => Err(ProviderError::RateLimited),
            400 => Err(ProviderError::InvalidParameter),
            501 | 502 => Err(ProviderError::Unknown(msg)),
            other => Err(ProviderError::Unknown(format!("code={} {}", other, msg))),
        };
    }

    let cookie = extract_session_cookie(body, cookie_parts);
    let profile = body.get("profile").filter(|p| !p.is_null());
    let nickname = profile
        .and_then(|p| p.get("nickname"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    let user_id = profile
        .and_then(|p| p.get("userId"))
        .and_then(|v| v.as_i64().map(|n| n.to_string()));
    Ok((LoginState::LoggedIn { nickname, user_id }, cookie))
}

/// 把 Set-Cookie 条目拼成 Cookie 请求头串（`name=value; name2=value2`）。
///
/// 每个条目形如 `MUSIC_R_T=…; Max-Age=…; Path=/`，这里**只**取首个
/// `name=value` 段（逗号前的属性段全部丢弃）。全部条目都提不出有效对时
/// 返回 `None`（调用方按凭据缺失处理）。
pub fn join_set_cookie(parts: &[String]) -> Option<String> {
    let pairs: Vec<String> = parts
        .iter()
        .filter_map(|raw| {
            // Set-Cookie 一条一个 cookie；n=v 后可能跟 ", Expires=…" 等属性
            let first = raw.split(';').next().unwrap_or("");
            // 同一条目内可能有逗号拼接的第二个 cookie，只取第一段
            let first = first.split(',').next().unwrap_or("");
            let (name, value) = first.split_once('=')?;
            let name = name.trim();
            let value = value.trim();
            if name.is_empty() || value.is_empty() {
                return None;
            }
            Some(format!("{}={}", name, value))
        })
        .collect();
    if pairs.is_empty() {
        None
    } else {
        Some(pairs.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::music::lyrics::LyricKind;
    use serde_json::json;

    // ---- 搜索解析（严格）-----------------------------------------------

    #[test]
    fn parses_normal_response_with_all_fields() {
        let body = json!({
            "result": { "songs": [
                { "id": 347230, "name": "海阔天空",
                  "ar": [{"id": 11127, "name": "Beyond"}],
                  "al": {"name": "乐与怒", "picUrl": "http://x/cover.jpg"},
                  "dt": 326000 }
            ] }
        });
        let tracks = parse_search_response(&body).unwrap();
        assert_eq!(tracks.len(), 1);
        let t = &tracks[0];
        assert_eq!(t.provider, ProviderId::Netease);
        assert_eq!(t.id, "347230");
        assert_eq!(t.title, "海阔天空");
        assert_eq!(t.artists, vec!["Beyond".to_string()]);
        assert_eq!(t.album.as_deref(), Some("乐与怒"));
        assert_eq!(t.duration_ms, Some(326000));
        assert_eq!(t.cover_url.as_deref(), Some("http://x/cover.jpg"));
    }

    #[test]
    fn parses_legacy_field_names() {
        let body = json!({
            "result": { "songs": [
                { "id": 1, "name": "A",
                  "artists": [{"name": "X"}, {"name": "Y"}],
                  "album": {"name": "Z"}, "duration": 1000 }
            ] }
        });
        let tracks = parse_search_response(&body).unwrap();
        assert_eq!(tracks[0].artists, vec!["X".to_string(), "Y".to_string()]);
        assert_eq!(tracks[0].album.as_deref(), Some("Z"));
        assert_eq!(tracks[0].duration_ms, Some(1000));
        // 旧格式只有名字没有 ID：宁可缺席，也不能编一个占位 ID 出来
        assert_eq!(tracks[0].album_id, None);
        assert!(
            tracks[0].artist_ids.iter().all(String::is_empty),
            "没给 ID 时留空串占位（长度仍与名字对齐）"
        );
        assert_eq!(tracks[0].artist_ids.len(), tracks[0].artists.len());
    }

    #[test]
    fn album_and_artist_ids_are_parsed_for_navigation() {
        // 「点专辑名跳专辑页 / 点歌手名跳歌手页」全靠这两个字段
        let body = json!({
            "result": { "songs": [
                { "id": 7, "name": "S",
                  "ar": [{"id": 11127, "name": "Beyond"}, {"id": "222", "name": "客串"}],
                  "al": {"id": 32311, "name": "乐与怒"},
                  "dt": 1000 }
            ] }
        });
        let t = &parse_search_response(&body).unwrap()[0];
        assert_eq!(t.album_id.as_deref(), Some("32311"));
        // 数字与字符串形式的 ID 都要吃得下（接口两种都给过）
        assert_eq!(t.artist_ids, vec!["11127".to_string(), "222".to_string()]);
        assert_eq!(t.artist_ids.len(), t.artists.len(), "ID 与名字必须一一对应");
    }

    #[test]
    fn artist_ids_stay_aligned_with_names_when_some_are_missing() {
        // 混合情况：中间那位只有名字。分两趟 filter 收集会让后面的 ID 错位到
        // 前一个人身上——点 B 跳到 C 的歌手页。空串占位是唯一安全的做法。
        let body = json!({
            "result": { "songs": [
                { "id": 9, "name": "S",
                  "ar": [{"id": 1, "name": "A"}, {"name": "B"}, {"id": 3, "name": "C"}],
                  "al": {"name": "X"}, "dt": 1 }
            ] }
        });
        let t = &parse_search_response(&body).unwrap()[0];
        assert_eq!(
            t.artists,
            vec!["A".to_string(), "B".to_string(), "C".to_string()]
        );
        assert_eq!(
            t.artist_ids,
            vec!["1".to_string(), String::new(), "3".to_string()],
            "ID 必须与名字同下标，缺失位留空串"
        );
        assert_eq!(t.artist_ids.len(), t.artists.len(), "长度必须相等");
    }

    #[test]
    fn multiple_artists_are_preserved() {
        let body = json!({
            "result": { "songs": [
                { "id": 346013, "name": "海阔天空(Live)",
                  "ar": [{"name": "黄贯中"}, {"name": "叶世荣"}, {"name": "黄家强"}] }
            ] }
        });
        let tracks = parse_search_response(&body).unwrap();
        assert_eq!(tracks[0].artists.len(), 3);
        assert_eq!(
            tracks[0].display_line(),
            "黄贯中/叶世荣/黄家强 - 海阔天空(Live)"
        );
    }

    #[test]
    fn empty_result_is_distinguished_from_invalid_data() {
        let body = json!({ "result": { "songs": [], "songCount": 0 } });
        assert_eq!(
            parse_search_response(&body).unwrap_err(),
            ProviderError::EmptyResult
        );
    }

    #[test]
    fn missing_songs_node_is_invalid_data() {
        for body in [json!({ "result": {} }), json!({ "code": 200 })] {
            assert_eq!(
                parse_search_response(&body).unwrap_err(),
                ProviderError::InvalidData
            );
        }
    }

    #[test]
    fn song_without_id_or_title_is_invalid_data() {
        let no_id = json!({ "result": { "songs": [ { "name": "x" } ] } });
        assert_eq!(
            parse_search_response(&no_id).unwrap_err(),
            ProviderError::InvalidData
        );

        let no_name = json!({ "result": { "songs": [ { "id": 1 } ] } });
        assert_eq!(
            parse_search_response(&no_name).unwrap_err(),
            ProviderError::InvalidData
        );

        let blank_name = json!({ "result": { "songs": [ { "id": 1, "name": "  " } ] } });
        assert_eq!(
            parse_search_response(&blank_name).unwrap_err(),
            ProviderError::InvalidData
        );
    }

    #[test]
    fn missing_optional_fields_are_tolerated_not_faked() {
        let body = json!({ "result": { "songs": [ { "id": 7, "name": "Solo" } ] } });
        let t = &parse_search_response(&body).unwrap()[0];
        assert!(t.artists.is_empty());
        assert_eq!(t.album, None);
        assert_eq!(t.duration_ms, None);
        assert_eq!(t.cover_url, None);
        assert_eq!(t.display_line(), "Solo");
    }

    // ---- 展示辅助（只影响显示，不回写模型）-----------------------------

    fn track_with_duration(ms: Option<u64>) -> Track {
        Track {
            provider: ProviderId::Netease,
            id: "1".into(),
            title: "T".into(),
            artists: vec![],
            album: None,
            duration_ms: ms,
            cover_url: None,
            ..Track::default()
        }
    }

    #[test]
    fn duration_text_formats_mm_ss() {
        assert_eq!(track_with_duration(Some(0)).duration_text(), "00:00");
        assert_eq!(track_with_duration(Some(326_000)).duration_text(), "05:26");
        assert_eq!(track_with_duration(Some(59_999)).duration_text(), "00:59");
    }

    #[test]
    fn duration_text_uses_h_mm_ss_beyond_one_hour() {
        assert_eq!(
            track_with_duration(Some(3_600_000)).duration_text(),
            "1:00:00"
        );
        assert_eq!(
            track_with_duration(Some(3_723_000)).duration_text(),
            "1:02:03"
        );
    }

    #[test]
    fn duration_text_unknown_is_placeholder_not_zero() {
        // 时长未知 → 占位符，不伪造 00:00
        assert_eq!(track_with_duration(None).duration_text(), "--:--");
    }

    // ---- 歌曲详情解析（容忍）-------------------------------------------

    #[test]
    fn song_details_parses_top_level_songs_node() {
        let body = json!({
            "songs": [
                { "id": 347230, "name": "海阔天空",
                  "ar": [{"name": "Beyond"}], "al": {"name": "乐与怒"}, "dt": 326000 }
            ],
            "privileges": [],
            "code": 200
        });
        let tracks = parse_song_details_response(&body).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id, "347230");
    }

    #[test]
    fn song_details_skips_malformed_and_deleted_entries() {
        // 3 条中 1 条缺 name（视为无法定位）、1 条完全不是对象 → 只保留有效条目
        let body = json!({
            "songs": [
                { "id": 1, "name": "有效" },
                { "id": 2 },
                42
            ]
        });
        let tracks = parse_song_details_response(&body).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id, "1");
    }

    #[test]
    fn song_details_all_malformed_is_empty_result() {
        let body = json!({ "songs": [ { "id": 2 }, "oops" ] });
        assert_eq!(
            parse_song_details_response(&body).unwrap_err(),
            ProviderError::EmptyResult
        );
    }

    #[test]
    fn song_details_missing_node_or_non_object_rejected() {
        assert_eq!(
            parse_song_details_response(&json!({ "code": 200 })).unwrap_err(),
            ProviderError::InvalidData
        );
        // HTML 错误页等非 JSON 对象 → UnsupportedResponse
        assert_eq!(
            parse_song_details_response(&json!("<html>502</html>")).unwrap_err(),
            ProviderError::UnsupportedResponse
        );
    }

    // ---- 歌单分页解析 ---------------------------------------------------

    #[test]
    fn playlist_window_slices_track_ids_by_offset_and_limit() {
        let body = json!({
            "playlist": {
                "id": 24381616, "name": "我喜欢的音乐", "trackCount": 5,
                "trackIds": [ {"id": 1}, {"id": 2}, {"id": 3}, {"id": 4}, {"id": 5} ]
            },
            "code": 200
        });
        let w = parse_playlist_detail_response(&body, 1, 2).unwrap();
        assert_eq!(w.name.as_deref(), Some("我喜欢的音乐"));
        assert_eq!(w.total, Some(5));
        assert_eq!(w.track_ids, vec!["2".to_string(), "3".to_string()]);
    }

    #[test]
    fn playlist_window_offset_beyond_end_is_empty_not_error() {
        let body = json!({
            "playlist": { "name": "P", "trackCount": 2, "trackIds": [ {"id": 1}, {"id": 2} ] }
        });
        let w = parse_playlist_detail_response(&body, 99, 10).unwrap();
        assert!(w.track_ids.is_empty());
        assert_eq!(w.total, Some(2));
    }

    #[test]
    fn playlist_window_falls_back_to_tracks_and_track_count() {
        // 无 trackIds / trackCount 时回退 tracks 与其长度
        let body = json!({
            "playlist": { "name": "P", "tracks": [ {"id": 7}, {"id": 8} ] }
        });
        let w = parse_playlist_detail_response(&body, 0, 10).unwrap();
        assert_eq!(w.track_ids, vec!["7".to_string(), "8".to_string()]);
        assert_eq!(w.total, Some(2));
    }

    #[test]
    fn playlist_missing_playlist_node_is_invalid_data() {
        for body in [
            json!({ "code": 200 }),
            json!({ "playlist": { "name": "无曲目列表" } }),
        ] {
            assert_eq!(
                parse_playlist_detail_response(&body, 0, 10).unwrap_err(),
                ProviderError::InvalidData
            );
        }
    }

    // ---- 精选列表解析（推荐 / 榜单，容忍）-------------------------------

    #[test]
    fn algorithm_entry_cards_are_recognised_by_name() {
        // 回归：登录后 /api/personalized/playlist 首位会插「私人雷达」，
        // 它不是歌单集合，留在「推荐歌单」栏里会让整栏看起来像私人电台。
        assert!(is_algorithm_playlist_card(
            "今天从《猛独が袭う》听起 | 私人雷达"
        ));
        assert!(is_algorithm_playlist_card("私人雷达"));
        assert!(is_algorithm_playlist_card(
            "日系私人雷达 | 最懂你的日系推荐 每日更新35首"
        ));
    }

    #[test]
    fn normal_playlists_are_not_mistaken_for_algorithm_cards() {
        // 保守匹配：名字里没有「私人雷达」的正常歌单一律放行，
        // 宁可漏过滤也不误杀（推荐栏宁可多一条也不能少一条真歌单）。
        for name in [
            "华语速爆",
            "ACG 精选",
            "【作业用BGM】东方燃曲",
            "「Billboard2026」术力口传说曲",
            "私人收藏 · 老歌",
            "雷达测速合集",
        ] {
            assert!(!is_algorithm_playlist_card(name), "误杀：{name}");
        }
    }

    #[test]
    fn featured_parses_personalized_result_layout() {
        // 推荐 /personalized → result[]（picUrl / copywriter）
        let body = json!({
            "result": [
                { "id": 24381616, "name": "华语速爆", "picUrl": "http://x/c1.jpg",
                  "trackCount": 30, "playCount": 1234567, "copywriter": "今日推荐" },
                { "id": 3100990342u64, "name": "ACG 精选", "picUrl": "http://x/c2.jpg",
                  "trackCount": 50, "playCount": 999 }
            ]
        });
        let items = parse_featured_playlists_response(&body).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].provider, ProviderId::Netease);
        assert_eq!(items[0].id, "24381616");
        assert_eq!(items[0].cover_url.as_deref(), Some("http://x/c1.jpg"));
        assert_eq!(items[0].track_count, Some(30));
        assert_eq!(items[0].play_count, Some(1234567));
        assert_eq!(items[0].description.as_deref(), Some("今日推荐"));
        assert_eq!(items[1].description, None, "无 copywriter 不伪造");
    }

    #[test]
    fn featured_parses_toplist_list_layout() {
        // 榜单 /toplist → list[]（coverImgUrl / updateFrequency）
        let body = json!({
            "list": [
                { "id": 19723756, "name": "飙升榜", "coverImgUrl": "http://x/t1.jpg",
                  "trackCount": 100, "playCount": 88888, "updateFrequency": "每天更新" }
            ]
        });
        let items = parse_featured_playlists_response(&body).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "19723756");
        assert_eq!(items[0].cover_url.as_deref(), Some("http://x/t1.jpg"));
        assert_eq!(items[0].description.as_deref(), Some("每天更新"));
    }

    #[test]
    fn featured_tolerates_malformed_entries_but_keeps_good_ones() {
        let body = json!({
            "result": [
                { "name": "缺 ID 无法定位" },
                { "id": 7, "name": "   " },
                { "id": 8, "name": "有效条目" }
            ]
        });
        let items = parse_featured_playlists_response(&body).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "8");
    }

    #[test]
    fn featured_empty_or_invalid_is_distinguished() {
        // 整批无有效条目 → EmptyResult
        assert_eq!(
            parse_featured_playlists_response(&json!({ "result": [] })).unwrap_err(),
            ProviderError::EmptyResult
        );
        // 无数组节点 → InvalidData
        assert_eq!(
            parse_featured_playlists_response(&json!({ "code": 200 })).unwrap_err(),
            ProviderError::InvalidData
        );
        // 非 JSON 对象 → UnsupportedResponse
        assert!(matches!(
            parse_featured_playlists_response(&json!("<html>error</html>")),
            Err(ProviderError::UnsupportedResponse)
        ));
    }

    // ---- 我的歌单 / 云端收藏 --------------------------------------------

    #[test]
    fn user_playlists_parses_playlist_node_and_tolerates_junk() {
        let body = json!({
            "code": 200,
            "more": false,
            "playlist": [
                { "id": 501, "name": "我喜欢的音乐", "trackCount": 128,
                  "coverImgUrl": "http://x/a.jpg" },
                { "name": "缺 ID" },
                { "id": 502, "name": "自建歌单", "trackCount": 3 }
            ]
        });
        let lists = parse_user_playlists_response(&body).unwrap();
        assert_eq!(lists.len(), 2, "缺 ID 的条目跳过，其余保留");
        assert_eq!(lists[0].id, "501");
        assert_eq!(lists[0].name, "我喜欢的音乐");
        assert_eq!(lists[0].track_count, Some(128));
        assert_eq!(lists[0].cover_url.as_deref(), Some("http://x/a.jpg"));
        assert_eq!(lists[1].id, "502");
        assert!(lists.iter().all(|p| p.provider == ProviderId::Netease));
    }

    #[test]
    fn empty_user_playlist_is_ok_not_empty_result() {
        // 新账号一个歌单都没有是**合法**状态：UI 要显示「没有歌单」，
        // 不能显示成「加载失败」。这是它与 featured 的关键区别。
        let lists = parse_user_playlists_response(&json!({ "playlist": [] })).unwrap();
        assert!(lists.is_empty());
        // 但缺节点仍是解析失败（接口改版不能静默变成"零歌单"）
        assert_eq!(
            parse_user_playlists_response(&json!({ "code": 200 })).unwrap_err(),
            ProviderError::InvalidData
        );
    }

    #[test]
    fn liked_ids_accept_numbers_and_strings() {
        let body = json!({ "code": 200, "ids": [347230, "186016", 0] });
        let ids = parse_liked_song_ids_response(&body).unwrap();
        assert_eq!(ids, vec!["347230", "186016", "0"]);
    }

    #[test]
    fn missing_ids_node_is_invalid_data_not_zero_favorites() {
        // 若把缺节点当成"零首"，一次接口改版就会静默清空用户的本地收藏
        assert_eq!(
            parse_liked_song_ids_response(&json!({ "code": 200 })).unwrap_err(),
            ProviderError::InvalidData
        );
        // 真空数组才是"确实一首都没喜欢"
        assert!(parse_liked_song_ids_response(&json!({ "ids": [] }))
            .unwrap()
            .is_empty());
    }

    // ---- 每日推荐歌曲解析 -----------------------------------------------

    #[test]
    fn daily_songs_are_parsed_from_the_v3_daily_songs_layout() {
        // `/api/v3/discovery/recommend/songs` 的真实布局：`data.dailySongs[]`
        let body = json!({
            "code": 200,
            "data": {
                "dailySongs": [
                    { "id": 1, "name": "今日一号",
                      "ar": [{"id": 11, "name": "甲"}],
                      "al": {"id": 111, "name": "专辑甲", "picUrl": "http://x/1.jpg"},
                      "dt": 210000 },
                    { "id": 2, "name": "今日二号",
                      "ar": [{"id": 22, "name": "乙"}],
                      "al": {"id": 222, "name": "专辑乙"},
                      "dt": 190000 }
                ]
            }
        });
        let tracks = parse_song_array_response(&body).unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].title, "今日一号");
        assert_eq!(tracks[0].album_id.as_deref(), Some("111"));
        assert_eq!(tracks[0].artist_ids, vec!["11".to_string()]);
        assert_eq!(tracks[1].duration_ms, Some(190000));
    }

    #[test]
    fn daily_songs_empty_list_is_empty_result_not_a_silent_zero() {
        // 登录了但当天确实没有推荐 → EmptyResult（UI 显示"没有内容"）；
        // 缺节点才是 InvalidData（接口改版，不能装作"今天没歌"）。
        assert_eq!(
            parse_song_array_response(&json!({ "code": 200, "data": { "dailySongs": [] } }))
                .unwrap_err(),
            ProviderError::EmptyResult
        );
        assert_eq!(
            parse_song_array_response(&json!({ "code": 200, "data": {} })).unwrap_err(),
            ProviderError::InvalidData
        );
    }

    #[test]
    fn nested_data_songs_layout_is_not_shadowed_by_the_bare_data_branch() {
        // 回归：`/data` 分支若排在 `/data/songs` 前面，`{"data":{"songs":[…]}}`
        // 会先命中那个**对象**，`as_array()` 失败后 or_else 已不再回退 → InvalidData
        let body = json!({
            "data": { "songs": [ { "id": 5, "name": "嵌套", "ar": [{"name": "丙"}] } ] }
        });
        let tracks = parse_song_array_response(&body).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "嵌套");
    }

    #[test]
    fn intelligence_list_layout_unwraps_the_songinfo_wrapper() {
        // 实测（登录账号，pid=红心歌单）心动模式返回 `data[]`，每项是
        // `{alg, id, recommended, songInfo}`——曲目字段全在 `songInfo` 里。
        // 不剥这层包装时 `parse_track_entry` 把**每条**都判成畸形、整批变空，
        // UI 于是提示「暂时没有可推荐的续曲」，而服务端其实一次给了 149 首。
        let body = json!({
            "code": 200,
            "data": [
                { "alg": "Alg_AI_One_27604614:-1", "id": 2690153653i64, "recommended": true,
                  "songInfo": { "id": 2690153653i64, "name": "弱虫モンブラン (Reloaded)",
                      "ar": [{"id": 1, "name": "DECO*27"}],
                      "al": {"id": 267649656, "name": "弱虫モンブラン", "picUrl": "http://p/1.jpg"},
                      "dt": 213013 } },
                { "alg": "sameTag-RH-sim", "id": 3438968283i64, "recommended": true,
                  "songInfo": { "id": 3438968283i64, "name": "花骨朵",
                      "ar": [{"id": 906118, "name": "洛天依Official"}],
                      "al": {"id": 399524457, "name": "花骨朵"}, "dt": 167083 } }
            ]
        });
        let tracks = parse_song_array_response(&body).unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].id, "2690153653");
        assert_eq!(tracks[0].artists, vec!["DECO*27".to_string()]);
        assert_eq!(tracks[0].duration_ms, Some(213013));
        assert_eq!(tracks[0].cover_url.as_deref(), Some("http://p/1.jpg"));
        assert_eq!(tracks[1].album.as_deref(), Some("花骨朵"));
        assert_eq!(tracks[1].artist_ids, vec!["906118".to_string()]);
    }

    #[test]
    fn a_wrapper_without_song_info_is_skipped_not_fatal() {
        // 偶有条目只有壳子（下架 / 无版权）：跳过它，不能把整批判死
        let body = json!({
            "code": 200,
            "data": [
                { "id": 1, "alg": "x" },
                { "id": 2, "songInfo": { "id": 2, "name": "有曲目", "ar": [{"name": "甲"}] } }
            ]
        });
        let tracks = parse_song_array_response(&body).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "有曲目");
    }

    #[test]
    fn intelligence_list_documented_data_result_layout_still_parses() {
        // API.md 没写心动模式的返回结构，`data.result[]` 是上游文档记载的另一种
        // 布局（外加 `data.orderId`）。实测服务端走的是 `data[]` 包装（见上一条
        // 测试），这里只保留兼容探测；`/data/result` 必须排在 `/data` **之前**，
        // 否则 `{"data":{"result":[…]}}` 会先命中 `data` 那个对象、`as_array()`
        // 失败且不再回退，整批被误判成 InvalidData。
        let body = json!({
            "code": 200,
            "data": {
                "orderId": "9f1a2c...",
                "result": [
                    { "id": 33894312, "name": "续曲一号",
                      "ar": [{"id": 91, "name": "歌手甲"}],
                      "al": {"id": 7001, "name": "专辑甲", "picUrl": "https://p/7001.jpg"},
                      "dt": 245000 },
                    { "id": 36871368, "name": "续曲二号",
                      "ar": [{"id": 92, "name": "歌手乙"}],
                      "al": {"id": 7002, "name": "专辑乙"},
                      "dt": 180000 }
                ]
            }
        });
        let tracks = parse_song_array_response(&body).unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].id, "33894312");
        assert_eq!(tracks[0].title, "续曲一号");
        assert_eq!(tracks[0].cover_url.as_deref(), Some("https://p/7001.jpg"));
        assert_eq!(tracks[1].artist_ids, vec!["92".to_string()]);
    }

    #[test]
    fn liked_playlist_is_found_by_special_type_not_by_name() {
        // 「<昵称>喜欢的音乐」会随账号语言、改名、他人撞名失效；specialType=5 才是接口口径
        let body = json!({
            "code": 200,
            "playlist": [
                { "id": 776879259, "name": "阿巴喜欢的音乐", "specialType": 5 },
                { "id": 18425795587i64, "name": "历史音频", "specialType": 0 },
                { "id": 13051831722i64, "name": "阿巴的2024年度歌单", "specialType": 20 }
            ]
        });
        assert_eq!(find_liked_playlist_id(&body).as_deref(), Some("776879259"));
    }

    #[test]
    fn liked_playlist_id_tolerates_string_ids_and_missing_marker() {
        let string_id = json!({ "playlist": [{ "id": "776879259", "specialType": 5 }] });
        assert_eq!(
            find_liked_playlist_id(&string_id).as_deref(),
            Some("776879259")
        );
        // 没有红心标记 / 没有 playlist 节点 → None（让调用方决定兜底，不猜）
        let no_marker = json!({ "playlist": [{ "id": 1, "specialType": 0 }] });
        assert_eq!(find_liked_playlist_id(&no_marker), None);
        assert_eq!(find_liked_playlist_id(&json!({ "code": 200 })), None);
    }

    #[test]
    fn featured_numeric_and_string_ids_are_normalized() {
        let body = json!({
            "data": [ { "id": 42, "name": "数字 ID" }, { "id": "99", "name": "字符串 ID" } ]
        });
        let items = parse_featured_playlists_response(&body).unwrap();
        assert_eq!(items[0].id, "42");
        assert_eq!(items[1].id, "99");
    }

    // ---- 播放地址解析 ---------------------------------------------------

    #[test]
    fn playback_source_parses_url_expi_level_type() {
        let body = json!({
            "data": [{
                "id": 347230,
                "url": "http://m701.music.126.net/xxx.mp3",
                "br": 320000, "code": 200,
                "expi": 1200, "type": "mp3", "level": "exhigh"
            }],
            "code": 200
        });
        let src = parse_playback_source(&body, 1_700_000_000).unwrap();
        assert_eq!(src.url, "http://m701.music.126.net/xxx.mp3");
        assert_eq!(src.expires_at, Some(1_700_000_000 + 1200));
        assert_eq!(src.quality, Some(AudioQuality::ExtraHigh));
        assert_eq!(src.content_type.as_deref(), Some("mp3"));
        assert_eq!(src.bitrate_kbps, Some(320), "接口 br=320000 bps → 320 kbps");
    }

    #[test]
    fn playback_source_bitrate_is_only_the_declared_number() {
        // 无损档接口给的是实测码率（bps，非整千），换算向下取整
        let flac = json!({
            "data": [{ "id": 1, "url": "http://x/a.flac", "code": 200,
                       "br": 962277, "level": "lossless", "type": "flac" }]
        });
        assert_eq!(
            parse_playback_source(&flac, 0).unwrap().bitrate_kbps,
            Some(962)
        );

        // 没给 br / 给了 0 / 给了小于一千的垃圾值 / 给了字符串：一律 None，
        // 不能让起播闸门拿到一个「零码率」或「一百倍码率」。
        for body in [
            json!({ "data": [{ "id": 1, "url": "http://x/a.mp3", "code": 200 }] }),
            json!({ "data": [{ "id": 1, "url": "http://x/a.mp3", "code": 200, "br": 0 }] }),
            json!({ "data": [{ "id": 1, "url": "http://x/a.mp3", "code": 200, "br": 999 }] }),
            json!({ "data": [{ "id": 1, "url": "http://x/a.mp3", "code": 200, "br": "320000" }] }),
        ] {
            assert_eq!(parse_playback_source(&body, 0).unwrap().bitrate_kbps, None);
        }
    }

    #[test]
    fn playback_source_without_expi_does_not_fake_expiry() {
        let body = json!({
            "data": [{ "id": 1, "url": "http://x/a.mp3", "code": 200 }]
        });
        let src = parse_playback_source(&body, 1_700_000_000).unwrap();
        assert_eq!(src.expires_at, None);
        assert_eq!(src.quality, None);
        assert_eq!(src.content_type, None);

        // expi 为 0 / 负数同样视为「无有效期限」，不伪造
        let zero = json!({
            "data": [{ "id": 1, "url": "http://x/a.mp3", "code": 200, "expi": 0 }]
        });
        assert_eq!(
            parse_playback_source(&zero, 1_700_000_000)
                .unwrap()
                .expires_at,
            None
        );
    }

    #[test]
    fn playback_source_null_url_is_playback_url_unavailable() {
        let body = json!({ "data": [{ "id": 1, "url": null, "code": 200 }] });
        assert_eq!(
            parse_playback_source(&body, 0).unwrap_err(),
            ProviderError::PlaybackUrlUnavailable
        );

        // 空 data 数组：结构正常但没有地址条目
        let empty = json!({ "data": [], "code": 200 });
        assert_eq!(
            parse_playback_source(&empty, 0).unwrap_err(),
            ProviderError::PlaybackUrlUnavailable
        );
    }

    #[test]
    fn playback_source_non_200_item_code_is_content_unavailable() {
        // 版权/VIP/地区限制：如实上报不可播，不重试、不绕过
        let body = json!({
            "data": [{ "id": 1, "url": null, "code": 404 }],
            "code": 200
        });
        assert_eq!(
            parse_playback_source(&body, 0).unwrap_err(),
            ProviderError::ContentUnavailable
        );
    }

    #[test]
    fn playback_source_missing_data_node_is_invalid_data() {
        assert_eq!(
            parse_playback_source(&json!({ "code": 200 }), 0).unwrap_err(),
            ProviderError::InvalidData
        );
    }

    #[test]
    fn unknown_quality_level_is_preserved_not_reclassified() {
        let body = json!({
            "data": [{ "id": 1, "url": "http://x/a.flac", "code": 200, "level": "jymaster" }]
        });
        let src = parse_playback_source(&body, 0).unwrap();
        assert_eq!(src.quality, Some(AudioQuality::Other("jymaster".into())));
    }

    // ---- 可播预检解析 ---------------------------------------------------

    #[test]
    fn availability_with_url_is_available() {
        let body = json!({ "data": [{ "id": 1, "url": "http://x/a.mp3", "code": 200 }] });
        let a = parse_availability_response(&body).unwrap();
        assert!(a.is_available());
    }

    #[test]
    fn availability_null_url_carries_interface_hint_not_fabrication() {
        // 接口给了业务码线索 → 保留
        let body = json!({ "data": [{ "id": 1, "url": null, "code": 404 }] });
        assert_eq!(
            parse_availability_response(&body).unwrap(),
            TrackAvailability::Unavailable(Some("接口返回 code=404".into()))
        );

        // 接口给了消息线索 → 优先保留
        let with_msg = json!({
            "data": [{ "id": 1, "url": null, "message": "亲爱的,暂无版权" }]
        });
        assert_eq!(
            parse_availability_response(&with_msg).unwrap(),
            TrackAvailability::Unavailable(Some("亲爱的,暂无版权".into()))
        );

        // 毫无线索 → None，不编造
        let bare = json!({ "data": [{ "id": 1, "url": null }] });
        assert_eq!(
            parse_availability_response(&bare).unwrap(),
            TrackAvailability::Unavailable(None)
        );
    }

    #[test]
    fn availability_empty_data_is_unavailable_missing_node_is_invalid() {
        assert_eq!(
            parse_availability_response(&json!({ "data": [] })).unwrap(),
            TrackAvailability::Unavailable(None)
        );
        assert_eq!(
            parse_availability_response(&json!({ "code": 200 })).unwrap_err(),
            ProviderError::InvalidData
        );
    }

    // ---- 错误分类与重试策略（规范 8.2 / 8.3）---------------------------

    #[test]
    fn retry_policy_matches_spec_8_3() {
        assert!(ProviderError::RequestTimeout.is_retryable());
        assert!(ProviderError::NetworkUnavailable.is_retryable());
        assert!(
            ProviderError::DataProviderUnavailable.is_retryable(),
            "数据服务暂不可用属于瞬时故障"
        );
        assert!(
            !ProviderError::RateLimited.is_retryable(),
            "限流应遵守服务端建议，不自动重试"
        );
        assert!(!ProviderError::InvalidParameter.is_retryable());
        assert!(!ProviderError::AuthenticationRequired.is_retryable());
        assert!(!ProviderError::AuthenticationExpired.is_retryable());
        assert!(!ProviderError::EmptyResult.is_retryable());
        assert!(
            !ProviderError::ContentUnavailable.is_retryable(),
            "版权/VIP/地区限制不重试、不绕过"
        );
        assert!(!ProviderError::PlaybackUrlUnavailable.is_retryable());
        assert!(!ProviderError::UnsupportedResponse.is_retryable());
        assert!(!ProviderError::Cancelled.is_retryable());
    }

    #[test]
    fn connection_reset_is_retryable_and_distinct_from_network_unavailable() {
        // 连接中途断开 ≠ 网络不可用：前者重试即好，后者要用户查网络。
        // 文案必须能区分，否则用户会去排查根本没坏的网络。
        assert!(
            ProviderError::ConnectionReset.is_retryable(),
            "连接被对端关闭属于瞬时故障，应允许有限重试"
        );
        assert_ne!(
            ProviderError::ConnectionReset.to_string(),
            ProviderError::NetworkUnavailable.to_string()
        );
        assert!(
            !ProviderError::ConnectionReset
                .to_string()
                .contains("网络不可用"),
            "不能说成网络不可用"
        );
    }

    #[test]
    fn errors_display_is_human_readable_not_json() {
        for e in [
            ProviderError::NetworkUnavailable,
            ProviderError::ConnectionReset,
            ProviderError::RequestTimeout,
            ProviderError::EmptyResult,
            ProviderError::InvalidData,
            ProviderError::ContentUnavailable,
            ProviderError::PlaybackUrlUnavailable,
            ProviderError::UnsupportedResponse,
            ProviderError::DataProviderUnavailable,
        ] {
            let s = e.to_string();
            assert!(!s.contains('{'), "错误提示不应暴露 JSON: {}", s);
            assert!(!s.to_lowercase().contains("reqwest"));
        }
    }

    #[test]
    fn availability_display_is_human_readable() {
        assert_eq!(TrackAvailability::Available.to_string(), "可播放");
        assert_eq!(TrackAvailability::Unavailable(None).to_string(), "不可播放");
        assert_eq!(
            TrackAvailability::Unavailable(Some("code=404".into())).to_string(),
            "不可播放（code=404）"
        );
    }

    // ---- 歌词解析（阶段 D）---------------------------------------------

    #[test]
    fn lyric_response_prefers_word_by_word() {
        let body = json!({
            "yrc": { "lyric": "[100,200](100,50,0)词(150,50,0)句" },
            "lrc": { "lyric": "[00:01.00]普通行" }
        });
        let lyrics = parse_lyric_response(&body).unwrap();
        assert_eq!(lyrics.kind, LyricKind::WordByWord);
        assert_eq!(lyrics.lines[0].text, "词句");
    }

    #[test]
    fn lyric_response_falls_back_to_plain_when_yrc_unparsable() {
        let body = json!({
            "yrc": { "lyric": "完全不是歌词格式" },
            "lrc": { "lyric": "[00:01.00]普通行\n[00:02.00]第二行" }
        });
        let lyrics = parse_lyric_response(&body).unwrap();
        assert_eq!(lyrics.kind, LyricKind::Plain);
        assert_eq!(lyrics.lines.len(), 2);
        assert_eq!(lyrics.lines[0].text, "普通行");
    }

    #[test]
    fn lyric_response_falls_back_when_yrc_absent() {
        let body = json!({ "lrc": { "lyric": "[00:01.00]普通行" } });
        let lyrics = parse_lyric_response(&body).unwrap();
        assert_eq!(lyrics.kind, LyricKind::Plain);
    }

    #[test]
    fn lyric_response_missing_lyrics_is_empty_result() {
        let body = json!({ "lrc": { "lyric": "" }, "yrc": { "lyric": "" } });
        assert!(matches!(
            parse_lyric_response(&body),
            Err(ProviderError::EmptyResult)
        ));

        let body2 = json!({ "code": 200 });
        assert!(matches!(
            parse_lyric_response(&body2),
            Err(ProviderError::EmptyResult)
        ));
    }

    #[test]
    fn lyric_response_non_object_is_unsupported() {
        let body = json!("<html>error page</html>");
        assert!(matches!(
            parse_lyric_response(&body),
            Err(ProviderError::UnsupportedResponse)
        ));
    }

    // ---- 对象安全视图 DynMusicProvider（阶段 E：接口统一、可替换）------

    /// 测试用 Mock Provider：脚本化返回或错误，验证「更换实现不改调用方」。
    struct MockProvider {
        label: &'static str,
    }

    impl MusicProvider for MockProvider {
        fn id(&self) -> ProviderId {
            ProviderId::Netease
        }

        async fn search_songs(
            &self,
            keyword: &str,
            limit: u32,
        ) -> Result<Vec<Track>, ProviderError> {
            if self.label == "broken" {
                return Err(ProviderError::DataProviderUnavailable);
            }
            Ok(vec![Track {
                provider: ProviderId::Netease,
                id: format!("mock-{}", keyword),
                title: format!("{}({})", keyword, self.label),
                artists: vec![self.label.to_string()],
                album: None,
                duration_ms: Some(1000 * u64::from(limit)),
                cover_url: None,
                ..Track::default()
            }])
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
                    title: format!("detail-{}", self.label),
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
            limit: u32,
        ) -> Result<Playlist, ProviderError> {
            if self.label == "broken" {
                return Err(ProviderError::RequestTimeout);
            }
            Ok(Playlist {
                provider: ProviderId::Netease,
                id: playlist_id.to_string(),
                name: Some(format!("list-{}", self.label)),
                total: Some(1),
                tracks: (0..limit)
                    .map(|i| Track {
                        provider: ProviderId::Netease,
                        id: format!("{}-{}", playlist_id, i),
                        title: format!("t{}", i),
                        artists: vec![],
                        album: None,
                        duration_ms: None,
                        cover_url: None,
                        ..Track::default()
                    })
                    .collect(),
            })
        }
        async fn daily_songs(&self) -> Result<Vec<Track>, ProviderError> {
            if self.label == "broken" {
                return Err(ProviderError::RequestTimeout);
            }
            Ok(vec![Track {
                provider: ProviderId::Netease,
                id: "daily-1".into(),
                title: "daily".into(),
                artists: vec!["a".into()],
                ..Track::default()
            }])
        }
        async fn intelligence_list(
            &self,
            song_id: &str,
            _playlist_id: &str,
            _start_id: &str,
            count: u32,
        ) -> Result<Vec<Track>, ProviderError> {
            if self.label == "broken" {
                return Err(ProviderError::RequestTimeout);
            }
            if song_id.is_empty() {
                return Err(ProviderError::InvalidParameter);
            }
            Ok((0..count.min(3))
                .map(|i| Track {
                    provider: ProviderId::Netease,
                    id: format!("intel-{i}"),
                    title: format!("intel-{i}"),
                    artists: vec!["a".into()],
                    ..Track::default()
                })
                .collect())
        }
        async fn search_match(
            &self,
            title: &str,
            _album: &str,
            _artist: &str,
            _duration_secs: u64,
            md5: &str,
        ) -> Result<Vec<Track>, ProviderError> {
            if title.trim().is_empty() || md5.trim().is_empty() {
                return Err(ProviderError::InvalidParameter);
            }
            Ok(vec![Track {
                provider: ProviderId::Netease,
                id: "matched-1".into(),
                title: title.into(),
                artists: vec!["a".into()],
                cover_url: Some("https://pic/matched-1.jpg".into()),
                ..Track::default()
            }])
        }

        async fn featured_playlists(
            &self,
            kind: FeaturedKind,
            limit: u32,
        ) -> Result<Vec<PlaylistSummary>, ProviderError> {
            if self.label == "broken" {
                return Err(ProviderError::RequestTimeout);
            }
            if limit == 0 {
                return Err(ProviderError::InvalidParameter);
            }
            Ok(vec![PlaylistSummary {
                provider: ProviderId::Netease,
                id: "7001".into(),
                name: format!("feat-{:?}-{}", kind, self.label),
                cover_url: None,
                track_count: Some(1),
                play_count: None,
                description: None,
            }])
        }

        async fn check_availability(
            &self,
            _song_id: &str,
        ) -> Result<TrackAvailability, ProviderError> {
            Ok(TrackAvailability::Available)
        }

        async fn playback_source(&self, _song_id: &str) -> Result<PlaybackSource, ProviderError> {
            Err(ProviderError::PlaybackUrlUnavailable)
        }

        async fn lyrics(&self, _song_id: &str) -> Result<Lyrics, ProviderError> {
            Err(ProviderError::EmptyResult)
        }

        async fn login_status(&self) -> Result<LoginState, ProviderError> {
            Ok(LoginState::Anonymous)
        }

        async fn qr_login_start(&self) -> Result<QrLoginSession, ProviderError> {
            Ok(QrLoginSession {
                key: format!("key-{}", self.label),
                qr_url: format!("https://music.163.com/login?codekey={}", self.label),
            })
        }

        async fn qr_login_poll(&self, key: &str) -> Result<LoginState, ProviderError> {
            if key.is_empty() {
                return Err(ProviderError::InvalidParameter);
            }
            Ok(match self.label {
                "broken" => LoginState::QrExpired,
                "scanned" => LoginState::QrScanned,
                "logged" => LoginState::LoggedIn {
                    nickname: Some("MockUser".into()),
                    user_id: Some("42".into()),
                },
                _ => LoginState::QrWaiting,
            })
        }

        async fn login_cellphone(
            &self,
            phone: &str,
            _password: &str,
        ) -> Result<LoginState, ProviderError> {
            if phone.is_empty() {
                return Err(ProviderError::InvalidParameter);
            }
            Ok(LoginState::LoggedIn {
                nickname: Some(format!("phone-{}", self.label)),
                user_id: Some("42".into()),
            })
        }

        async fn login_cookie(&self, cookie: &str) -> Result<LoginState, ProviderError> {
            if cookie.is_empty() {
                return Err(ProviderError::InvalidParameter);
            }
            Ok(LoginState::LoggedIn {
                nickname: Some(format!("cookie-{}", self.label)),
                user_id: Some("42".into()),
            })
        }

        async fn logout(&self) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn dyn_view_dispatches_like_static_dispatch() {
        let mock = MockProvider { label: "a" };
        // 同一实现分别走静态（泛型）与动态（dyn）分发，结果一致
        let static_result = MusicProvider::search_songs(&mock, "x", 3).await.unwrap();
        let dyn_view: &dyn DynMusicProvider = &mock;
        let dyn_result = dyn_view.search_songs("x", 3).await.unwrap();
        assert_eq!(static_result, dyn_result);
        assert_eq!(dyn_view.id(), ProviderId::Netease);
    }

    #[tokio::test]
    async fn dyn_view_swaps_provider_without_touching_caller() {
        // 调用方只认 Arc<dyn DynMusicProvider>；换实现 = 换 Arc 内容
        async fn caller(p: &std::sync::Arc<dyn DynMusicProvider>) -> String {
            p.search_songs("kw", 1).await.unwrap()[0].title.clone()
        }

        let a: std::sync::Arc<dyn DynMusicProvider> =
            std::sync::Arc::new(MockProvider { label: "a" });
        let b: std::sync::Arc<dyn DynMusicProvider> =
            std::sync::Arc::new(MockProvider { label: "b" });

        assert_eq!(caller(&a).await, "kw(a)");
        assert_eq!(caller(&b).await, "kw(b)");
    }

    #[tokio::test]
    async fn dyn_view_propagates_typed_errors_not_raw_json() {
        let broken: std::sync::Arc<dyn DynMusicProvider> =
            std::sync::Arc::new(MockProvider { label: "broken" });
        assert_eq!(
            broken.search_songs("kw", 1).await.unwrap_err(),
            ProviderError::DataProviderUnavailable
        );
        assert_eq!(
            broken.playlist("1", 0, 5).await.unwrap_err(),
            ProviderError::RequestTimeout
        );
        // 参数校验路径同样走统一错误分类
        let ok: std::sync::Arc<dyn DynMusicProvider> =
            std::sync::Arc::new(MockProvider { label: "a" });
        assert_eq!(
            ok.song_details(&[]).await.unwrap_err(),
            ProviderError::InvalidParameter
        );
    }

    // ---- 登录（阶段 G：二维码）----------------------------------------

    #[test]
    fn qr_key_response_extracts_unikey() {
        let body = json!({ "code": 200, "unikey": "abc-123" });
        assert_eq!(parse_qr_key_response(&body).unwrap(), "abc-123");
    }

    #[test]
    fn qr_key_missing_or_blank_unikey_is_invalid_data() {
        assert_eq!(
            parse_qr_key_response(&json!({ "code": 200 })).unwrap_err(),
            ProviderError::InvalidData
        );
        assert_eq!(
            parse_qr_key_response(&json!({ "unikey": "  " })).unwrap_err(),
            ProviderError::InvalidData
        );
        assert_eq!(
            parse_qr_key_response(&json!({ "unikey": 42 })).unwrap_err(),
            ProviderError::InvalidData
        );
    }

    #[test]
    fn qr_check_waiting_scanned_expired_map_to_states() {
        assert_eq!(
            parse_qr_check_response(&json!({ "code": 801 }), &[])
                .unwrap()
                .state,
            LoginState::QrWaiting
        );
        assert_eq!(
            parse_qr_check_response(&json!({ "code": 802 }), &[])
                .unwrap()
                .state,
            LoginState::QrScanned
        );
        assert_eq!(
            parse_qr_check_response(&json!({ "code": 800 }), &[])
                .unwrap()
                .state,
            LoginState::QrExpired
        );
    }

    #[test]
    fn qr_check_success_with_cookie_is_logged_in() {
        let body = json!({
            "code": 803,
            "nickname": "测试用户",
            "userId": 12345
        });
        let cookies = vec!["MUSIC_R_T=secret-token; Max-Age=2147483647; Path=/".to_string()];
        let outcome = parse_qr_check_response(&body, &cookies).unwrap();
        assert_eq!(
            outcome.state,
            LoginState::LoggedIn {
                nickname: Some("测试用户".into()),
                user_id: Some("12345".into()),
            }
        );
        assert_eq!(
            outcome.cookie.as_deref(),
            Some("MUSIC_R_T=secret-token"),
            "应提取 name=value 对并丢弃属性段"
        );
    }

    #[test]
    fn qr_check_success_without_cookie_is_invalid_data() {
        // 没有会话材料的「登录成功」不可信
        let body = json!({ "code": 803, "nickname": "x" });
        assert_eq!(
            parse_qr_check_response(&body, &[]).unwrap_err(),
            ProviderError::InvalidData
        );
    }

    #[test]
    fn qr_check_takes_cookie_from_body_when_headers_are_empty() {
        // 回归：网易云 803 常把 cookie 放在响应体字段里，响应头一个 Set-Cookie
        // 都没有。只认响应头会判成「登录成功但没有会话材料」→ 手机已授权、
        // 程序仍显示未登录。
        let body = json!({
            "code": 803,
            "nickname": "耳朵",
            "cookie": "MUSIC_U=abc123; __csrf=def456"
        });
        let outcome = parse_qr_check_response(&body, &[]).expect("body.cookie 应被采用");
        assert_eq!(
            outcome.state,
            LoginState::LoggedIn {
                nickname: Some("耳朵".into()),
                user_id: None,
            }
        );
        assert_eq!(
            outcome.cookie.as_deref(),
            Some("MUSIC_U=abc123; __csrf=def456")
        );
    }

    #[test]
    fn qr_check_takes_cookie_from_nested_data() {
        let body = json!({ "code": 803, "data": { "cookie": "MUSIC_U=zzz" } });
        let outcome = parse_qr_check_response(&body, &[]).expect("data.cookie 应被采用");
        assert_eq!(outcome.cookie.as_deref(), Some("MUSIC_U=zzz"));
    }

    #[test]
    fn qr_check_prefers_header_cookie_over_body() {
        // 两条都有时以响应头为准（它已去掉属性段，格式更干净）
        let body = json!({ "code": 803, "cookie": "MUSIC_U=from-body" });
        let cookies = vec!["MUSIC_U=from-header; Path=/".to_string()];
        let outcome = parse_qr_check_response(&body, &cookies).unwrap();
        assert_eq!(outcome.cookie.as_deref(), Some("MUSIC_U=from-header"));
    }

    #[test]
    fn qr_check_unknown_code_is_invalid_data() {
        assert_eq!(
            parse_qr_check_response(&json!({ "code": 999 }), &[]).unwrap_err(),
            ProviderError::InvalidData
        );
        assert_eq!(
            parse_qr_check_response(&json!({ "msg": "no code" }), &[]).unwrap_err(),
            ProviderError::InvalidData
        );
    }

    #[test]
    fn login_status_null_profile_is_anonymous_not_error() {
        assert_eq!(
            parse_login_status_response(&json!({ "profile": null })).unwrap(),
            LoginState::Anonymous
        );
        assert_eq!(
            parse_login_status_response(&json!({ "data": {} })).unwrap(),
            LoginState::Anonymous
        );
    }

    #[test]
    fn login_status_profile_maps_to_logged_in() {
        let body = json!({
            "profile": { "nickname": "Alice", "userId": 777 }
        });
        assert_eq!(
            parse_login_status_response(&body).unwrap(),
            LoginState::LoggedIn {
                nickname: Some("Alice".into()),
                user_id: Some("777".into()),
            }
        );
    }

    #[test]
    fn cellphone_login_success_returns_state_and_cookie() {
        let body = json!({
            "code": 200,
            "profile": { "nickname": "Bob", "userId": 888 },
            "cookie": "MUSIC_U=phone-cookie; __csrf=csrf"
        });
        let (state, cookie) = parse_cellphone_login_response(&body, &[]).unwrap();
        assert_eq!(
            state,
            LoginState::LoggedIn {
                nickname: Some("Bob".into()),
                user_id: Some("888".into()),
            }
        );
        assert_eq!(cookie.as_deref(), Some("MUSIC_U=phone-cookie; __csrf=csrf"));
    }

    #[test]
    fn cellphone_login_failure_codes_map_to_readable_errors() {
        // 502 密码错误 → 可读 Unknown（不回 InvalidData，也不重试）
        assert!(matches!(
            parse_cellphone_login_response(&json!({ "code": 502, "message": "密码错误" }), &[]),
            Err(ProviderError::Unknown(_))
        ));
        // 503 频繁请求 → RateLimited
        assert_eq!(
            parse_cellphone_login_response(&json!({ "code": 503 }), &[]).unwrap_err(),
            ProviderError::RateLimited
        );
        // 400 参数错误
        assert_eq!(
            parse_cellphone_login_response(&json!({ "code": 400 }), &[]).unwrap_err(),
            ProviderError::InvalidParameter
        );
    }

    #[test]
    fn cellphone_login_missing_code_is_invalid_data() {
        assert_eq!(
            parse_cellphone_login_response(&json!({ "profile": {} }), &[]).unwrap_err(),
            ProviderError::InvalidData
        );
    }

    #[test]
    fn join_set_cookie_takes_first_pair_and_skips_junk() {
        let parts = vec![
            "MUSIC_A_T=token-a; Max-Age=999; Path=/".to_string(),
            "MUSIC_U_T=token-b; Path=/".to_string(),
            "   ".to_string(),
            "noequals".to_string(),
        ];
        assert_eq!(
            join_set_cookie(&parts).as_deref(),
            Some("MUSIC_A_T=token-a; MUSIC_U_T=token-b")
        );
        assert_eq!(join_set_cookie(&[]), None);
        assert_eq!(join_set_cookie(&["=v".to_string()]), None, "空名无效");
        assert_eq!(join_set_cookie(&["k=".to_string()]), None, "空值无效");
    }

    #[test]
    fn qr_poll_outcome_debug_redacts_cookie() {
        let outcome =
            QrPollOutcome::success("MUSIC_R_T=super-secret".into(), Some("Bob".into()), None);
        let dbg = format!("{:?}", outcome);
        assert!(
            !dbg.contains("super-secret"),
            "Debug 不得泄露 cookie: {}",
            dbg
        );
        assert!(dbg.contains("<redacted>"), "应标注脱敏: {}", dbg);
        assert!(dbg.contains("Bob"), "非凭据字段应可见: {}", dbg);
    }

    #[test]
    fn login_state_display_is_human_readable() {
        assert_eq!(LoginState::Anonymous.to_string(), "未登录");
        assert_eq!(LoginState::QrWaiting.to_string(), "等待扫码");
        assert_eq!(LoginState::QrScanned.to_string(), "已扫码，待确认");
        assert_eq!(LoginState::QrExpired.to_string(), "二维码已过期");
        assert_eq!(
            LoginState::LoggedIn {
                nickname: Some("小明".into()),
                user_id: Some("1".into())
            }
            .to_string(),
            "已登录：小明"
        );
        assert_eq!(
            LoginState::LoggedIn {
                nickname: None,
                user_id: None
            }
            .to_string(),
            "已登录"
        );
    }

    #[tokio::test]
    async fn dyn_view_login_roundtrip() {
        let logged: std::sync::Arc<dyn DynMusicProvider> =
            std::sync::Arc::new(MockProvider { label: "logged" });
        let start = logged.qr_login_start().await.unwrap();
        assert_eq!(start.key, "key-logged");
        assert!(start.qr_url.contains("codekey=logged"));
        let state = logged.qr_login_poll(&start.key).await.unwrap();
        assert!(matches!(state, LoginState::LoggedIn { .. }));
        assert_eq!(
            logged.qr_login_poll("").await.unwrap_err(),
            ProviderError::InvalidParameter
        );
    }
}
