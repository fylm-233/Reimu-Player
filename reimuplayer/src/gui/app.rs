//! Reimu Player 窗口五页装配（Player / Search / Rec / Daily / Playlist）与播放编排接线。
//!
//! 布局参考 `code (3).html`（SONORA — System 6 / Y2K Tape Deck）：
//! 窗口标题栏 → 居中页面导航 → 各页内容；页内无菜单栏/页脚等外围装饰。
//!
//! 交互与状态（规范 §6 线程模型 / §8 状态机）：
//! - 播放编排全部走 [`PlayerCore`]（PlaySession generation 防过期 + 临时文件回收）；
//! - 网络请求经 [`ProviderBridge`] 异步派发，**UI 线程零阻塞**，音频回调零网络；
//! - 歌词时间轴由 `Lyrics::line_at`（实际播放时钟）驱动，点击行 seek；
//! - 取消：提升 generation + 桥中断在途下载（按钮或换台自动）；
//! - 本地文件（输入路径 / 拖放）独立可用，不触网。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, Layout, Margin, Pos2, Rect, Sense, Stroke,
    StrokeKind, TextureHandle, TextureId, TextureOptions, Ui, UiBuilder, Vec2,
};

use super::bridge::{ProviderBridge, Response};
use super::deck_seq;
use super::desktop_lyric::LyricFrame;
use super::halftone;
use super::icons;
use super::library;
use super::login::{self, LoginAction, LoginMode, LoginUi};
use super::lyric_cache::LyricsCache;
use super::perf_log;
use super::player_core::{CoreEvent, PlayMode, PlayerCore, RepeatMode, TickAction};
use super::qr;
#[cfg_attr(test, allow(unused_imports))]
use super::session_store;
use super::settings::{self, SettingsAction, SettingsView};
use super::theme::{self, *};
use super::widgets::{
    self, with_opacity, CassetteView, ControlSpec, DeckView, HotRow, LyricRow, ResRow, RowAction,
    SpineView,
};
use super::window::{self, CloseAnim, WindowAction};
use crate::audio::RodioEngine;
use crate::i18n::{pick, Lang};
use crate::music::link::{parse_share, ShareKind, ShareRef};
use crate::music::load_state::LoadState;
use crate::music::models::format_duration;
use crate::music::provider::{
    DynMusicProvider, FeaturedKind, LoginState, Playlist, PlaylistSummary, ProviderError,
    ProviderId, QualityCeiling, Track,
};
use crate::music::DynAudioFetcher;
use crate::providers::ProviderTimeouts;

/// 当前页面（窗口内导航）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Page {
    Player,
    Search,
    Rec,
    /// 每日推荐（网易云 `/api/v3/discovery/recommend/songs`，需登录）。
    ///
    /// 注意与「每日**歌单**」区分：那个接口（`/recommend/resource`）给的是
    /// 30 张歌单的摘要，点开还要再取曲目；本页要的是**歌曲本身**——
    /// 进入即整列表展开，不点任何东西就能看到当天推荐的全部曲目。
    Daily,
    Playlist,
}

impl Page {
    /// 窗口标题（code (3).html `winTitles`：随页面切换）。
    ///
    /// 品牌词 `crate::BRAND`（Reimu Player）中英文都保留原样，后面接页面名。
    /// 五串都是 `&'static`，所以品牌写死而不用 `format!`——漂移由
    /// [`tests::every_page_title_carries_the_brand`] 盯住。
    fn title(&self, lang: Lang) -> &'static str {
        match self {
            Page::Player => pick(lang, "Reimu Player 音乐播放器", "REIMU PLAYER AUDIO PLAYER"),
            Page::Search => pick(lang, "Reimu Player 搜索", "REIMU PLAYER SEARCH"),
            Page::Rec => pick(lang, "Reimu Player 推荐", "REIMU PLAYER RECOMMENDED"),
            Page::Daily => pick(
                lang,
                "Reimu Player 每日推荐",
                "REIMU PLAYER DAILY RECOMMEND",
            ),
            Page::Playlist => pick(lang, "Reimu Player 歌单", "REIMU PLAYER PLAYLISTS"),
        }
    }

    /// 导航页签短名（贴标题栏下方那一排）。中文取两字以内，
    /// 五个页签在 720px 最小窗口宽下仍能整排居中排开。
    fn nav_label(&self, lang: Lang) -> &'static str {
        match self {
            Page::Player => pick(lang, "播放", "Player"),
            Page::Search => pick(lang, "搜索", "Search"),
            Page::Rec => pick(lang, "推荐", "Rec"),
            Page::Daily => pick(lang, "每日", "Daily"),
            Page::Playlist => pick(lang, "歌单", "Playlist"),
        }
    }
}

/// 搜索/歌单请求页大小。
const PAGE_LIMIT: u32 = 30;

/// 已载入歌单历史的上限（超出淘汰最久未打开的一张）。
const PLAYLIST_HISTORY_LIMIT: usize = 12;

/// 每日推荐跨天检查的最小间隔（秒）。
///
/// 检查本身要读一次系统本地时间（`GetLocalTime`），没必要每帧打；
/// 5 秒粒度对"跨天自动换新"完全够用。
const DAILY_CHECK_SECS: f32 = 5.0;

/// Toast 停留时长（秒）。1.4s 太短，连读两句都来不及。
const TOAST_SECS: f32 = 2.2;

/// 键盘 seek 步长（秒）。
const SEEK_STEP_SECS: f32 = 5.0;

/// 键盘/滚轮音量步长。
const VOLUME_STEP: f32 = 0.05;

/// 封面点阵纹理边长（px）。磁带舱与歌单卡都按此尺寸采样后缩放显示，
/// 保证 halftone 网点密度一致（§3.8 2.5px 网点）。
/// 实际值定义在 [`halftone::COVER_PX`]：后台线程烘纹理时也要用同一个数。
const COVER_PX: usize = halftone::COVER_PX;

/// 封面纹理缓存上限（**LRU 逐条淘汰**，防止长时间浏览无限增长）。
///
/// 上限必须**大于单帧内被绘制的最大封面数**。歌单页的卡片列表不是虚拟化渲染
/// （`ScrollArea` 会跑完整段闭包），263 张账号歌单的封面每帧都会被要一次；
/// 上限 192 时缓存永远装不下，LRU 只好淘汰本帧刚画过的条目——下一帧它又变回
/// 占位图并重新拉取，实测稳态 510 次淘汰/秒，就是用户报的「封面与占位图来回
/// 交错闪烁」。512 张 × 36KB ≈ 18MB 显存，覆盖现有全部列表（歌单 263、
/// 搜索 50）还留了余量。
const COVER_CACHE_LIMIT: usize = 512;

/// 封面在途请求上限——刻意低于封面池总槽位（4 道 × 每道 16 = 64）。
///
/// 为什么要在**发请求之前**限流：槽位满了以后多出来的请求会被桥立刻回执
/// `Cancelled`，UI 按「取消必须销账」把 URL 从在途表摘掉，于是下一帧又重发、
/// 又被取消——玩家看到的就是同一行的真封面和占位图逐帧来回交错。限流让多出来
/// 的封面安静地等下一帧，而不是把队列挤爆。
const COVER_INFLIGHT_LIMIT: usize = 32;

/// 二维码扫码状态轮询间隔（秒）。网易云建议 2s，过快会被限流。
const QR_POLL_SECS: f32 = 2.0;

/// 连续轮询失败的上限。单次网络抖动不该让轮询永久停摆（用户扫了码却
/// 因为一次超时再也收不到结果，是「扫码无法登录」的典型成因）。
/// 超过这个次数才停止轮询并把错误留在面板上。
const QR_POLL_MAX_FAILS: u32 = 5;

/// 二维码模块边长的**上限**（px）。
///
/// 实际值按面板框宽反算（见 [`qr_module_px`]）：取能整除框宽的最大整数，
/// 使纹理能 1:1 落在框里、模块宽度不出现 3px/4px 混排（混排会让手机
/// 摄像头误读，表现为"二维码看着正常但扫不出来"）。
const QR_MODULE_PX_MAX: usize = 6;

/// 按二维码模块数反算模块像素边长。
///
/// `modules` 是二维码本体边长（不含静区）；静区由 [`qr::qr_image`] 自己加 4 模块。
/// 返回值保证 `(modules + 8) * px <= 框宽`，即纹理不缩放就能放进面板。
fn qr_module_px(modules: usize) -> usize {
    let total = modules + 8; // 4 模块静区 ×2
    let box_side = login::QR_BOX_SIDE as usize;
    (box_side / total).clamp(1, QR_MODULE_PX_MAX)
}

/// 页面导航按钮间距（px）。七个页签下 8px 会顶到最小窗宽，收到 6px。
const NAV_GAP: f32 = 6.0;

/// 曲目信息面板的单行行高（px）：11.5px 正文 + 上下各 ~3px 呼吸。
const INFO_ROW_H: f32 = 17.0;

/// 本地检索命中上限：超出部分只给计数，不进列表（避免一次搜索塞进上万行）。
const LOCAL_HIT_LIMIT: usize = 50;

/// 「最近播放」默认时间窗（小时）。
const HISTORY_HOURS_DEFAULT: i64 = 24;

/// 离线每日推荐条数。
const DAILY_OFFLINE_LIMIT: usize = 12;

/// 歌手页最多取多少首（接口按 `limit` 返回热门曲目）。
///
/// 50 是网易云歌手页一屏的量级；再多磁带架也翻不完，还白占内存。
const ARTIST_SONG_LIMIT: u32 = 50;

/// 账号歌单一次拉多少个（网易云「我的歌单」页默认一屏的量级）。
const USER_PLAYLIST_LIMIT: u32 = 50;

/// 收藏同步时一次取多少首详情。
///
/// 网易云 `song/detail` 支持批量 ids，但 URL 长度有限；200 首约 4KB query，
/// 稳在服务端限制之内，也让进度提示不会卡太久。
const SYNC_DETAILS_CHUNK: usize = 200;

/// 心动模式一次要几首（接口 `count`）。
///
/// **必须显式传**：接口默认 `count=1`，不传就是"点了心动只换一首就没下文"。
/// 20 首 ≈ 80 分钟的续曲量，够撑过几次网络抖动又不至于把内存里的队列撑胖。
const HEARTBEAT_BATCH: u32 = 20;

/// 队列游标距心动批次末尾不足几首时提前续批。
const HEARTBEAT_LOOKAHEAD: usize = 5;

/// 连续几批要不到新曲目就收手。
///
/// 智能播放接口对匿名/低信任账号会返回空数组；没这个上限，"每收到一次空批次
/// 就再要一次"会变成一个对着限流接口打转的循环。
const HEARTBEAT_MAX_STALL: u8 = 3;

/// Playlist 页的内容来源（用户数据视图）。
///
/// 只描述「页面在看哪一份用户数据」；列表内容由 `current_tracks()` 按此取值。
/// 与在线歌单 / 本地曲库 / FM 互斥——同一时刻只有一个来源为真，播放列表才
/// 不会与界面显示的内容错位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum UserView {
    /// 未选中（沿用在线歌单 / 本地曲库 / FM 的既有逻辑）。
    #[default]
    None,
    /// 我的收藏。
    Favorites,
    /// 最近播放。
    History,
    /// 自建歌单（按 id）。
    LocalPlaylist(u64),
    /// 专辑曲目（磁带架复用，标题「专辑曲子」）。内容取 [`ReimuApp::album_tracks`]。
    Album,
    /// 艺人曲目（磁带架复用，标题「歌手曲子」）。内容取 [`ReimuApp::artist_tracks`]。
    Artist,
}

/// 磁带架当前在看的专辑/艺人（`UserView::Album` / `UserView::Artist` 时有效）。
///
/// 为什么把 id/名字放在独立字段而不是塞进 [`UserView`]：`UserView` 是 `Copy`
/// 的小枚举，`current_tracks()` 等几十处按值 match；一旦塞进 `String` 就会
/// 逼着全部改成按引用匹配。内容与「看的是谁」分开存，代价只是一个
/// `Option`，收益是整个视图机保持简单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShelfTarget {
    /// 专辑还是艺人。
    pub kind: ShelfKind,
    /// 网易云 ID（跳转与请求都用它）。
    pub id: String,
    /// 展示名（磁带架副标题 / 返回提示）。
    pub name: String,
}

/// [`ShelfTarget`] 的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShelfKind {
    /// 专辑页。
    Album,
    /// 艺人页。
    Artist,
}

/// 二次确认要执行的动作（确认后消费，不依赖计时器）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfirmAction {
    /// 删除自建歌单。
    DeleteLocalPlaylist(u64),
    /// 把某张歌单移出「已载入」历史。
    RemoveLoadedPlaylist(String),
    /// 从账号歌单列表隐藏（仅本地，不动云端）。
    HideAccountPlaylist(String),
}

/// 待确认的危险动作。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfirmDialog {
    title: String,
    message: String,
    confirm_label: String,
    action: ConfirmAction,
}

/// 精选歌单原地展开详情的位置（推荐页两列）。
///
/// 每日页**不在此列**：它展示的是「每日推荐歌曲」本身，进入页面就整列表展开，
/// 没有任何"点开卡片才看到曲目"的折叠层级（那是每日**歌单**的交互，
/// 与产品要的"每日推荐歌曲"不是一回事）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeekPanel {
    /// 左列「推荐歌单」。
    Rec,
    /// 右列「官方排行榜」（热榜）。
    Chart,
}

impl PeekPanel {
    /// 滚动区 id：各面板同时展开时各滚各的。
    fn scroll_salt(self) -> &'static str {
        match self {
            PeekPanel::Rec => "rec-peek-shelf",
            PeekPanel::Chart => "chart-peek-shelf",
        }
    }
}

/// 精选歌单的**原地展开**详情。
///
/// 与「已载入歌单」是两回事：点开展开只在这个面板里取一份曲目给用户看，
/// **不写进 [`ReimuApp::playlists`]**——否则点一下推荐/每日卡，歌单页就多出一张卡，
/// 用户会以为自己新建了一张同名歌单。收起时保留已取到的曲目，再点同一张卡
/// 可以立刻展开，不用重发请求。
#[derive(Debug)]
pub(crate) struct PlaylistPeek {
    /// 歌单 ID（请求与去重都用它）。
    pub id: String,
    /// 页头下方的副标题（歌单名 · 曲目数 · 更新频率/推荐语）。
    pub subtitle: String,
    /// 本次曲目请求的 seq（响应按它归位）。
    pub seq: u64,
    /// 曲目加载状态（只持有一份数据）。
    pub state: LoadState<Playlist>,
    /// 是否正展开（收起后保留数据）。
    pub expanded: bool,
}

/// 原地详情这一帧被点了什么（渲染只读，动作交给调用方落地）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeekHit {
    None,
    Play(usize),
    Back,
}

/// 进入专辑/艺人页之前的磁带架状态（返回时原样恢复）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ShelfSnapshot {
    /// 原来的视图来源。
    pub view: UserView,
    /// 原来选中的行（`None` = 什么都没选）。
    pub selected: Option<usize>,
    /// 原来是否在看本地曲库。
    pub active_local: bool,
    /// 原来加载的在线歌单。
    pub active_playlist: Option<usize>,
}

/// 收藏双向同步的状态机（登录后自动跑一轮）。
///
/// 方向一（云端 → 本地）：拉 `/api/song/like/get` 的 ID 列表，差集里本地没有的
/// 批量取详情后补进本地收藏。
/// 方向二（本地 → 云端）：本地收藏里在线曲目、但云端没有的，逐首 `song_like`。
///
/// 为什么要状态机而不是一个 `async` 流程：所有网络都在 [`super::bridge`] 的
/// 工作线程里，UI 侧只能靠 seq 把响应接起来。四个 seq 各管一段，
/// `push_queue` 逐首串行推送（并发打接口容易被限流）。
#[derive(Debug, Default)]
pub(crate) struct FavoriteSync {
    /// 拉取云端 ID 列表的在途 seq（0 = 未在途）。
    ids_seq: u64,
    /// 批量取详情（补全云端新增曲目）的在途 seq。
    details_seq: u64,
    /// 当前这一首推送的在途 seq。
    push_seq: u64,
    /// 还等着取详情的云端歌曲 ID（分片请求，取完才推送）。
    details_pending: Vec<String>,
    /// 还等着推送的歌曲 ID。
    push_queue: Vec<String>,
    /// 本轮从云端补进本地多少首。
    pulled: usize,
    /// 本轮成功推送到云端多少首。
    pushed: usize,
    /// 本轮失败多少首（网络/权限），只用于提示，不重试。
    failed: usize,
}

impl FavoriteSync {
    /// 是否有任何一段还在途。
    fn active(&self) -> bool {
        self.ids_seq != 0 || self.details_seq != 0 || self.push_seq != 0
    }

    /// 回到空闲态。
    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// 算收藏同步的两个差集：`(云端有本地没有的, 本地有云端没有的)`。
///
/// 抽成纯函数是为了能单测——"双向同步"最容易出的错就是把方向搞反（把云端
/// 缺的歌从本地删掉），而这种错在界面上看不出来，只会在下次登录时表现为
/// "收藏莫名少了几首"。用 `HashSet` 而不是 `Vec::contains`：上千首收藏时
/// 后者是 O(n²)。
fn favorite_sync_plan(
    cloud_ids: &[String],
    local_online: &std::collections::HashSet<String>,
) -> (Vec<String>, Vec<String>) {
    let cloud: std::collections::HashSet<&str> = cloud_ids.iter().map(String::as_str).collect();
    let pull: Vec<String> = cloud_ids
        .iter()
        .filter(|id| !local_online.contains(*id))
        .cloned()
        .collect();
    let mut push: Vec<String> = local_online
        .iter()
        .filter(|id| !cloud.contains(id.as_str()))
        .cloned()
        .collect();
    // HashSet 迭代序随机：排一下让日志与失败重试可复现
    push.sort();
    (pull, push)
}

/// 取第一位**有 ID** 的艺人。
///
/// `artist_ids` 与 `artists` 按下标一一对应，缺失位是空字符串（见
/// [`Track::artist_ids`]）。为什么不是 `artist_ids[0]`：合唱曲里第一位艺人
/// 偶尔不给 ID，直接取第 0 位会跳到一个空页；这里取"第一个真的有 ID 的"，
/// 名字仍然按同一个下标取，不会张冠李戴。
fn primary_artist_id(track: &Track) -> Option<&str> {
    track
        .artist_ids
        .iter()
        .map(String::as_str)
        .find(|id| !id.is_empty())
}

/// 「最近播放」时间窗文案。
fn history_window_label(hours: i64, lang: Lang) -> String {
    match hours {
        168 => pick(lang, "7 天", "7D").to_string(),
        720 => pick(lang, "30 天", "30D").to_string(),
        _ => pick(lang, "24 小时", "24H").to_string(),
    }
}

/// 用户数据装载：生产读真实配置目录，测试恒取默认值。
///
/// 测试里 `assemble()` 会被构造无数次，若直连 `%APPDATA%` 就会读写真实用户
/// 历史/收藏/歌单——既污染数据也让断言不可复现（既有 `prefs.rs` 踩过同样的坑）。
fn user_data<T: Default>(load: impl FnOnce() -> T) -> T {
    #[cfg(test)]
    {
        let _ = load;
        T::default()
    }
    #[cfg(not(test))]
    {
        load()
    }
}

/// 已入库的一张真实封面：纹理 + 最近一次被绘制的时钟刻度。
///
/// 有 `used` 才能做 LRU。没有它只能「装满整体清空」，而那正是封面闪烁的根子：
/// 一次清空把满屏的封面同时打回占位图，重新请求、重新装满、再清空。
struct CachedCover {
    tex: TextureHandle,
    used: u64,
}

/// Reimu Player 主程序。
pub struct ReimuApp {
    // ---- Provider / 桥（可替换）----
    //
    // `factory` / `fetcher` / `timeouts` 三个字段随「SEARCH TIMEOUT 设置」一起删除：
    // 那套设置靠**重建 Provider + 重开桥**生效，代价是丢掉登录 Cookie。
    // 音质设置改走 `bridge.set_quality()`（原子写），不再需要保留重建材料。
    bridge: ProviderBridge,

    // ---- 播放编排核心 ----
    core: PlayerCore<RodioEngine>,

    // ---- 导航 ----
    page: Page,
    /// 上次同步到 OS 窗口标题的页面。
    title_page: Page,

    // ---- 窗口管理（自绘边框交互）----
    /// 关闭淡出动画（[`CloseAnim`]，完成才发 [`egui::ViewportCommand::Close`]）。
    close_anim: CloseAnim,

    // ---- 输入 ----
    search_input: String,
    /// 下一帧把键盘焦点交给搜索框（Ctrl+F / `/` 快捷键）。
    focus_search: bool,
    playlist_input: String,
    /// 本次歌单请求成功后是否跳到 Playlist 页（仅显式"打开"动作为真）。
    playlist_jump: bool,
    /// 本次歌单请求成功后是否自动播放第一首（榜单行 ▶ / 双击）。
    playlist_autoplay: bool,

    // ---- 请求序列号（过期响应丢弃）----
    search_seq: u64,
    detail_seq: u64,
    playlist_seq: u64,

    // ---- 加载状态 ----
    search_state: LoadState<Vec<Track>>,
    detail_state: LoadState<Vec<Track>>,
    playlist_state: LoadState<Playlist>,
    // ---- 精选列表（推荐歌单 / 官方排行榜）----
    rec_seq: u64,
    rec_state: LoadState<Vec<PlaylistSummary>>,
    chart_seq: u64,
    chart_state: LoadState<Vec<PlaylistSummary>>,
    /// 推荐页两列原地展开的歌单详情（不写进已载入历史）。
    rec_peek: Option<PlaylistPeek>,
    chart_peek: Option<PlaylistPeek>,

    // ---- 每日推荐（需登录）----
    daily_seq: u64,
    /// 每日推荐歌曲（服务端按账号 + 当天生成，**页面直接展开成列表**）。
    daily_state: LoadState<Vec<Track>>,
    /// 当前这批推荐属于哪一天（[`super::daily_clock::Ymd::key`]）。
    ///
    /// 存在的唯一理由：判断"跨天了没有"。跨天必须自动重拉——服务端按天
    /// 生成内容，不重拉的话用户第二天打开看到的还是昨天的推荐。
    /// `0` = 还没成功拉到过（此时不参与跨天比较）。
    daily_day: u32,
    /// 上次做跨天检查的时刻（秒，UI 累计时间）。见 [`DAILY_CHECK_SECS`]。
    daily_check_at: f32,

    // ---- 登录（二维码，凭据只存在 Provider 内存里）----
    login: LoginUi,
    /// `QrStart` 请求序列号。
    qr_seq: u64,
    /// `QrPoll` 请求序列号。
    qr_poll_seq: u64,
    /// 下次轮询的时间戳（`t + QR_POLL_SECS`）；`None` = 暂停轮询。
    qr_poll_at: Option<f32>,
    /// 连续轮询失败次数（网络抖动不终止轮询，超过上限才停）。
    qr_poll_fails: u32,
    /// 本地编码出来的二维码纹理（内容不出进程）。
    qr_texture: Option<TextureHandle>,

    // ---- 登录（手机号 / Cookie / 退出，会话跨启动持久化）----
    /// 手机号登录请求序列号。
    phone_seq: u64,
    /// Cookie 登录请求序列号。
    cookie_seq: u64,
    /// 退出登录请求序列号。
    logout_seq: u64,

    // ---- 分享链接（粘贴即解析）----
    /// 短链展开请求序列号。
    link_seq: u64,
    /// 正在展开的短链（响应回来时校验）。
    link_pending: Option<String>,
    /// 链接解析出的单曲 ID：详情回来即自动装带播放。
    link_song: Option<String>,

    // ---- 真实封面（网易云 cover_url → 半调点阵纹理）----
    /// URL → 已合成的点阵纹理（带最近使用刻度，超限时按 LRU 逐条淘汰）。
    cover_tex: HashMap<String, CachedCover>,
    /// 封面使用时钟：每帧递增，作为 LRU 的「现在」。
    cover_clock: u64,
    /// URL → 在途请求序列号（避免每帧重复请求）。
    cover_inflight: HashMap<String, u64>,
    /// 拉取/解码失败的 URL（不再重试，回退程序化封面）。
    cover_failed: std::collections::HashSet<String>,

    // ---- 列表/选中 ----
    selected: Option<usize>,
    /// 播放队列快照：最近一次「开始播放」时的列表。
    ///
    /// 与 `current_tracks()`（界面正在浏览的列表）分离：加载新歌单/搜索新关键词
    /// 只改浏览列表，不会偷换「下一首」；用户真正点播放时才建立新队列。
    queue: Vec<Track>,
    /// 队列是否已建立（区分"空队列"与"还没播过"）。
    queue_active: bool,
    /// 已载入歌单（真实历史，不伪造）。
    playlists: Vec<Playlist>,
    /// 歌单 ID → 最近打开时刻（毫秒），超出上限时淘汰最久未访问的。
    playlist_opened_at: std::collections::HashMap<String, u64>,
    active_playlist: Option<usize>,
    /// 本地曲库（指定文件夹扫描，[`library`]）。
    library: Option<library::LocalLibrary>,
    /// Playlist 页是否选中本地曲库卡（与 `active_playlist` 互斥）。
    active_local: bool,
    /// 已知曲目表（ID → 曲目元数据，跨页面复用）。
    known: HashMap<String, Track>,
    /// 最近搜索（真实历史）。
    recent: Vec<String>,

    // ---- 播放顺序（随机洗牌序列 / 列表循环）----
    /// 随机播放的**洗牌序列**（索引顺序）。`None` = 未启用随机或列表已变。
    ///
    /// 用「洗牌序列」而非「每次随机」：让「实际下一首」可确定性预知，
    /// 预缓存才拿得到目标；也顺带修掉旧实现「随机可能命中当前曲 / 同秒重复」。
    shuffle_order: Option<Vec<usize>>,
    /// 洗牌序列游标（下一首在 `shuffle_order` 中的位置）。
    shuffle_pos: usize,
    /// 洗牌种子（单调递增，避免相邻两次洗出相同顺序）。
    shuffle_seed: u64,
    /// 洗牌序列对应的列表指纹（列表内容变化即重洗）。
    list_fingerprint: u64,

    // ---- 心动模式（智能播放）----
    /// 是否处于心动模式。
    ///
    /// **与随机互斥**：随机是在封闭列表里打乱，心动是向开放流续曲，两者对
    /// 「下一首从哪来」的回答互相矛盾。开启一个必然关闭另一个（见
    /// [`Self::start_heartbeat`] 与 [`Self::cycle_play_mode`]）。
    heartbeat_on: bool,
    /// 进入心动前的播放模式，退出时原样恢复。
    heartbeat_prev_mode: Option<PlayMode>,
    /// 种子：`(歌曲 ID, 所属歌单 ID)`。续批要复用同一个歌单上下文。
    heartbeat_seed: Option<(String, String)>,
    /// 心动批次里已追加到 [`Self::queue`] 的最后一条的下标（`None` = 还没追加过）。
    ///
    /// 队列游标逼近这里就该续批。
    heartbeat_tail: Option<usize>,
    /// 第一条心动续曲在 [`Self::queue`] 里的下标；退出时把这条之后的未播曲目收回去。
    heartbeat_start: Option<usize>,
    /// 已按下开关、但还在等种子就绪（本地曲目要先匹配网易云 ID）。
    heartbeat_pending: bool,
    /// 在途请求序列号（`None` = 没有在途）。
    heartbeat_seq: Option<u64>,
    /// 已入库的心动曲目 ID（服务端会重复推荐，去重后才不至于"下一首又是它"）。
    heartbeat_seen: std::collections::HashSet<String>,
    /// 连续拿到"全重复/空"批次的次数；到 [`HEARTBEAT_MAX_STALL`] 就收手关闭心动。
    heartbeat_stall: u8,

    // ---- 本地曲目 → 网易云匹配 ----
    /// 本地路径 → 匹配到的网易云 [`Track`]（含封面与数字歌曲 ID）。
    local_match: HashMap<String, Track>,
    /// 在途匹配请求：路径 → 序列号（去重，避免每帧重发）。
    match_inflight: HashMap<String, u64>,

    // ---- 歌词缓存（下一首预缓存）----
    /// 跨曲目的歌词缓存（键 = 歌曲 ID）。
    lyric_cache: LyricsCache,
    /// 在途预取歌词：歌曲 ID → 请求序列号（去重，避免每帧重发）。
    lyric_inflight: HashMap<String, u64>,

    // ---- 下一首音频预取 ----
    /// 在途音频预取的请求序列号（0 = 没有）。
    prefetch_seq: u64,
    /// 已发出预取的歌曲 ID（去重：同一首不重复预取）。
    ///
    /// 这个状态必须能被"取消"清掉：被掐掉的预取如果不清账，那首歌就
    /// **永远不再被预取**——用户看到的是"每次切到它都要等整首下载"。
    prefetch_song: Option<String>,

    // ---- 歌词滚动 ----
    lyric_focus: Option<usize>,
    last_lyric: Option<usize>,

    // ---- 舱体动画（机械时间线 [`deck_seq::DeckSeq`] 驱动）----
    tray_open: f32,
    insert: f32,
    /// 落座轻震 y 偏移（px）。
    seat_dy: f32,
    /// 合舱咔哒 y 偏移（px）。
    well_dy: f32,
    /// 机械时间线（boot / 开舱→退带→装带→落座→合舱→咔哒）。
    deck_seq: deck_seq::DeckSeq,
    /// 当前舱内磁带 ID（变更检测 → 换带时间线）。
    deck_tape_id: Option<String>,

    // ---- 设置 ----
    /// 曲目信息面板底部的设置区是否展开（默认收起）。
    settings_open: bool,
    /// 界面语言（**生效值**）。
    lang: Lang,
    /// 用户偏好里记的语言。
    ///
    /// 与 [`Self::lang`] 分开的原因：探针 `REIMU_PROBE_LANG` 会覆盖 `lang` 做视觉
    /// 校对，而落盘只许写用户的真实选择。两者共用一个字段时，探针运行里任何一次
    /// `persist_prefs`（改音质也会调）都会把强制值写进用户 `prefs.json`——实测泄漏过。
    pref_lang: Lang,

    // ---- 桌面歌词 ----
    /// 分发中心：门控 + 去重，渲染端由 [`super::desktop_lyric::DesktopLyricSink`] 提供。
    desktop_lyric: super::desktop_lyric::DesktopLyricHub,
    /// 桌面歌词的共享控制状态（锁定 / 背景不透明度 / 进度条样式）。
    ///
    /// 三个入口（设置面板、歌词窗、prefs 落盘）读写的是**同一个对象**，
    /// 「锁定 / 解锁 / 调透明度互不冲突」靠的就是这一点，而不是靠互相发消息。
    lyric_control: Arc<super::lyric_control::LyricControl>,
    /// 探针钉死透明度时，**冻结下来用于落盘的**原偏好值（仅调试构建会有 `Some`）。
    ///
    /// 有值时[`Self::prefs_snapshot`]用这里的值——和 `REIMU_PROBE_LANG` 一个规矩：
    /// 探针只影响当次显示，绝不改写用户配置。
    /// （阶段 16 踩过：一次 `OPACITY=0` 的截图跑完就把 0% 写进了 prefs.json，
    /// 下次正常启动桌面歌词只剩文字、背景全没了，看起来像"窗口不见了"。）
    lyric_opacity_frozen: Option<u8>,

    // ---- 用户数据（本地持久化：历史 / 收藏 / 自建歌单）----
    /// 播放历史（"听够"才记，见 [`super::history::qualifies`]）。
    history: super::history::PlayHistory,
    /// 本地收藏（同时充当账号「我喜欢的音乐」的本地镜像）。
    favorites: super::favorites::Favorites,
    /// 自建歌单。
    local_playlists: super::local_playlist::LocalPlaylists,
    /// 当前播放曲目快照（历史记录用：本地曲目 `NowPlaying.id` 只有文件名）。
    current_track: Option<Track>,
    /// 当前这首是否已经记进历史（一首只记一次）。
    history_marked: bool,
    /// 在途的「喜欢」请求序列号（0 = 无）。
    like_seq: u64,
    /// 在途「喜欢」请求对应的曲目（响应回来时据此改本地镜像）。
    like_target: Option<Track>,

    // ---- 搜索：本地检索 + 高亮 ----
    /// 在线/本地结果共用的高亮关键词。
    highlight_query: String,
    /// 本地曲库命中（最多 [`LOCAL_HIT_LIMIT`] 条）。
    local_hits: Vec<Track>,

    // ---- 专辑 / 艺人页（复用磁带架显示区）----
    /// 专辑曲目缓存（`UserView::Album` 时是磁带架内容）。
    album_tracks: Vec<Track>,
    /// 艺人曲目缓存（`UserView::Artist` 时是磁带架内容）。
    artist_tracks: Vec<Track>,
    /// 当前在看的专辑/艺人（`UserView::Album` / `Artist` 时必为 `Some`）。
    shelf_target: Option<ShelfTarget>,
    /// 进入专辑/艺人页之前的状态（返回时恢复）。
    shelf_back: Option<ShelfSnapshot>,
    album_seq: u64,
    artist_seq: u64,
    /// 专辑/艺人页的加载状态（内容在 `album_tracks` / `artist_tracks`）。
    album_state: LoadState<()>,
    artist_state: LoadState<()>,

    // ---- 收藏双向同步 ----
    sync: FavoriteSync,

    // ---- 我的歌单（网易云个人歌单）----
    /// 登录后拉到的账号歌单（空 = 没拉到 / 还没登录）。
    user_playlists: Vec<PlaylistSummary>,
    user_pl_seq: u64,
    user_pl_state: LoadState<()>,
    /// 拉过一次就不要再每帧拉（登录态变化时才重置）。
    user_pl_loaded: bool,

    // ---- 统一右键菜单 ----
    /// 当前打开的菜单（`None` = 没开）。
    menu: Option<super::context_menu::ContextMenuState>,
    /// 菜单指向的曲目（打开菜单那一刻的**快照**）。
    ///
    /// 不存下标再回查：菜单开着的时候列表可能因为响应落地而变（收藏页取消
    /// 收藏会删行），按下标回查会点到隔壁那首。
    menu_track: Option<Track>,
    /// 菜单指向的行号（只有"从当前歌单移除"需要）。
    menu_index: Option<usize>,
    /// 菜单自带的队列快照（每日推荐/榜单这类"列表即队列"的行）：
    /// 播放时用它建队列，而不是把当前浏览列表顶进去。
    menu_chart: Option<Vec<Track>>,

    // ---- 托盘（默认关闭）----
    tray: Option<super::tray::Tray>,
    tray_enabled: bool,
    /// 托盘创建失败只提示一次。
    tray_warned: bool,
    /// 窗口是否已被「收进托盘」（关掉托盘开关时要放回来，否则进程不可见）。
    hidden_to_tray: bool,
    /// 托盘要求退出（在 `update` 末尾统一发 Close，避免在轮询中途拆自己）。
    quit_requested: bool,

    // ---- SMTC（与媒体键同一个开关）----
    smtc: Option<super::smtc::Smtc>,
    /// SMTC 建立失败只提示一次。
    smtc_warned: bool,

    /// 本帧的 egui 上下文。
    ///
    /// 托盘命令、设置动作都在**没有 `&mut Ui`** 的地方执行，却要发视口命令
    /// （`Visible` / `Focus` / `Close`）。`egui::Context` 内部是 `Arc`，克隆很便宜。
    ctx: Option<egui::Context>,

    // ---- 虚拟列表（收藏 / 最近播放 / 自建歌单，需 &[Track] 供 current_tracks）----
    /// Playlist 页当前选中的用户数据视图。
    ///
    /// 与 [`Self::active_local`] / [`Self::active_playlist`]
    /// **互斥**：互斥关系由 [`Self::select_user_view`] 与 [`Self::clear_user_view`]
    /// 强制，避免出现「两个来源同时为真、列表却只有一个」的错乱。
    user_view: UserView,
    /// 最近播放的时间窗（小时：24 / 168 / 720）。
    history_hours: i64,
    favorites_view: Vec<Track>,
    history_view: Vec<Track>,
    local_pl_view: Vec<Track>,

    // ---- 自建歌单编辑 ----
    /// 新建歌单输入框。
    new_playlist_name: String,
    /// 正在重命名的歌单（id + 输入缓冲）。
    rename_target: Option<u64>,
    rename_input: String,
    /// 二次确认模态（删除歌单 / 移除载入历史）。
    confirm: Option<ConfirmDialog>,

    // ---- 媒体键 ----
    media_keys: Option<super::media_keys::MediaKeys>,
    media_keys_enabled: bool,
    /// 注册失败只提示一次（每帧轮询不该反复弹 toast）。
    media_keys_warned: bool,

    // ---- 离线每日推荐（未登录降级内容）----
    daily_local: Vec<Track>,
    daily_offline: bool,

    // ---- 标题走马灯 ----
    /// 当前标题的滚动累计时长（秒；换曲归零，避免新标题从半途开始滚）。
    marquee_t: f32,
    /// 上一帧的标题（变更检测用）。
    marquee_title: String,

    // ---- 播放/窗口交互（拖动 seek、静音、全屏、列表滚动）----
    /// 波形拖动中的预览比例（`None` = 显示真实进度）。
    seek_preview: Option<f32>,
    /// 时间行显示剩余时间（点击切换）。
    show_remaining: bool,
    /// 静音态与静音前音量（点 VOL/数字切换）。
    muted: bool,
    pre_mute_volume: f32,
    /// F11 全屏态。
    fullscreen: bool,
    /// 结果列表指纹：变化时滚回顶部（避免换关键词后停在空白区）。
    res_scroll_fp: u64,
    /// 官方榜单（推荐页右栏）选中行下标。
    chart_selected: Option<usize>,

    // ---- 反馈 ----
    toast: Option<(String, f32)>,
    error_modal: Option<(String, String)>,

    // ---- 封面纹理缓存（§3.8：产物缓存为纹理，禁止每帧重算）----
    covers: HashMap<u64, TextureHandle>,

    // ---- 动画时钟 ----
    t: f32,

    // ---- 实时频谱（EQ 面板显示电平，含暂停后的衰减）----
    eq_levels: crate::audio::Bands,
}

/// 把网易云匹配结果并进一条本地曲目。
///
/// **只补空、不改写**：`艺人 - 歌名` 是用户自己写进文件名/标签的字，服务端
/// 匹配再准也不该把它换掉（换错了用户会以为曲库被程序动过）。这里补的是
/// 本地文件根本拿不到的东西：封面、专辑、精确时长，以及 —— 存在 `id` 之外
/// 的匹配表里 —— 可用于心动模式的数字歌曲 ID。
fn merge_local_match(target: &mut Track, matched: &Track) {
    if target.cover_url.is_none() {
        target.cover_url = matched.cover_url.clone();
    }
    if target.album.is_none() {
        target.album = matched.album.clone();
    }
    if target.album_id.is_none() {
        target.album_id = matched.album_id.clone();
    }
    if target.duration_ms.is_none() {
        target.duration_ms = matched.duration_ms;
    }
    if target
        .artists
        .iter()
        .all(|a| a.trim().is_empty() || a == "—")
        && !matched.artists.is_empty()
    {
        target.artists = matched.artists.clone();
        target.artist_ids = matched.artist_ids.clone();
    }
}

impl ReimuApp {
    /// 组装骨架：生产构造（[`Self::new`]）与测试构造（`test_app`）共用。
    ///
    /// 为什么单独抽出来：结构体字面量曾在两处各写一遍，加字段时漏改一处就编译
    /// 不过；抽成唯一装配点后，新字段只在这里出现一次。
    fn assemble(
        bridge: ProviderBridge,
        core: PlayerCore<RodioEngine>,
        library: Option<library::LocalLibrary>,
    ) -> Self {
        // 启动即恢复本地曲库：有持久化歌单时默认选中它，重启后仍是一张可用的歌单
        let has_library = library.is_some();
        let library_has_tracks = library
            .as_ref()
            .map(|l| !l.tracks.is_empty())
            .unwrap_or(false);
        Self {
            bridge,
            core,
            page: Page::Player,
            title_page: Page::Player,
            search_input: String::new(),
            focus_search: false,
            playlist_input: String::new(),
            playlist_jump: false,
            playlist_autoplay: false,
            search_seq: 0,
            detail_seq: 0,
            playlist_seq: 0,
            search_state: LoadState::new(),
            detail_state: LoadState::new(),
            playlist_state: LoadState::new(),
            rec_seq: 0,
            rec_state: LoadState::new(),
            chart_seq: 0,
            chart_state: LoadState::new(),
            rec_peek: None,
            chart_peek: None,
            daily_seq: 0,
            daily_state: LoadState::new(),
            daily_day: 0,
            // 负值 → 首帧立刻检查一次（否则启动后前 5 秒不查跨天）
            daily_check_at: -DAILY_CHECK_SECS,
            login: LoginUi::default(),
            qr_seq: 0,
            qr_poll_seq: 0,
            qr_poll_at: None,
            qr_poll_fails: 0,
            qr_texture: None,
            phone_seq: 0,
            cookie_seq: 0,
            logout_seq: 0,
            link_seq: 0,
            link_pending: None,
            link_song: None,
            cover_tex: HashMap::new(),
            cover_clock: 0,
            cover_inflight: HashMap::new(),
            cover_failed: std::collections::HashSet::new(),
            selected: if library_has_tracks { Some(0) } else { None },
            queue: Vec::new(),
            queue_active: false,
            playlists: Vec::new(),
            playlist_opened_at: std::collections::HashMap::new(),
            active_playlist: None,
            library,
            active_local: has_library,
            known: HashMap::new(),
            recent: Vec::new(),
            shuffle_order: None,
            shuffle_pos: 0,
            shuffle_seed: 0x9E37_79B9_7F4A_7C15,
            list_fingerprint: 0,
            heartbeat_on: false,
            heartbeat_prev_mode: None,
            heartbeat_seed: None,
            heartbeat_tail: None,
            heartbeat_start: None,
            heartbeat_pending: false,
            heartbeat_seq: None,
            heartbeat_seen: std::collections::HashSet::new(),
            heartbeat_stall: 0,
            local_match: HashMap::new(),
            match_inflight: HashMap::new(),
            lyric_cache: LyricsCache::new(),
            lyric_inflight: HashMap::new(),
            prefetch_seq: 0,
            prefetch_song: None,
            lyric_focus: None,
            last_lyric: None,
            tray_open: 0.0,
            insert: 0.0,
            seat_dy: 0.0,
            well_dy: 0.0,
            deck_seq: deck_seq::DeckSeq::default(),
            deck_tape_id: None,
            settings_open: false,
            lang: Lang::default(),
            pref_lang: Lang::default(),
            desktop_lyric: super::desktop_lyric::DesktopLyricHub::new(),
            // 测试环境不落盘，透明度/样式就是默认值；真实启动在 `new()` 里按 prefs 恢复
            lyric_control: Arc::new(super::lyric_control::LyricControl::new()),
            lyric_opacity_frozen: None,
            // 用户数据：测试环境一律取默认值（绝不读写真实 `%APPDATA%`）
            history: user_data(super::history::PlayHistory::load),
            favorites: user_data(super::favorites::Favorites::load),
            local_playlists: user_data(super::local_playlist::LocalPlaylists::load),
            current_track: None,
            history_marked: false,
            like_seq: 0,
            like_target: None,
            highlight_query: String::new(),
            local_hits: Vec::new(),
            album_tracks: Vec::new(),
            artist_tracks: Vec::new(),
            shelf_target: None,
            shelf_back: None,
            album_seq: 0,
            artist_seq: 0,
            album_state: LoadState::new(),
            artist_state: LoadState::new(),
            sync: FavoriteSync::default(),
            user_playlists: Vec::new(),
            user_pl_seq: 0,
            user_pl_state: LoadState::new(),
            user_pl_loaded: false,
            menu: None,
            menu_track: None,
            menu_index: None,
            menu_chart: None,
            tray: None,
            tray_enabled: false,
            tray_warned: false,
            hidden_to_tray: false,
            quit_requested: false,
            smtc: None,
            smtc_warned: false,
            ctx: None,
            user_view: UserView::None,
            history_hours: HISTORY_HOURS_DEFAULT,
            favorites_view: Vec::new(),
            history_view: Vec::new(),
            local_pl_view: Vec::new(),
            new_playlist_name: String::new(),
            rename_target: None,
            rename_input: String::new(),
            confirm: None,
            media_keys: None,
            media_keys_enabled: true,
            media_keys_warned: false,
            daily_local: Vec::new(),
            daily_offline: false,
            marquee_t: 0.0,
            marquee_title: String::new(),
            seek_preview: None,
            show_remaining: false,
            muted: false,
            pre_mute_volume: 0.72,
            fullscreen: false,
            res_scroll_fp: 0,
            chart_selected: None,
            toast: None,
            error_modal: None,
            covers: HashMap::new(),
            t: 0.0,
            eq_levels: [0.0; crate::audio::BANDS],
            close_anim: CloseAnim::default(),
        }
    }

    /// 构建主程序：装配全局样式/字体，注入 Provider 工厂与音频下载器。
    ///
    /// `library_dir` 给定时启动即扫描本地曲库（`--library`）。
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        timeouts: ProviderTimeouts,
        factory: Arc<dyn Fn(ProviderTimeouts) -> Arc<dyn DynMusicProvider> + Send + Sync>,
        fetcher: Arc<dyn DynAudioFetcher>,
        library_dir: Option<String>,
    ) -> Self {
        // 偏好要先读一次：**主题下标必须在 `apply_global_style` 之前定下来**，
        // 否则 egui 那套 panel_fill / selection 色会按默认主题烘进去，
        // 换个主题只有一半界面跟着变。
        let prefs = super::prefs::load();
        theme::set_theme_index(prefs.theme as usize);
        theme::apply_global_style(&cc.egui_ctx);
        theme::setup_fonts(&cc.egui_ctx);
        let bridge = ProviderBridge::spawn(factory(timeouts), fetcher.clone());
        // 音频输出设备缺失不阻断程序：浏览/歌词仍可用（core 降级）
        let engine = RodioEngine::open()
            .map(crate::music::playback_controller::PlaybackController::new)
            .map_err(|e| tracing::warn!("音频输出设备不可用: {}", e))
            .ok();
        let mut app = Self::assemble(bridge, PlayerCore::new(engine), library::load());
        // 开机装带（HTML boot()：开舱→装带→落座→合舱→咔哒→toast）
        app.deck_seq.start_boot();
        // 恢复偏好：音质上限 + 界面语言（默认档/默认语言 = 旧行为，无回退风险）
        app.bridge.set_quality(prefs.quality);
        app.lang = prefs.lang;
        app.pref_lang = prefs.lang;
        // 播放偏好：音量 / 静音 / 随机 / 循环（老配置缺键 → 默认值）
        let volume = prefs.volume.min(100) as f32 / 100.0;
        app.core.set_volume(if prefs.muted { 0.0 } else { volume });
        app.muted = prefs.muted;
        app.pre_mute_volume = if volume > 0.0 { volume } else { 0.72 };
        // 恢复也走档位入口：老配置里「随机 + 单曲循环」这种矛盾组合会被归一到随机档
        app.core.set_play_mode(PlayMode::of(
            prefs.shuffle,
            RepeatMode::from_code(prefs.repeat),
        ));
        // 探针可强制语言：中/英两态都要能截到图校对。只改生效值 `lang`，
        // 落盘走 `pref_lang`，用户偏好不会被探针污染。
        if let Some(lang) = super::probe::forced_lang() {
            tracing::info!(?lang, "probe: 强制界面语言（仅本次运行生效）");
            app.lang = lang;
        } else {
            tracing::info!(lang = ?app.lang, "界面语言（prefs.json）");
        }
        // 视觉校对：探针要求展开设置区时提前打开（默认收起，否则拍不到）
        if super::probe::wants_settings_open() {
            app.settings_open = true;
        }
        // 视觉校对：探针要求时直接开启桌面歌词（独立视口，只能靠它验窗口）
        #[cfg(debug_assertions)]
        if super::probe::wants_lyric_window() {
            app.desktop_lyric.set_enabled(true);
        }
        // 视觉校对：探针要求时预置一次本地检索（校对逐字高亮）
        #[cfg(debug_assertions)]
        if let Some(query) = super::probe::forced_query() {
            app.search_input = query.clone();
            app.clear_user_view();
            app.active_local = false;
            app.active_playlist = None;
            app.run_local_search(&query);
        }
        // 查一次登录态：让标题栏 chip 一启动就是真实状态（匿名也是合法结果）
        app.bridge.login_status();
        // 桌面歌词渲染端：两个透明置顶视口（纯展示层 + 控件层）。
        // 锁定/透明度/进度条样式与设置面板共享**同一个** `LyricControl`，
        // 三个视口读写的都是它，才谈得上"三者互不冲突"。
        // 落盘值在这里恢复：透明度与进度条样式是外观偏好，锁定态**不恢复**
        // （开机就锁死等于把用户关在门外）。
        app.lyric_control = Arc::new(super::lyric_control::LyricControl::restore(
            prefs.lyric_opacity,
            prefs.lyric_progress,
        ));
        // 视觉校对：探针要求时先把锁定态与透明度摆到指定值（截图用）
        #[cfg(debug_assertions)]
        {
            if super::probe::wants_lyric_locked() {
                app.lyric_control.set_locked(true);
            }
            if let Some(pct) = super::probe::forced_lyric_opacity() {
                app.lyric_control.set_opacity(pct);
                // 冻结落盘值：探针只改当次显示，prefs 里仍是用户原来的数字
                app.lyric_opacity_frozen = Some(prefs.lyric_opacity);
            }
        }
        let mut sink =
            super::lyric_window::LyricWindow::new(cc.egui_ctx.clone(), app.lyric_control.clone());
        sink.set_lang(app.lang);
        app.desktop_lyric.attach(Box::new(sink));
        // 媒体键：独立线程注册 VK_MEDIA_*；注册失败只提示一次，不影响其它功能
        app.start_media_keys();
        // SMTC 不在这里建：它一建立，系统音量浮层就会多出一张空的媒体卡。
        // 改为「第一首曲目开始播放时」懒建（见 `poll_smtc`），对用户更干净。
        // 托盘默认关闭：只有偏好里明确打开过才创建
        app.tray_enabled = prefs.tray;
        if app.tray_enabled {
            app.start_tray();
        }
        // 离线每日推荐：首次进入 Daily 页前就要算好（未登录时页面才有内容）
        app.rebuild_daily_offline();
        // 视觉校对：探针要求时把每日推荐钉在指定状态（截图用）。
        // 必须在这里注入而不是进页面之后——`ensure_daily` 只在 Idle 时才发请求，
        // 先摆好状态就不会被真实请求覆盖。
        #[cfg(debug_assertions)]
        match super::probe::daily_state_override() {
            Some("loading") => app.daily_state.begin_load(),
            Some("failed") => app.daily_state.finish(Err(
                crate::music::provider::ProviderError::AuthenticationRequired,
            )),
            Some("empty") => app.daily_state.finish(Ok(Vec::new())),
            _ => {}
        }
        // 启动即扫描本地曲库（--library）
        if let Some(dir) = library_dir {
            app.load_library_dir(Path::new(&dir));
        }
        app
    }

    // ------------------------------------------------------------------
    // 请求发起
    // ------------------------------------------------------------------

    /// 切页（窗口标题同步见 `ui()` 帧首）。
    fn set_page(&mut self, page: Page) {
        self.page = page;
        match page {
            Page::Rec => self.ensure_featured(),
            Page::Daily => self.ensure_daily(),
            Page::Playlist => self.ensure_user_playlists(),
            _ => {}
        }
    }

    /// 精选列表惰性加载：首次进 Rec 页各发一次（推荐歌单 / 官方排行榜）。
    fn ensure_featured(&mut self) {
        if self.rec_state.is_idle() {
            self.rec_seq = self.bridge.featured(FeaturedKind::Recommended, 12);
            self.rec_state.begin_load();
        }
        if self.chart_state.is_idle() {
            self.chart_seq = self.bridge.featured(FeaturedKind::Toplists, 12);
            self.chart_state.begin_load();
        }
    }

    /// 每日推荐歌曲（`/api/v3/discovery/recommend/songs`，需登录）。
    ///
    /// 进入页面即拉，**不要求任何点击**：页面本身就是"今天这 30 首"的完整列表，
    /// 没有卡片、没有折叠、没有"点开才看到曲目"这一层。
    fn ensure_daily(&mut self) {
        if self.daily_state.is_idle() {
            self.request_daily();
        }
    }

    /// 发起一次每日推荐请求，并记下这批属于哪一天。
    fn request_daily(&mut self) {
        self.daily_day = super::daily_clock::today_key();
        self.daily_seq = self.bridge.daily_songs();
        self.daily_state.begin_load();
    }

    /// 跨天检查入口（每帧调用，内部按 [`DAILY_CHECK_SECS`] 节流）。
    ///
    /// `GetLocalTime` 是系统调用，没必要 60fps 打一次；5 秒粒度对"跨天换新"
    /// 完全够用——没人会掐着零点零几秒看推荐变没变。
    fn tick_daily_rollover(&mut self) {
        if self.t - self.daily_check_at < DAILY_CHECK_SECS {
            return;
        }
        self.daily_check_at = self.t;
        self.check_daily_rollover(super::daily_clock::today_key());
    }

    /// 跨天判定本体（纯逻辑，可单测）。
    ///
    /// 为什么必须每帧查、而不是只在进页面时查：用户可能开着程序过夜，
    /// 也可能把窗口收进托盘、第二天直接点开。只在切页时查，会让他一直
    /// 看着昨天的推荐——而"每日"的全部意义就在于它会变。
    fn check_daily_rollover(&mut self, today: u32) {
        if self.daily_day == 0 || self.daily_day == today {
            return;
        }
        tracing::info!(
            old = self.daily_day,
            new = today,
            "每日推荐已跨天，自动重新拉取当天数据"
        );
        // 旧内容整批作废：服务端那批已经不叫"今天"了
        self.daily_state.reset();
        self.request_daily();
        // 离线兜底也换新一天的（同一份曲库、不同种子）
        self.rebuild_daily_offline();
    }

    /// 重新拉一次（页面上的重载按钮 / 失败后重试）。
    fn refresh_daily(&mut self) {
        self.request_daily();
        // 顺带重算离线推荐：换了本地曲库/听了新歌，离线那批也该跟着变
        self.rebuild_daily_offline();
    }

    /// 重算「离线每日推荐」（未登录/无网络时的降级内容）。
    ///
    /// 完全确定性：同一天、同一份曲库与历史，结果必然相同——用户重开程序
    /// 不会看到"今日推荐"每帧都在变（那就不叫每日了）。
    fn rebuild_daily_offline(&mut self) {
        let tracks = self
            .library
            .as_ref()
            .map(|l| l.tracks.clone())
            .unwrap_or_default();
        if tracks.is_empty() {
            self.daily_local.clear();
            self.daily_offline = false;
            return;
        }
        self.daily_local = super::recommend::daily_picks(
            &tracks,
            &self.history,
            super::history::now_secs(),
            DAILY_OFFLINE_LIMIT,
        );
        self.daily_offline = !self.daily_local.is_empty();
    }

    /// 载入并立即播放第一首（榜单行 ▶ / 双击）。
    fn play_playlist_id(&mut self, playlist_id: &str) {
        self.load_playlist_id(playlist_id, false, true);
    }

    /// 按 ID 打开歌单并跳页（分享链接等显式"打开"动作）。
    fn open_playlist_id_jumping(&mut self, playlist_id: &str) {
        self.load_playlist_id(playlist_id, true, false);
    }

    fn load_playlist_id(&mut self, playlist_id: &str, jump: bool, autoplay: bool) {
        // 连点去重：同一张歌单在途时忽略后续点击（旧响应本来也会被 seq 丢弃，
        // 但每次点击都在放大网络与"重复入列"的错觉）
        if self.playlist_state.is_loading() && self.playlist_input == playlist_id {
            return;
        }
        self.playlist_input = playlist_id.to_string();
        self.playlist_jump = jump;
        self.playlist_autoplay = autoplay;
        self.playlist_seq = self.bridge.playlist(playlist_id, 0, PAGE_LIMIT);
        self.playlist_state.begin_load();
        self.push_toast(self.t("正在载入歌单…", "Loading playlist…"));
    }

    /// 记入「已载入」历史：同 ID 原地更新，超过上限淘汰最久未访问的一张。
    ///
    /// 返回该歌单在历史中的下标（淘汰后重新定位，保证不指向已删除项）。
    fn remember_loaded_playlist(&mut self, playlist: Playlist) -> usize {
        let id = playlist.id.clone();
        if let Some(pos) = self.playlists.iter().position(|p| p.id == id) {
            self.playlists[pos] = playlist.clone();
        } else {
            self.playlists.push(playlist.clone());
        }
        self.playlist_opened_at
            .insert(id.clone(), (self.t * 1000.0) as u64);
        while self.playlists.len() > PLAYLIST_HISTORY_LIMIT {
            let active = self.active_playlist;
            let victim = self
                .playlists
                .iter()
                .enumerate()
                .filter(|(i, p)| Some(*i) != active && p.id != id)
                .min_by_key(|(_, p)| self.playlist_opened_at.get(&p.id).copied().unwrap_or(0))
                .map(|(i, _)| i);
            let Some(victim) = victim else { break };
            self.playlists.remove(victim);
            if let Some(a) = self.active_playlist.as_mut() {
                if *a > victim {
                    *a -= 1;
                }
            }
        }
        self.playlists
            .iter()
            .position(|p| p.id == id)
            .unwrap_or_else(|| self.playlists.len().saturating_sub(1))
    }

    /// 「已载入」历史里是否该显示这张卡。
    ///
    /// 账号歌单在上方已有独立卡片：同一 ID 再以「已载入」重复出现，用户会以为
    /// 点一下又新建了一张同名歌单。数据仍留在 `playlists` 里供右侧曲目与播放
    /// 使用，只是不再画第二张卡。
    ///
    /// 阶段 17 起歌单页只画「账号歌单 + 自建歌单」，这张卡已不渲染，
    /// 这条规则由单测继续守着（同一份判断将来重新挂卡时不必再想一遍）。
    #[allow(dead_code)]
    fn loaded_card_visible(&self, id: &str) -> bool {
        !self.user_playlists.iter().any(|p| p.id == id)
    }

    /// 精选歌单原地展开（推荐卡 / 榜单行 / 每日卡点击）。
    ///
    /// 刻意不走 [`Self::load_playlist_id`]：那条路会把歌单写进「已载入」历史，
    /// 点一下推荐卡，歌单页就多出一张同名卡。这里只取曲目给当前面板看。
    fn open_playlist_peek(&mut self, panel: PeekPanel, item: &PlaylistSummary) {
        let mut parts = vec![item.name.clone()];
        if let Some(n) = item.track_count {
            parts.push(format!("{n} {}", self.t("盘", "TAPES")));
        }
        if let Some(d) = item.description.as_deref().filter(|d| !d.trim().is_empty()) {
            parts.push(d.to_string());
        }
        self.open_peek(panel, &item.id, &parts.join(" · "));
    }

    /// 按 ID 展开原地详情；同一张歌单已经取到曲目时直接复用。
    fn open_peek(&mut self, panel: PeekPanel, id: &str, subtitle: &str) {
        let id = id.trim();
        if id.is_empty() {
            return;
        }
        if let Some(peek) = self.peek_mut(panel) {
            if peek.id == id && peek.state.is_ready() {
                peek.expanded = true;
                return;
            }
        }
        let seq = self.bridge.playlist(id.to_string(), 0, PAGE_LIMIT);
        let mut state = LoadState::new();
        state.begin_load();
        *self.peek_mut(panel) = Some(PlaylistPeek {
            id: id.to_string(),
            subtitle: subtitle.to_string(),
            seq,
            state,
            expanded: true,
        });
    }

    fn peek(&self, panel: PeekPanel) -> Option<&PlaylistPeek> {
        match panel {
            PeekPanel::Rec => self.rec_peek.as_ref(),
            PeekPanel::Chart => self.chart_peek.as_ref(),
        }
    }

    fn peek_mut(&mut self, panel: PeekPanel) -> &mut Option<PlaylistPeek> {
        match panel {
            PeekPanel::Rec => &mut self.rec_peek,
            PeekPanel::Chart => &mut self.chart_peek,
        }
    }

    /// 这个 seq 属于哪个面板的原地详情（响应归位用）。
    fn peek_seq_match(&self, seq: u64) -> Option<PeekPanel> {
        [PeekPanel::Rec, PeekPanel::Chart]
            .into_iter()
            .find(|panel| self.peek(*panel).is_some_and(|peek| peek.seq == seq))
    }

    /// 当前是否展开着详情（渲染分支用）。
    fn peek_open(&self, panel: PeekPanel) -> bool {
        self.peek(panel).is_some_and(|p| p.expanded)
    }

    /// 收起详情（返回列表）；已取到的曲目留在状态里供下次展开复用。
    fn close_peek(&mut self, panel: PeekPanel) {
        if let Some(peek) = self.peek_mut(panel) {
            peek.expanded = false;
        }
    }

    /// 原地详情里点一首：把这张歌单**换成磁带架的待播放内容**并开始播。
    ///
    /// 语义要点（用户明确要求）：**替换**，不是追加，也不是"另起一个播放列表"——
    /// 磁带架显示什么，下一首就来自什么。所以这里一次性做完三件事：
    /// 1. 歌单进 `playlists`（数据仍在，但不画「已载入」卡）；
    /// 2. 它成为**当前浏览列表**（`active_playlist`）→ 磁带架立刻换成它；
    /// 3. 它同时成为**播放队列** → `next` 走的就是它。
    ///
    /// 旧实现只换了队列、没换浏览列表：从推荐里点播之后，磁带架还停在上一张
    /// 歌单上，界面显示和实际播放顺序是两套东西。
    fn play_peek_track(&mut self, panel: PeekPanel, index: usize) {
        let Some(list) = self.peek(panel).and_then(|p| p.state.get()).cloned() else {
            return;
        };
        let Some(track) = list.tracks.get(index).cloned() else {
            return;
        };
        let tracks = list.tracks.clone();
        let idx = self.remember_loaded_playlist(list);
        self.clear_user_view();
        self.active_local = false;
        self.active_playlist = Some(idx);
        self.selected = Some(index);
        self.replace_queue(tracks);
        self.play_track(track);
    }

    /// 移除一张已载入歌单（不动自建歌单/账号歌单）。
    fn remove_loaded_playlist(&mut self, id: &str) {
        let Some(pos) = self.playlists.iter().position(|p| p.id == id) else {
            return;
        };
        let was_active = self.active_playlist == Some(pos);
        self.playlists.remove(pos);
        self.playlist_opened_at.remove(id);
        if let Some(a) = self.active_playlist.as_mut() {
            if *a > pos {
                *a -= 1;
            } else if *a == pos {
                self.active_playlist = None;
            }
        }
        if was_active {
            self.selected = None;
            self.push_toast(self.t(
                "已从列表移除；正在播放的曲目继续",
                "Removed from the list; playback continues",
            ));
        } else {
            self.push_toast(self.t("已从列表移除", "REMOVED FROM LIST"));
        }
    }

    /// 从账号歌单列表隐藏（仅本地，不调用取消收藏接口）。
    fn hide_account_playlist(&mut self, id: &str) {
        let before = self.user_playlists.len();
        self.user_playlists.retain(|p| p.id != id);
        if self.user_playlists.len() != before {
            self.push_toast(self.t("已从列表隐藏", "HIDDEN FROM LIST"));
        }
    }

    // ------------------------------------------------------------------
    // 分享链接（粘贴即解析）
    // ------------------------------------------------------------------

    /// 尝试把输入当网易云分享链接处理；返回 `true` 表示已接管这次输入。
    ///
    /// 分层原因：解析是纯函数（[`parse_share`]，可离线单测），短链展开要联网，
    /// 所以这里只负责"要不要发请求"的决策，网络交给桥。
    fn try_open_share(&mut self, input: &str) -> bool {
        let Some(share) = parse_share(input) else {
            return false;
        };
        match share {
            ShareRef::Resolved { kind, id } => self.open_share_target(kind, &id),
            ShareRef::ShortLink(url) => {
                self.link_seq = self.bridge.expand_link(url.clone());
                self.link_pending = Some(url.clone());
                self.push_toast(self.t("正在展开短链…", "Expanding short link…"));
            }
            ShareRef::Unsupported { segment } => {
                // 明确拒绝而不是静默失败：MV / 电台节目不在本播放器能力范围内
                self.push_toast(self.t(
                    &format!("暂不支持 /{segment} 类型链接（支持单曲、歌单）"),
                    &format!("Unsupported link type /{segment} (songs and playlists only)"),
                ));
            }
        }
        true
    }

    /// 已解析出目标资源：单曲拉详情后自动播放，歌单直接打开。
    fn open_share_target(&mut self, kind: ShareKind, id: &str) {
        match kind {
            ShareKind::Song => {
                self.link_song = Some(id.to_string());
                self.detail_seq = self.bridge.song_details(vec![id.to_string()]);
                self.detail_state.begin_load();
                self.push_toast(self.t(
                    &format!("正在装载单曲 {id}…"),
                    &format!("Loading track {id}…"),
                ));
            }
            ShareKind::Playlist => {
                self.open_playlist_id_jumping(id);
                self.push_toast(self.t(
                    &format!("正在打开歌单 {id}…"),
                    &format!("Opening playlist {id}…"),
                ));
            }
            ShareKind::Album => {
                // 专辑接口未接入 Provider：如实告知，不假装支持
                self.push_toast(self.t(
                    "专辑链接暂未接入（可改用歌单链接）",
                    "Album links are not supported yet (use a playlist link)",
                ));
            }
        }
    }

    // ------------------------------------------------------------------
    // 登录（二维码）
    // ------------------------------------------------------------------

    /// 打开登录面板并按需发起二维码（已有可用二维码时不重发）。
    fn open_login(&mut self) {
        self.login.show();
        if self.login.mode == LoginMode::Qr && self.login.needs_restart() {
            self.start_qr_login();
        }
    }

    /// 发起二维码登录（`QrStart`）。
    fn start_qr_login(&mut self) {
        self.qr_seq = self.bridge.qr_start();
        self.qr_texture = None;
        self.qr_poll_at = None;
        self.qr_poll_fails = 0;
        self.login.error = None;
        self.login.qr_key = None;
        self.login.qr_url = None;
        self.login.qr_side = 0.0;
    }

    /// 手机号 + 密码登录（`CellphoneLogin`）。
    fn start_cellphone_login(&mut self) {
        let phone = self.login.phone.trim().to_string();
        let password = self.login.password.trim().to_string();
        if phone.is_empty() || password.is_empty() {
            self.login.error = Some(
                self.t("请输入手机号和密码", "Enter a phone number and password")
                    .to_string(),
            );
            return;
        }
        self.login.error = None;
        self.phone_seq = self.bridge.login_cellphone(phone, password);
    }

    /// Cookie 登录（`CookieLogin`）。
    fn start_cookie_login(&mut self) {
        let cookie = self.login.cookie_input.trim().to_string();
        if cookie.is_empty() {
            self.login.error = Some(
                self.t("请粘贴会话 Cookie", "Paste a session cookie")
                    .to_string(),
            );
            return;
        }
        self.login.error = None;
        self.cookie_seq = self.bridge.login_cookie(cookie);
    }

    /// 退出登录（`Logout`）。
    fn start_logout(&mut self) {
        self.logout_seq = self.bridge.logout();
    }

    /// 登录成功统一收口：更新状态、关面板、加密持久化、刷新需要登录的页面。
    fn on_logged_in(&mut self, state: LoginState) {
        self.login.state = Some(state);
        self.qr_poll_at = None;
        let who = self
            .login
            .logged_in(self.lang)
            .unwrap_or_default()
            .to_string();
        self.login.hide();
        self.persist_session();
        self.push_toast(
            self.t(&format!("登录成功：{who}"), &format!("Signed in as {who}"))
                .to_string(),
        );
        // 登录后把需要账号的页面刷新一次（每日推荐）
        if self.page == Page::Daily {
            // 登录前是 301（需要登录），登录后必须换成账号维度的当天推荐
            self.daily_state.reset();
            self.ensure_daily();
        }
        // 账号相关的两份数据：个人歌单 + 收藏双向同步。
        // 都在登录成功后自动跑一次——用户刚扫完码，最想看到的就是"我的东西在了"。
        self.user_pl_loaded = false;
        self.user_pl_state.reset();
        self.ensure_user_playlists();
        self.start_favorite_sync();
    }

    /// 把当前会话 Cookie 加密落盘（登录成功后调用；失败只记日志，不影响登录）。
    fn persist_session(&self) {
        if let Some(cookie) = self.bridge.session_cookie() {
            #[cfg(not(test))]
            {
                if let Err(e) = session_store::save(&session_store::Session { cookie }) {
                    tracing::warn!("会话持久化失败（不影响登录）: {e}");
                }
            }
            // 测试构建不落盘：绝不写用户真实配置目录（同 prefs 纪律）
            #[cfg(test)]
            let _ = cookie;
        }
    }

    /// 清除本地加密会话（退出 / 失效时调用）。
    fn clear_session_store(&self) {
        #[cfg(not(test))]
        session_store::clear();
    }

    /// 每帧推进：按 2s 节奏轮询扫码状态（不轰炸服务端）。
    fn tick_login_poll(&mut self) {
        if !self.login.open || self.login.logged_in(self.lang).is_some() {
            return;
        }
        if self.login.mode != LoginMode::Qr {
            return;
        }
        let Some(key) = self.login.qr_key.clone() else {
            return;
        };
        let Some(due) = self.qr_poll_at else {
            return;
        };
        if self.t < due {
            return;
        }
        self.qr_poll_seq = self.bridge.qr_poll(key);
        // 先推到下一拍，避免响应回来前每帧都发一次
        self.qr_poll_at = Some(self.t + QR_POLL_SECS);
    }

    /// 处理登录面板动作（由 [`login::login_panel`] 回传）。
    fn on_login_action(&mut self, action: LoginAction) {
        match action {
            LoginAction::None => {}
            LoginAction::Close => self.login.hide(),
            LoginAction::Start => self.start_qr_login(),
            LoginAction::Poll(key) => {
                self.qr_poll_seq = self.bridge.qr_poll(key);
            }
            LoginAction::SwitchMode(mode) => {
                self.login.mode = mode;
                self.login.error = None;
                // 切到扫码且尚未有可用二维码时，主动取码
                if mode == LoginMode::Qr && self.login.needs_restart() {
                    self.start_qr_login();
                }
            }
            LoginAction::LoginCellphone => self.start_cellphone_login(),
            LoginAction::LoginCookie => self.start_cookie_login(),
            LoginAction::Logout => self.start_logout(),
        }
    }

    /// 需要登录时的统一引导（FM / 每日歌单等）。
    fn require_login(&mut self, what: &str) {
        self.push_toast(self.t(
            &format!("{what}需要登录，请扫码"),
            &format!("{what} requires sign-in — please scan the QR code"),
        ));
        self.open_login();
    }

    fn submit_search(&mut self) {
        let keyword = self.search_input.trim().to_string();
        if keyword.is_empty() {
            self.push_toast(self.t("请输入搜索关键词", "Enter a search keyword"));
            return;
        }
        // 粘贴的是网易云分享链接/分享文案 → 走链接解析，不当关键词搜
        if self.try_open_share(&keyword) {
            return;
        }
        self.search_seq = self.bridge.search(keyword.clone(), PAGE_LIMIT);
        self.search_state.begin_load();
        self.selected = None;
        // 搜索是明确的「我要看检索结果」动作：清掉其它来源，否则结果面板会继续
        // 显示本地曲库/在线歌单（`current_tracks` 的优先级），看起来像搜索没生效。
        self.clear_user_view();
        self.active_local = false;
        self.active_playlist = None;
        // 本地命中与在线搜索**并行**：没有网络/没登录也能搜到自己的磁带
        self.run_local_search(&keyword);
        self.recent.retain(|r| r != &keyword);
        self.recent.insert(0, keyword);
        self.recent.truncate(5);
    }

    /// 在本地曲库里检索，填充 `local_hits` 与高亮关键词。
    ///
    /// 关键词为空即清空命中（清空搜索框后不该还挂着一堆"结果"）。
    fn run_local_search(&mut self, keyword: &str) {
        let q = keyword.trim();
        if q.is_empty() {
            self.local_hits.clear();
            self.highlight_query.clear();
            return;
        }
        self.highlight_query = q.to_string();
        self.local_hits = self
            .library
            .as_ref()
            .map(|lib| {
                lib.filter(q)
                    .into_iter()
                    .take(LOCAL_HIT_LIMIT)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
    }

    fn submit_playlist(&mut self) {
        let playlist_id = self.playlist_input.trim().to_string();
        if playlist_id.is_empty() {
            self.push_toast(self.t(
                "请输入歌单 ID 或粘贴分享链接",
                "Enter a playlist ID or paste a share link",
            ));
            return;
        }
        if self.try_open_share(&playlist_id) {
            return;
        }
        if self.playlist_state.is_loading() && self.playlist_input == playlist_id {
            return;
        }
        self.playlist_input = playlist_id;
        self.playlist_jump = false;
        self.playlist_autoplay = false;
        self.playlist_seq = self
            .bridge
            .playlist(self.playlist_input.clone(), 0, PAGE_LIMIT);
        self.playlist_state.begin_load();
    }

    /// 切换播放音质上限。
    ///
    /// **不重建 Provider**：`set_quality` 只是往 Provider 的原子量里写一个字节，
    /// 在途请求与登录 Cookie 都不受影响（旧的超时设置靠重建 Provider 生效，
    /// 会顺带丢掉登录态——那条路已经删掉了）。
    fn apply_quality(&mut self, ceiling: QualityCeiling) {
        if self.bridge.quality() == ceiling {
            return;
        }
        self.bridge.set_quality(ceiling);
        // 用户的选择要活过重启（曲库索引是可重建缓存，偏好不是）
        self.persist_prefs();
        let msg = self
            .t(
                &format!(
                    "音质上限：{} ({})",
                    ceiling.label(self.lang),
                    ceiling.detail()
                ),
                &format!(
                    "Quality cap: {} ({})",
                    ceiling.label(crate::i18n::Lang::En),
                    ceiling.detail()
                ),
            )
            .to_string();
        self.push_toast(msg);
    }

    fn any_loading(&self) -> bool {
        self.search_state.is_loading()
            || self.detail_state.is_loading()
            || self.playlist_state.is_loading()
            || self.rec_state.is_loading()
            || self.chart_state.is_loading()
            || self.daily_state.is_loading()
    }

    /// 是否需要 60fps 重绘。
    ///
    /// 舱体机械动作（开舱→退带→装带→落座→合舱）是连续位移，10fps 会明显
    /// 一顿一顿；播放中的真实频谱同理。只有真正静止时才降到 10fps 省电。
    fn needs_smooth_frames(&self) -> bool {
        self.deck_seq.busy()
            || self.core.engine_state() == "Playing"
            || self.any_loading()
            // 暂停后 EQ 柱还在衰减，也需要连续帧把它落干净
            || self.eq_levels.iter().any(|v| *v > 0.002)
    }

    /// 扫描本地曲库目录（启动参数 / 设置面板触发）。
    ///
    /// 成功：入库 + Playlist 页选中本地曲库卡 + 轻提示摘要
    /// （含 `truncated` 如实报告）；失败：人话 toast，不改动既有曲库。
    fn load_library_dir(&mut self, dir: &Path) {
        self.apply_library_dir(dir, true);
    }

    /// [`Self::load_library_dir`] 的实现，`persist` 控制是否写盘。
    ///
    /// 测试走 `persist = false`：单测绝不能往用户真实的配置目录里写索引。
    fn apply_library_dir(&mut self, dir: &Path, persist: bool) {
        match library::scan_dir(dir) {
            Ok(lib) => {
                let count = lib.tracks.len();
                let truncated = lib.truncated;
                // 先落盘再提示：写失败不能假装成功，但内存里的曲库仍可用
                let persist_err = if persist {
                    library::save(&lib).err()
                } else {
                    None
                };
                self.library = Some(lib);
                self.clear_user_view();
                self.active_local = true;
                self.active_playlist = None;
                self.selected = if count == 0 { None } else { Some(0) };
                // 曲库变了 → 离线每日推荐的候选也变了
                self.rebuild_daily_offline();
                if let Some(e) = persist_err {
                    // 日志固定中文（诊断用），界面文案跟语言走
                    tracing::warn!("本地曲库索引保存失败: {}", e.message(Lang::Zh));
                    let reason = e.message(self.lang);
                    self.push_toast(self.t(
                        &format!("曲库已载入，但保存失败（重启后需重扫）：{reason}"),
                        &format!(
                            "Library loaded, but saving failed (rescan next launch): {reason}"
                        ),
                    ));
                    return;
                }
                if count == 0 {
                    self.push_toast(self.t(
                        "该文件夹没有找到音频文件",
                        "No audio files found in that folder",
                    ));
                } else if truncated {
                    self.push_toast(self.t(
                        &format!("曲库已载入 {count} 首（达到上限，列表不完整）"),
                        &format!("Library loaded: {count} tracks (limit reached, list truncated)"),
                    ));
                } else {
                    self.push_toast(self.t(
                        &format!("曲库已载入 {count} 首"),
                        &format!("Library loaded: {count} tracks"),
                    ));
                }
            }
            Err(e) => self.push_toast(e.message(self.lang)),
        }
    }

    // ------------------------------------------------------------------
    // 播放接线
    // ------------------------------------------------------------------

    /// 播放在线曲目（换台自动取消旧装载）。
    ///
    /// 本地曲目（[`ProviderId::Local`]，`id` = 文件路径）走
    /// [`Self::play_local_path`]，**不触网**（规范 6.2 ID 语义分离）。
    fn play_track(&mut self, track: Track) {
        if track.provider == ProviderId::Local {
            self.play_local_path(Path::new(&track.id));
            return;
        }
        self.known.insert(track.id.clone(), track.clone());
        // 历史记录点：换曲即重置「是否已记」标记，真正的写入由 `tick_history`
        // 在「听够」之后触发（见 `history::qualifies`）。
        self.current_track = Some(track.clone());
        self.history_marked = false;
        // 预缓存命中：歌词直接喂给核心，省一次网络往返；未命中走正常请求路径
        let cached = self.lyric_cache.get(&track.id).cloned();
        self.core
            .play_online_with_lyrics(&track, &mut self.bridge, cached);
        self.set_page(Page::Player);
        self.lyric_focus = None;
        self.last_lyric = None;
        self.push_toast(self.t(
            &format!("装载中：{}", track.title),
            &format!("LOADING: {}", track.title.to_uppercase()),
        ));
    }

    /// 播放当前列表选中曲目（以当前浏览列表建立新播放队列）。
    fn play_selected(&mut self) {
        let Some(track) = self
            .current_tracks()
            .get(self.selected.unwrap_or(usize::MAX))
            .cloned()
        else {
            self.push_toast(self.t("请先选择曲目", "Select a track first"));
            return;
        };
        self.adopt_queue();
        self.play_track(track);
    }

    /// 播放本地文件（独立可用，不触网）。
    fn play_local_path(&mut self, path: &Path) {
        // 从在线曲目转去播本地时，在途的下载已经没有意义：不掐的话它会一直
        // 占着串行装载线程，下次点在线曲目还要排在它后面。
        self.bridge.cancel_audio();
        // 引擎马上要改拿本地文件，在线那份不再"在用"（缓存淘汰可以回收它了）
        self.bridge.set_active_audio(None);
        self.core.play_local(path);
        // 本地曲目也要进历史/收藏：核心只拿得到路径，这里补一份曲目快照。
        // 快照默认只有文件名推出来的字——若这张碟已经匹配上过网易云，
        // 就把封面/专辑/时长并进来，否则正在播放面板会退回一张占位图。
        let mut snapshot = library::track_for_path(path);
        if let Some(matched) = self.local_match.get(&snapshot.id).cloned() {
            merge_local_match(&mut snapshot, &matched);
        }
        self.current_track = Some(snapshot);
        self.history_marked = false;
        // 顺手排队识别（幂等：已在途或已匹配直接跳过）。心动模式要的数字
        // 歌曲 ID 也从这里来。
        self.request_local_match(&path.to_string_lossy());
        self.set_page(Page::Player);
        self.lyric_focus = None;
        self.last_lyric = None;
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| self.t("本地文件", "FILE").to_string());
        self.push_toast(self.t(
            &format!("本地播放：{name}"),
            &format!("PLAYING LOCAL: {}", name.to_uppercase()),
        ));
    }

    /// 自然结束后的下一曲（顺序 / 随机 / 列表循环均真实，共用 [`Self::next_index`]）。
    fn auto_next(&mut self) {
        if self.playback_tracks().is_empty() {
            self.core.stop();
            return;
        }
        let Some(next) = self.next_index() else {
            self.push_toast(self.t("播放列表已走完", "End of playlist"));
            self.core.stop();
            return;
        };
        let Some(track) = self.playback_tracks().get(next).cloned() else {
            self.core.stop();
            return;
        };
        self.reveal_in_browse(&track);
        self.play_track(track);
    }

    /// 提前触发封面下载（复用 `cover_tex` / `cover_inflight` / `cover_failed` 三态去重）。
    ///
    /// 只发请求、填在途表，**不创建纹理**——纹理由响应落地时的
    /// [`Self::install_cover`] 统一上传。这样预缓存既不阻塞本帧，也不产生
    /// 用不到的程序化占位纹理。
    fn prefetch_cover(&mut self, url: &str) {
        if self.cover_tex.contains_key(url)
            || self.cover_inflight.contains_key(url)
            || self.cover_failed.contains(url)
            || self.cover_inflight.len() >= COVER_INFLIGHT_LIMIT
        {
            return;
        }
        let seq = self.bridge.cover_image(url);
        self.cover_inflight.insert(url.to_string(), seq);
    }

    /// 当前曲开始播放后，预取「实际下一首」的**音频**、歌词与封面。
    ///
    /// 这是**只读副作用**（只填缓存/在途表），**绝不**调用 `play_track`、
    /// 不改 `selected`、不触发 `auto_next`；真正切歌仍由 `auto_next` / 手动键
    /// 独立驱动。目标用 [`Self::peek_next_index`]（不推进随机游标），
    /// 切歌或播放顺序变化时下一次 `Started` 会重新计算，天然同步更新。
    ///
    /// 音频预取是"连续听歌零等待"的唯一来源：结果只进装载器的会话缓存，
    /// 用户真按下一首时 `prepare_track` 直接命中，连取址请求都不用发。
    fn prefetch_next(&mut self) {
        let Some(next) = self.peek_next_index() else {
            return;
        };
        let Some(track) = self.playback_tracks().get(next).cloned() else {
            return;
        };
        if let Some(url) = track.cover_url.clone() {
            self.prefetch_cover(&url);
        }
        if track.provider == ProviderId::Local {
            return;
        }
        if !self.lyric_cache.contains(&track.id) && !self.lyric_inflight.contains_key(&track.id) {
            let seq = self.bridge.lyrics(track.id.clone());
            self.lyric_inflight.insert(track.id.clone(), seq);
        }
        // 单曲循环 / 单曲列表时"下一首"就是自己，预取它纯属空转
        let same_as_current = self
            .core
            .now_playing()
            .map(|n| n.id == track.id)
            .unwrap_or(false);
        if !same_as_current && self.prefetch_song.as_deref() != Some(track.id.as_str()) {
            self.prefetch_seq = self.bridge.prefetch_track(track.id.clone());
            self.prefetch_song = Some(track.id.clone());
        }
    }

    /// 核心事件 → 反馈。
    fn on_core_event(&mut self, event: CoreEvent) {
        match event {
            CoreEvent::Started => {
                let title = self
                    .core
                    .now_playing()
                    .map(|n| n.title.clone())
                    .unwrap_or_default();
                self.push_toast(self.t(
                    &format!("开始播放：{title}"),
                    &format!("PLAYBACK STARTED: {}", title.to_uppercase()),
                ));
                // 开始播放后预取下一首歌词/封面，减少切歌等待
                self.prefetch_next();
            }
            CoreEvent::Failed(failure) => {
                self.error_modal = Some((
                    self.error_title("PLAYBACK FAILED").to_string(),
                    failure.message(self.lang),
                ));
            }
            CoreEvent::Cancelled => self.push_toast(self.t("已取消装载", "Loading cancelled")),
            CoreEvent::LyricsReady | CoreEvent::Superseded => {}
        }
    }

    // ------------------------------------------------------------------
    // 响应处理（序列号防过期）
    // ------------------------------------------------------------------

    fn drain_responses(&mut self, ctx: &egui::Context) {
        while let Some(response) = self.bridge.poll() {
            // 即将交给引擎的那份文件要"钉住"：缓存淘汰与半成品清扫不得删它。
            // 不能指望 Windows 拒绝删除——`RodioEngine::reload()` 是按路径重开的，
            // 目录项一消失，下一次 play/seek 就 MediaRead 失败。
            if let Response::Prepared { seq, result } = &response {
                if *seq == self.core.prepare_seq {
                    if let Ok(prepared) = result {
                        self.bridge.set_active_audio(Some(prepared.path.clone()));
                    }
                }
            }
            // 播放/歌词/取消优先由核心处理
            if let Some(event) = self.core.on_response(&response) {
                self.on_core_event(event);
                continue;
            }
            match response {
                Response::Search { seq, result } if seq == self.search_seq => {
                    self.set_page(Page::Search);
                    match result {
                        Ok(tracks) => {
                            let n = tracks.len();
                            for t in &tracks {
                                self.known.insert(t.id.clone(), t.clone());
                            }
                            self.selected = if tracks.is_empty() { None } else { Some(0) };
                            self.search_state.finish(Ok(tracks));
                            self.push_toast(
                                self.t(&format!("找到 {n} 条结果"), &format!("{n} results")),
                            );
                        }
                        Err(e) => self.handle_error("SEARCH FAILED", &e),
                    }
                }
                Response::SongDetails { seq, result } if seq == self.detail_seq => match result {
                    Ok(tracks) => {
                        let linked = self.link_song.take();
                        // 链接指向的单曲要在把 tracks 移进状态之前先取出来
                        let hit = linked
                            .as_ref()
                            .and_then(|id| tracks.iter().find(|t| &t.id == id).cloned());
                        self.detail_state.finish(Ok(tracks));
                        match (linked, hit) {
                            // 分享链接解析出的单曲：详情回来直接装带（少一次点击）
                            (Some(_), Some(track)) => self.play_track(track),
                            (Some(_), None) => self.push_toast(self.t(
                                "链接指向的单曲不存在或已下架",
                                "The linked track is missing or unavailable",
                            )),
                            (None, _) => self.push_toast(self.t("详情已更新", "Details updated")),
                        }
                    }
                    Err(e) => {
                        self.link_song = None;
                        self.handle_error("DETAIL FAILED", &e);
                    }
                },
                // 推荐页 / 每日页原地详情：只落进对应面板，**不写进已载入历史**。
                Response::Playlist { seq, result } if self.peek_seq_match(seq).is_some() => {
                    if let Ok(list) = &result {
                        for t in &list.tracks {
                            self.known.insert(t.id.clone(), t.clone());
                        }
                    }
                    if let Some(panel) = self.peek_seq_match(seq) {
                        if let Some(peek) = self.peek_mut(panel) {
                            peek.state.finish(result);
                        }
                    }
                }
                Response::Playlist { seq, result } if seq == self.playlist_seq => {
                    let jump = self.playlist_jump;
                    let autoplay = self.playlist_autoplay;
                    self.playlist_jump = false;
                    self.playlist_autoplay = false;
                    match result {
                        Ok(playlist) => {
                            let n = playlist.tracks.len();
                            let name = playlist.name.clone().unwrap_or_default();
                            let tracks = playlist.tracks.clone();
                            for t in &playlist.tracks {
                                self.known.insert(t.id.clone(), t.clone());
                            }
                            // 历史按 ID 原地更新 + 容量上限；只有用户明确"打开"、
                            // "播放"或本来就在歌单页时才切换当前浏览列表，否则留在原页。
                            let idx = self.remember_loaded_playlist(playlist.clone());
                            let activate = jump || autoplay || self.page == Page::Playlist;
                            if activate {
                                self.clear_user_view();
                                self.active_local = false;
                                self.active_playlist = Some(idx);
                                self.selected = if playlist.tracks.is_empty() {
                                    None
                                } else {
                                    Some(0)
                                };
                            }
                            if jump {
                                self.set_page(Page::Playlist);
                            }
                            self.playlist_state.finish(Ok(playlist));
                            // 显式"播放"：用刚载入的列表建立新队列并从第一首开始。
                            // 与浏览动作分开：只是载入时绝不偷换正在播放的队列。
                            if autoplay {
                                if let Some(first) = tracks.first().cloned() {
                                    self.replace_queue(tracks);
                                    self.play_track(first);
                                }
                            }
                            let hint = if jump || self.page == Page::Playlist {
                                String::new()
                            } else {
                                self.t(" · 在歌单页查看", " · see the Playlist page")
                                    .to_string()
                            };
                            let en_hint = if jump || self.page == Page::Playlist {
                                String::new()
                            } else {
                                " · see the Playlist page".to_string()
                            };
                            self.push_toast(self.t(
                                &format!("歌单「{}」{} 首{hint}", name.trim(), n),
                                &format!("Playlist \"{}\" — {n} tracks{en_hint}", name.trim()),
                            ));
                        }
                        Err(e) => self.handle_error("PLAYLIST FAILED", &e),
                    }
                }
                Response::Featured { seq, result } => {
                    // 两路精选各按序列号归位（过期丢弃）；错误语义与 handle_error 一致
                    if seq == self.rec_seq {
                        match result {
                            Ok(items) => {
                                self.rec_state.finish(Ok(items));
                                self.push_toast(
                                    self.t("推荐歌单已更新", "Recommendations updated"),
                                );
                            }
                            Err(e) => {
                                self.rec_state.finish(Err(e.clone()));
                                self.report_featured_error("REC FAILED", &e);
                            }
                        }
                    } else if seq == self.chart_seq {
                        match result {
                            Ok(items) => {
                                self.chart_state.finish(Ok(items));
                                self.push_toast(
                                    self.t("官方排行榜已更新", "Official charts updated"),
                                );
                            }
                            Err(e) => {
                                self.chart_state.finish(Err(e.clone()));
                                self.report_featured_error("CHART FAILED", &e);
                            }
                        }
                    } else {
                        tracing::debug!("丢弃过期精选响应 seq={}", seq);
                    }
                }
                Response::IntelligenceList { seq, result } if Some(seq) == self.heartbeat_seq => {
                    self.heartbeat_seq = None;
                    match result {
                        Ok(tracks) => self.apply_heartbeat_batch(tracks),
                        Err(e) => {
                            // 失败就当场回滚：键灭、状态清空。留着高亮等于撒谎。
                            let active = self.heartbeat_on || self.heartbeat_pending;
                            self.heartbeat_pending = false;
                            self.heartbeat_on = false;
                            if active {
                                // 还原档位交给用户下一次点击，这里不抢
                                self.heartbeat_prev_mode = None;
                                self.stop_heartbeat();
                            }
                            self.report_login_gate(
                                "HEARTBEAT FAILED",
                                &e,
                                self.t("心动模式", "Heartbeat"),
                            );
                        }
                    }
                }
                Response::MatchLocal { path, result, .. } => match result {
                    Ok(matched) => self.apply_local_match(path, matched),
                    Err(e) => {
                        self.match_inflight.remove(&path);
                        if self.heartbeat_pending {
                            self.heartbeat_pending = false;
                            self.push_toast(self.t(
                                "本地曲目识别失败，开不了心动模式",
                                "LOCAL LOOKUP FAILED — HEARTBEAT UNAVAILABLE",
                            ));
                        }
                        tracing::debug!("本地匹配失败 {path}: {}", e.message(self.lang));
                    }
                },
                Response::DailySongs { seq, result } if seq == self.daily_seq => {
                    match result {
                        Ok(tracks) => {
                            let n = tracks.len();
                            // 记下这批属于哪一天：跨天比较全靠它
                            self.daily_day = super::daily_clock::today_key();
                            self.daily_state.finish(Ok(tracks));
                            self.push_toast(self.t(
                                &format!("今日推荐 {n} 首"),
                                &format!("{n} tracks recommended today"),
                            ));
                        }
                        Err(e) => {
                            self.daily_state.finish(Err(e.clone()));
                            self.report_login_gate(
                                "DAILY FAILED",
                                &e,
                                self.t("每日推荐", "Daily Recommend"),
                            );
                        }
                    }
                }
                Response::LoginStatus { result, .. } => match result {
                    Ok(state) => {
                        // 启动恢复的会话若已失效（有 Cookie 但服务端判匿名），
                        // 清除本地加密会话，避免下次再拿坏 Cookie 去试。
                        let restored = self.bridge.session_cookie().is_some();
                        self.login.state = Some(state.clone());
                        if matches!(state, LoginState::Anonymous) && restored {
                            self.clear_session_store();
                        }
                        // 启动时恢复了有效会话：把账号数据补上（每个会话只跑一次，
                        // `ensure_user_playlists` 与 `start_favorite_sync` 都自带幂等）
                        if !matches!(state, LoginState::Anonymous) && !self.user_pl_loaded {
                            self.ensure_user_playlists();
                            self.start_favorite_sync();
                        }
                    }
                    // 状态查询失败不该弹错误框：登录只是可选能力
                    Err(e) => tracing::debug!("登录状态查询失败: {e}"),
                },
                Response::QrStarted { seq, result } if seq == self.qr_seq => match result {
                    Ok(session) => {
                        // 先按内容算出模块数，再取整数模块像素 → 纹理可 1:1 落在面板里。
                        let modules = qr::qr_modules(&session.qr_url).unwrap_or(37);
                        let module_px = qr_module_px(modules);
                        self.qr_texture = qr::qr_image(&session.qr_url, module_px).map(|image| {
                            ctx.load_texture(
                                "reimu-player-login-qr",
                                image,
                                TextureOptions::NEAREST,
                            )
                        });
                        // 记下自然边长供绘制时 1:1 落框（不缩放）
                        self.login.qr_side = ((modules + 8) * module_px) as f32;
                        self.login.qr_url = Some(session.qr_url);
                        self.login.qr_key = Some(session.key);
                        self.login.state = Some(LoginState::QrWaiting);
                        self.login.error = None;
                        self.login.started = true;
                        // 首次轮询延后一拍，给用户一个"看到二维码"的窗口
                        self.qr_poll_at = Some(self.t + QR_POLL_SECS);
                    }
                    Err(e) => {
                        self.login.error = Some(e.to_string());
                        self.qr_poll_at = None;
                    }
                },
                Response::QrPolled { seq, result } if seq == self.qr_poll_seq => match result {
                    Ok(state) => {
                        self.qr_poll_fails = 0;
                        let expired = state == LoginState::QrExpired;
                        let logged_in = matches!(state, LoginState::LoggedIn { .. });
                        if logged_in {
                            self.on_logged_in(state);
                        } else {
                            self.login.state = Some(state);
                            if expired {
                                self.qr_poll_at = None;
                                self.qr_texture = None;
                                self.login.qr_side = 0.0;
                                self.login.error = Some(
                                    self.t(
                                        "二维码已过期，请重新获取",
                                        "QR code expired — please request a new one",
                                    )
                                    .to_string(),
                                );
                            } else {
                                // 继续轮询（等待扫码 / 已扫码待确认）
                                self.qr_poll_at = Some(self.t + QR_POLL_SECS);
                            }
                        }
                    }
                    Err(e) => {
                        // 网络抖动不该终止轮询：用户可能刚扫完码，正等这一拍
                        // 的结果。重试到上限才停，错误文案也带上重试进度。
                        self.qr_poll_fails += 1;
                        if self.qr_poll_fails < QR_POLL_MAX_FAILS {
                            tracing::warn!(
                                "二维码轮询失败（第 {}/{} 次），继续重试: {e}",
                                self.qr_poll_fails,
                                QR_POLL_MAX_FAILS
                            );
                            self.qr_poll_at = Some(self.t + QR_POLL_SECS);
                        } else {
                            self.login.error = Some(
                                self.t(
                                    &format!("{e}（已重试 {} 次）", self.qr_poll_fails),
                                    &format!("{e} (retried {} times)", self.qr_poll_fails),
                                )
                                .to_string(),
                            );
                            self.qr_poll_at = None;
                        }
                    }
                },
                Response::CellphoneLoggedIn { seq, result } if seq == self.phone_seq => {
                    match result {
                        Ok(state) => self.on_logged_in(state),
                        Err(e) => self.login.error = Some(e.message(self.lang)),
                    }
                }
                Response::CookieLoggedIn { seq, result } if seq == self.cookie_seq => {
                    match result {
                        Ok(state) => self.on_logged_in(state),
                        Err(e) => self.login.error = Some(e.message(self.lang)),
                    }
                }
                Response::LoggedOut { seq, result } if seq == self.logout_seq => {
                    // 退出：无论网络侧成败，本地都清会话 + 置匿名（本地已不可信）
                    self.clear_session_store();
                    self.login.state = Some(LoginState::Anonymous);
                    // 账号数据一并清掉：留在界面上会让人以为还登录着
                    self.user_playlists.clear();
                    self.user_pl_state.reset();
                    // 置 true 表示"这一轮不必再拉"（未登录拉会直接报错）
                    self.user_pl_loaded = true;
                    self.sync.reset();
                    self.shelf_target = None;
                    self.shelf_back = None;
                    self.album_tracks.clear();
                    self.artist_tracks.clear();
                    if matches!(self.user_view, UserView::Album | UserView::Artist) {
                        self.user_view = UserView::None;
                    }
                    if result.is_ok() {
                        self.push_toast(self.t("已退出登录", "Signed out"));
                    } else {
                        self.push_toast(self.t(
                            "已退出登录（服务端未确认）",
                            "Signed out (server not confirmed)",
                        ));
                    }
                }
                Response::Lyrics { seq, result } => {
                    // 预取歌词落地（当前曲的歌词已由 core 消费，这里只可能是预取）。
                    if let Some(id) = self
                        .lyric_inflight
                        .iter()
                        .find(|(_, s)| **s == seq)
                        .map(|(id, _)| id.clone())
                    {
                        self.lyric_inflight.remove(&id);
                        if let Ok(lyrics) = result {
                            self.lyric_cache.insert(id, lyrics);
                        }
                    }
                }
                Response::Expanded { url, result, .. } => {
                    if self.link_pending.as_deref() != Some(url.as_str()) {
                        tracing::debug!("丢弃过期短链展开响应: {url}");
                    } else {
                        self.link_pending = None;
                        match result {
                            Ok(final_url) => match parse_share(&final_url) {
                                Some(ShareRef::Resolved { kind, id }) => {
                                    self.open_share_target(kind, &id)
                                }
                                Some(ShareRef::Unsupported { segment }) => self.push_toast(
                                    self.t(
                                        &format!("暂不支持 /{segment} 类型链接"),
                                        &format!("Unsupported link type /{segment}"),
                                    )
                                    .to_string(),
                                ),
                                _ => self.push_toast(
                                    self.t(
                                        "短链展开后仍无法识别目标",
                                        "Could not resolve the expanded link",
                                    )
                                    .to_string(),
                                ),
                            },
                            Err(e) => self.push_toast(
                                self.t(
                                    &format!("短链展开失败：{e}"),
                                    &format!("Failed to expand the link: {e}"),
                                )
                                .to_string(),
                            ),
                        }
                    }
                }
                Response::CoverImage { url, result, .. } => {
                    self.cover_inflight.remove(&url);
                    match result {
                        // 已经是烘好的 RGBA（解码 + 网纹在后台线程完成）
                        Ok(rgba) => self.install_cover(ctx, url, &rgba),
                        Err(e) => {
                            tracing::debug!("封面拉取失败（回退程序化封面）: {url} / {e}");
                            self.cover_failed.insert(url);
                        }
                    }
                }
                Response::Liked { seq, like, result } if seq == self.like_seq => {
                    match result {
                        Ok(()) => {
                            // 服务端已确认，本地镜像跟着走（UI 状态与离线展示都靠它）
                            if let Some(track) = self.like_target.take() {
                                if self.favorites.contains(&track) != like {
                                    self.favorites.toggle(&track, super::history::now_secs());
                                    self.persist_user_data();
                                    if self.user_view == UserView::Favorites {
                                        self.refresh_user_views();
                                    }
                                }
                            }
                            let msg = if like {
                                self.t("已加入「我喜欢的音乐」", "ADDED TO YOUR LIKED SONGS")
                            } else {
                                self.t("已从「我喜欢的音乐」移除", "REMOVED FROM YOUR LIKED SONGS")
                            };
                            self.push_toast(msg);
                        }
                        Err(e) => {
                            // 写操作不重试：失败就把 UI 打回原状并如实说明
                            self.like_target = None;
                            self.push_toast(e.message(self.lang));
                        }
                    }
                }
                // ---- 收藏双向同步（四段 seq 各自归位）----
                Response::LikedSongs { seq, result } if seq == self.sync.ids_seq => {
                    self.on_sync_ids(result);
                }
                Response::SongDetails { seq, result } if seq == self.sync.details_seq => {
                    self.on_sync_details(result);
                }
                Response::Liked { seq, result, .. } if seq == self.sync.push_seq => {
                    self.sync.push_seq = 0;
                    if result.is_ok() {
                        self.sync.pushed += 1;
                    } else {
                        // 单首失败不中断整轮：一首没权限不该让后面 200 首都停
                        self.sync.failed += 1;
                    }
                    self.push_next_favorite();
                }
                // ---- 专辑 / 歌手页（复用磁带架）----
                Response::AlbumSongs {
                    seq,
                    album_id,
                    result,
                } if seq == self.album_seq => {
                    // 期间用户可能已经点了别的专辑：id 对不上就当过期响应丢弃
                    let current = self
                        .shelf_target
                        .as_ref()
                        .map(|t| t.id.as_str())
                        .unwrap_or_default();
                    if current != album_id {
                        tracing::debug!("丢弃过期专辑响应 id={album_id}");
                    } else {
                        self.apply_shelf_result(ShelfKind::Album, result);
                    }
                }
                Response::ArtistSongs {
                    seq,
                    artist_id,
                    result,
                } if seq == self.artist_seq => {
                    let current = self
                        .shelf_target
                        .as_ref()
                        .map(|t| t.id.as_str())
                        .unwrap_or_default();
                    if current != artist_id {
                        tracing::debug!("丢弃过期歌手响应 id={artist_id}");
                    } else {
                        self.apply_shelf_result(ShelfKind::Artist, result);
                    }
                }
                // ---- 我的歌单（网易云个人歌单）----
                Response::UserPlaylists { seq, result } if seq == self.user_pl_seq => {
                    self.user_pl_seq = 0;
                    match result {
                        Ok(lists) => {
                            self.user_playlists = lists;
                            self.user_pl_state.finish(Ok(()));
                            self.user_pl_loaded = true;
                        }
                        Err(e) => {
                            self.user_pl_state.reset();
                            self.user_pl_loaded = true;
                            // 未登录/网络失败都只轻提示：歌单面板空着不影响播放
                            self.push_toast(e.message(self.lang));
                        }
                    }
                }
                // 取消回执：**必须销账**。UI 侧的在途登记（`*_seq` / `*_inflight`）
                // 是"同一个请求只发一次"的唯一依据，不注销就等于永久放弃这个请求。
                Response::Cancelled { seq } => self.forget_inflight(seq),
                // 预取落地：只清账，不播放、不提示。
                //
                // 真正有用的东西已经在装载器的会话缓存里了——用户随后点这首歌时
                // `prepare_track` 直接命中，连取址请求都不用发。
                Response::Prefetched { seq, .. } if seq == self.prefetch_seq => {
                    self.prefetch_seq = 0;
                    self.prefetch_song = None;
                }
                // 过期预取（已换台/已切歌）：同样清账，否则那首歌永远不再被预取
                Response::Prefetched { .. } => {}
                // 过期响应：静默丢弃（审计报告「搜索超时即放弃过期结果」）
                other => {
                    tracing::debug!("丢弃过期响应 seq={}", other.seq());
                }
            }
        }
    }

    /// 请求被取消后的清账（换台、用户取消、请求队列溢出都会走到这里）。
    ///
    /// 为什么这一步不能省：换台会抬一次取消水位（音频 lane 由
    /// [`super::bridge::ProviderBridge::cancel_audio`] 抬，元数据 lane 只在
    /// 显式取消时抬），把排在后面的封面、本地匹配、歌词请求整批作废。
    /// 作废本身是对的，但如果 UI 不把这些请求从"在途"里摘掉，那些表就永远记着
    /// "这个 URL 还在拉"——于是封面再也不会有人去拉（磁带架永远一张真图都没有）、
    /// 列表永远停在转圈。这就是"加载慢 / 加载不出来"里最难自己恢复的那一类。
    fn forget_inflight(&mut self, seq: u64) {
        if seq == 0 {
            return;
        }
        // 按 URL / 路径记在途的三张表：不摘掉就是永久放弃这个键
        for map in [
            &mut self.cover_inflight,
            &mut self.match_inflight,
            &mut self.lyric_inflight,
        ] {
            map.retain(|_, s| *s != seq);
        }

        // 音频预取被掐掉也要清账：不清的话那首歌会被记成"已预取"，
        // 于是**永远不再被预取**——用户看到的是"每次切到它都要等整首下载"。
        if self.prefetch_seq == seq {
            self.prefetch_seq = 0;
            self.prefetch_song = None;
        }

        // 单飞 seq：命中就把登记清零，并把对应的加载状态放回 Idle（否则转圈）
        macro_rules! clear {
            ($field:expr, $state:expr) => {
                if $field == seq {
                    $field = 0;
                    $state.reset();
                }
            };
            ($field:expr) => {
                if $field == seq {
                    $field = 0;
                }
            };
        }
        clear!(self.search_seq, self.search_state);
        clear!(self.detail_seq, self.detail_state);
        clear!(self.playlist_seq, self.playlist_state);
        self.playlist_autoplay = false;
        clear!(self.rec_seq, self.rec_state);
        clear!(self.chart_seq, self.chart_state);
        clear!(self.daily_seq, self.daily_state);
        clear!(self.album_seq, self.album_state);
        clear!(self.artist_seq, self.artist_state);
        clear!(self.user_pl_seq, self.user_pl_state);
        clear!(self.qr_seq);
        clear!(self.qr_poll_seq);
        clear!(self.phone_seq);
        clear!(self.cookie_seq);
        clear!(self.logout_seq);
        if self.link_seq == seq {
            self.link_seq = 0;
            self.link_pending = None;
        }
        if self.like_seq == seq {
            self.like_seq = 0;
            self.like_target = None;
        }
        if self.heartbeat_seq == Some(seq) {
            self.heartbeat_seq = None;
            self.heartbeat_pending = false;
        }
        // 收藏同步三段各有后继动作：按"这一路失败"走，整轮才不会卡在队列里
        if self.sync.ids_seq == seq {
            self.on_sync_ids(Err(ProviderError::Cancelled));
        }
        if self.sync.details_seq == seq {
            self.on_sync_details(Err(ProviderError::Cancelled));
        }
        if self.sync.push_seq == seq {
            self.sync.push_seq = 0;
            self.sync.failed += 1;
            self.push_next_favorite();
        }
    }

    /// 错误框标题：**路由键 → 界面文案**。
    ///
    /// 键保持英文稳定串（[`Self::handle_error`] 靠它把状态归位到对应的
    /// `LoadState`），这里只翻译用户看到的那一份。
    fn error_title<'a>(&self, key: &'a str) -> &'a str {
        match key {
            "SEARCH FAILED" => self.t("搜索失败", "SEARCH FAILED"),
            "DETAIL FAILED" => self.t("详情获取失败", "DETAIL FAILED"),
            "PLAYLIST FAILED" => self.t("歌单获取失败", "PLAYLIST FAILED"),
            "REC FAILED" => self.t("推荐获取失败", "REC FAILED"),
            "CHART FAILED" => self.t("榜单获取失败", "CHART FAILED"),
            "DAILY FAILED" => self.t("每日歌单获取失败", "DAILY FAILED"),
            "FM FAILED" => self.t("私人 FM 获取失败", "FM FAILED"),
            "PLAYBACK FAILED" => self.t("播放失败", "PLAYBACK FAILED"),
            // 未知键原样显示：宁可露出英文，也不吞掉真实信息
            other => other,
        }
    }

    fn handle_error(&mut self, title: &str, error: &ProviderError) {
        match error {
            // 空结果不是错误：轻提示
            ProviderError::EmptyResult => {
                match title {
                    "SEARCH FAILED" => self.search_state.finish(Err(error.clone())),
                    "DETAIL FAILED" => self.detail_state.finish(Err(error.clone())),
                    _ => self.playlist_state.finish(Err(error.clone())),
                }
                self.push_toast(self.t("没有找到匹配结果", "No matching results"));
            }
            other => {
                match title {
                    "SEARCH FAILED" => self.search_state.finish(Err(other.clone())),
                    "DETAIL FAILED" => self.detail_state.finish(Err(other.clone())),
                    _ => self.playlist_state.finish(Err(other.clone())),
                }
                // 人话提示（规范 8.2：不显示原始 JSON / 堆栈），文案跟界面语言
                self.error_modal = Some((
                    self.error_title(title).to_string(),
                    other.message(self.lang),
                ));
            }
        }
    }

    /// 精选列表错误提示（状态已由响应臂归位，这里只做 8.2 人话反馈）。
    fn report_featured_error(&mut self, title: &str, error: &ProviderError) {
        match error {
            // 空结果不是错误：轻提示
            ProviderError::EmptyResult => {
                self.push_toast(self.t("没有找到匹配结果", "No matching results"))
            }
            other => {
                self.error_modal = Some((
                    self.error_title(title).to_string(),
                    other.message(self.lang),
                ));
            }
        }
    }

    /// 需登录页面的错误提示：`AuthenticationRequired` 不弹错误框，直接引导扫码。
    ///
    /// 理由：这是**功能门槛**而不是故障，弹「该功能需要登录」的模态框等于把用户
    /// 挡在门外不给出口；直接把登录面板打开才是可执行的下一步。
    fn report_login_gate(&mut self, title: &str, error: &ProviderError, what: &str) {
        match error {
            ProviderError::AuthenticationRequired => self.require_login(what),
            ProviderError::AuthenticationExpired => {
                self.push_toast(self.t(
                    "登录已过期，请重新扫码",
                    "Sign-in expired — please scan again",
                ));
                self.login.state = Some(LoginState::Anonymous);
                self.open_login();
            }
            other => self.report_featured_error(title, other),
        }
    }

    // ------------------------------------------------------------------
    // 展示辅助
    // ------------------------------------------------------------------

    /// 当前列表（本地曲库 / 活动歌单 / 搜索结果，互斥优先级从左到右）。
    ///
    /// 磁带架与 `auto_next` 都以这里为唯一事实源，保证「界面显示」与「实际播放
    /// 顺序」一致。
    fn current_tracks(&self) -> &[Track] {
        // 用户数据视图优先级最高：用户刚点了「我的收藏」，界面必须立刻跟着换。
        match self.user_view {
            UserView::Favorites => return &self.favorites_view,
            UserView::History => return &self.history_view,
            UserView::LocalPlaylist(_) => return &self.local_pl_view,
            UserView::Album => return &self.album_tracks,
            UserView::Artist => return &self.artist_tracks,
            UserView::None => {}
        }
        if self.active_local {
            return self
                .library
                .as_ref()
                .map(|l| l.tracks.as_slice())
                .unwrap_or(&[]);
        }
        if let Some(p) = self.active_playlist.and_then(|i| self.playlists.get(i)) {
            return p.tracks.as_slice();
        }
        self.search_state.get().map(Vec::as_slice).unwrap_or(&[])
    }

    /// 播放队列（最近一次开始播放时的列表快照）。
    ///
    /// 浏览列表（[`Self::current_tracks`]）可以随载入歌单/搜索变化；播放队列
    /// 只在用户明确点播放时更新——这样"打开一张歌单看看"不会偷换下一首。
    fn playback_tracks(&self) -> &[Track] {
        if self.queue_active {
            &self.queue
        } else {
            self.current_tracks()
        }
    }

    fn replace_queue(&mut self, tracks: Vec<Track>) {
        self.queue = tracks;
        self.queue_active = true;
        self.shuffle_order = None;
    }

    /// 用当前浏览列表建立播放队列（用户点 ▶ / 双击行 / 播放全部时调用）。
    fn adopt_queue(&mut self) {
        let tracks = self.current_tracks().to_vec();
        self.replace_queue(tracks);
    }

    /// 确保队列存在（右键「下一首播放 / 加入队列」在尚未播放时也能工作）。
    fn ensure_queue(&mut self) {
        if !self.queue_active {
            self.adopt_queue();
        }
    }

    /// 队列中正在播放曲目的位置（优先取引擎真实曲目，其次选中项）。
    fn queue_cursor(&self) -> Option<usize> {
        let tracks = self.playback_tracks();
        if let Some(id) = self.core.now_playing().map(|n| n.id.clone()) {
            if let Some(pos) = tracks.iter().position(|t| t.id == id) {
                return Some(pos);
            }
        }
        self.selected.filter(|i| *i < tracks.len())
    }

    /// 切到某个用户数据视图（收藏 / 最近播放 / 自建歌单）。
    ///
    /// 会重建对应的 `*_view` 快照并清掉其它来源，保证「界面显示的列表」与
    /// 「`next` / 播放用的列表」是同一份。
    fn select_user_view(&mut self, view: UserView) {
        self.user_view = view;
        self.active_local = false;
        self.active_playlist = None;
        self.selected = None;
        self.refresh_user_views();
        let len = self.current_tracks().len();
        if len > 0 {
            self.selected = Some(0);
        }
    }

    /// 离开用户数据视图（切回本地曲库 / 在线歌单 / FM 时调用）。
    fn clear_user_view(&mut self) {
        self.user_view = UserView::None;
    }

    /// 按持久化数据重建三份视图快照。
    ///
    /// 只在「数据变更」或「进入视图」时调用，不在每帧调用：`current_tracks`
    /// 返回的是引用，每帧重建会让借用关系无法成立，也白耗 CPU。
    fn refresh_user_views(&mut self) {
        self.favorites_view = self.favorites.tracks();
        self.history_view = self
            .history
            .recent(self.history_hours, super::history::now_secs())
            .into_iter()
            .map(|r| r.track.clone())
            .collect();
        self.local_pl_view = match self.user_view {
            UserView::LocalPlaylist(id) => self
                .local_playlists
                .get(id)
                .map(|p| p.tracks())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
    }

    /// 当前视图的标题（Playlist 页页头用）。
    fn user_view_title(&self) -> Option<String> {
        match self.user_view {
            UserView::None => None,
            UserView::Favorites => Some(self.t("我的收藏", "FAVORITES").to_string()),
            UserView::History => Some(self.t("最近播放", "RECENTLY PLAYED").to_string()),
            UserView::LocalPlaylist(id) => self
                .local_playlists
                .get(id)
                .map(|p| p.name.clone())
                .or_else(|| Some(self.t("自建歌单", "LOCAL PLAYLIST").to_string())),
            // 磁带架标题按用户原话写：「专辑曲子」「歌手曲子」。
            // 具体是哪张专辑/哪位歌手由副标题与返回按钮交代，标题保持稳定，
            // 否则标题会随名字长短抖动，磁带架看起来在跳。
            UserView::Album => Some(self.t("专辑曲子", "ALBUM TRACKS").to_string()),
            UserView::Artist => Some(self.t("歌手曲子", "ARTIST TRACKS").to_string()),
        }
    }

    /// 磁带架当前标题（未进专辑/艺人页时是「磁带架」）。
    fn shelf_title(&self) -> &'static str {
        match self.shelf_target.as_ref().map(|t| t.kind) {
            Some(ShelfKind::Album) => self.t("专辑曲子", "ALBUM TRACKS"),
            Some(ShelfKind::Artist) => self.t("歌手曲子", "ARTIST TRACKS"),
            None => self.t("磁带架", "Tape Shelf"),
        }
    }

    /// 记下进入专辑/艺人页之前的磁带架状态。
    fn shelf_snapshot(&self) -> ShelfSnapshot {
        ShelfSnapshot {
            view: self.user_view,
            selected: self.selected,
            active_local: self.active_local,
            active_playlist: self.active_playlist,
        }
    }

    /// 打开专辑页（复用磁带架显示区）。
    ///
    /// 只在**从非专辑/艺人页进入**时保存快照：从专辑页点歌手、再点回专辑这类
    /// 连续跳转，返回要回到最初的磁带架，而不是回到上一张专辑。
    fn open_album_page(&mut self, album_id: &str, name: &str) {
        let id = album_id.trim();
        if id.is_empty() {
            self.push_toast(self.t("这张磁带没有专辑信息", "This tape has no album information"));
            return;
        }
        if !matches!(self.user_view, UserView::Album | UserView::Artist) {
            self.shelf_back = Some(self.shelf_snapshot());
        }
        self.shelf_target = Some(ShelfTarget {
            kind: ShelfKind::Album,
            id: id.to_string(),
            name: name.to_string(),
        });
        self.album_tracks.clear();
        self.album_seq = self.bridge.album_songs(id.to_string());
        self.album_state.begin_load();
        self.select_user_view(UserView::Album);
        // 磁带架在 Player 页右列：跳转必须把人带过去，否则点了像没反应
        self.set_page(Page::Player);
    }

    /// 打开歌手页（复用磁带架显示区）。
    fn open_artist_page(&mut self, artist_id: &str, name: &str) {
        let id = artist_id.trim();
        if id.is_empty() {
            self.push_toast(self.t(
                "这位歌手没有可跳转的主页",
                "This artist has no page to open",
            ));
            return;
        }
        if !matches!(self.user_view, UserView::Album | UserView::Artist) {
            self.shelf_back = Some(self.shelf_snapshot());
        }
        self.shelf_target = Some(ShelfTarget {
            kind: ShelfKind::Artist,
            id: id.to_string(),
            name: name.to_string(),
        });
        self.artist_tracks.clear();
        self.artist_seq = self.bridge.artist_songs(id.to_string(), ARTIST_SONG_LIMIT);
        self.artist_state.begin_load();
        self.select_user_view(UserView::Artist);
        self.set_page(Page::Player);
    }

    /// 从专辑/艺人页返回磁带架（恢复进入前的来源与选中行）。
    ///
    /// 不重发任何请求：`search_state` / `playlists` / `library` 都还在原处，
    /// 恢复视图来源即可，磁带架内容立刻回来。
    fn back_to_shelf(&mut self) {
        self.shelf_target = None;
        let Some(snapshot) = self.shelf_back.take() else {
            // 没有快照（理论上不该发生）：退到"无视图"，至少不留一张空专辑页
            self.user_view = UserView::None;
            self.selected = None;
            return;
        };
        self.user_view = snapshot.view;
        self.active_local = snapshot.active_local;
        self.active_playlist = snapshot.active_playlist;
        self.refresh_user_views();
        let len = self.current_tracks().len();
        self.selected = match snapshot.selected {
            Some(i) if i < len => Some(i),
            _ if len > 0 => Some(0),
            _ => None,
        };
    }

    /// 专辑/艺人曲目落地：写进对应缓存 + 选中第一行。
    fn apply_shelf_result(&mut self, kind: ShelfKind, result: Result<Vec<Track>, ProviderError>) {
        match result {
            Ok(tracks) => {
                let n = tracks.len();
                for t in &tracks {
                    self.known.insert(t.id.clone(), t.clone());
                }
                match kind {
                    ShelfKind::Album => {
                        self.album_state.finish(Ok(()));
                        self.album_tracks = tracks;
                    }
                    ShelfKind::Artist => {
                        self.artist_state.finish(Ok(()));
                        self.artist_tracks = tracks;
                    }
                }
                self.selected = if n == 0 { None } else { Some(0) };
                if n == 0 {
                    self.push_toast(self.t("这里没有可播放的曲子", "NO PLAYABLE TRACKS HERE"));
                }
            }
            Err(e) => {
                let msg = e.message(self.lang);
                match kind {
                    ShelfKind::Album => self.album_state.finish(Err(e)),
                    ShelfKind::Artist => self.artist_state.finish(Err(e)),
                }
                self.push_toast(msg);
            }
        }
    }

    /// 收藏 / 取消收藏（本地镜像，不触网）。
    ///
    /// 只用于**本地曲目**与响应落地后的镜像写入。在线曲目要走
    /// [`Self::toggle_like`]——那才会写进账号的「我喜欢的音乐」。
    fn toggle_favorite(&mut self, track: &Track) -> bool {
        let now = super::history::now_secs();
        let on = self.favorites.toggle(track, now);
        self.persist_user_data();
        // 正在看收藏列表时立刻反映增删（取消收藏的那行应当消失）
        if self.user_view == UserView::Favorites {
            self.refresh_user_views();
            let len = self.current_tracks().len();
            self.selected = if len == 0 { None } else { Some(0) };
        }
        let msg = if on {
            self.t("已加入收藏", "ADDED TO FAVORITES")
        } else {
            self.t("已取消收藏", "REMOVED FROM FAVORITES")
        };
        self.push_toast(msg);
        on
    }

    /// 红心：按曲目来源分流到「官方喜欢」或「本地收藏」。
    ///
    /// - 本地文件：没有云端入口，只存本地；
    /// - 在线曲目：必须登录，走网易云 `/api/song/like` 真写进账号歌单。
    ///   未登录时**不改状态**，只提示去登录——假装成功会让用户以为已经存进云端。
    /// - 已有在途请求时直接忽略：写操作连点两次可能把状态翻回来。
    fn toggle_like(&mut self, track: &Track) {
        if track.provider == ProviderId::Local {
            self.toggle_favorite(track);
            return;
        }
        if self.like_target.is_some() {
            tracing::debug!("红心请求在途，忽略重复点击");
            return;
        }
        if self.login.logged_in(self.lang).is_none() {
            self.push_toast(self.t(
                "登录后才能同步到「我喜欢的音乐」",
                "SIGN IN TO SYNC TO YOUR LIKED SONGS",
            ));
            self.open_login();
            return;
        }
        let like = !self.favorites.contains(track);
        self.like_seq = self.bridge.like_song(track.id.clone(), like);
        self.like_target = Some(track.clone());
        let msg = if like {
            self.t("正在加入「我喜欢的音乐」…", "ADDING TO LIKED SONGS…")
        } else {
            self.t("正在取消喜欢…", "REMOVING FROM LIKED SONGS…")
        };
        self.push_toast(msg);
    }

    /// 红心是否处于"已喜欢"（本地镜像；在途时保持原状，避免闪一下又弹回）。
    fn is_liked(&self, track: &Track) -> bool {
        self.favorites.contains(track)
    }

    /// 落盘三份用户数据（测试环境不写真实目录）。
    fn persist_user_data(&self) {
        #[cfg(not(test))]
        {
            self.favorites.save();
            self.history.save();
            self.local_playlists.save();
        }
    }

    /// 新建自建歌单并切到它。
    fn create_local_playlist(&mut self) {
        let name = self.new_playlist_name.trim().to_string();
        if name.is_empty() {
            self.push_toast(self.t("请输入歌单名称", "Enter a playlist name"));
            return;
        }
        if self.local_playlists.has_name(&name) {
            self.push_toast(self.t(
                "已有同名歌单，请换一个名字",
                "A playlist with that name already exists",
            ));
            return;
        }
        let id = self
            .local_playlists
            .create(&name, super::history::now_secs());
        self.new_playlist_name.clear();
        self.persist_user_data();
        self.select_user_view(UserView::LocalPlaylist(id));
        self.push_toast(self.t(
            &format!("已新建歌单：{name}"),
            &format!("PLAYLIST CREATED: {}", name.to_uppercase()),
        ));
    }

    /// 把一首曲目加入自建歌单（重复加入返回 `false`，不刷屏）。
    fn add_to_local_playlist(&mut self, id: u64, track: &Track) {
        if !self.local_playlists.add_track(id, track) {
            self.push_toast(self.t("这首已在歌单里", "ALREADY IN THIS PLAYLIST"));
            return;
        }
        self.persist_user_data();
        if self.user_view == UserView::LocalPlaylist(id) {
            self.refresh_user_views();
        }
        self.push_toast(self.t(
            &format!("已加入：{}", track.title),
            &format!("ADDED: {}", track.title.to_uppercase()),
        ));
    }

    /// 提交重命名（`rename_target` 为空时 no-op）。
    fn commit_rename_playlist(&mut self) {
        let Some(id) = self.rename_target else { return };
        let name = self.rename_input.trim().to_string();
        if name.is_empty() {
            self.rename_target = None;
            self.rename_input.clear();
            return;
        }
        if self.local_playlists.rename(id, &name) {
            self.persist_user_data();
            self.push_toast(self.t("歌单已重命名", "PLAYLIST RENAMED"));
        }
        self.rename_target = None;
        self.rename_input.clear();
    }

    /// 请求删除自建歌单（弹出模态确认；不依赖 3 秒计时窗口）。
    fn request_delete_local_playlist(&mut self, id: u64) {
        let name = self
            .local_playlists
            .get(id)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        self.confirm = Some(ConfirmDialog {
            title: self
                .t("删除歌单 / DELETE PLAYLIST", "DELETE PLAYLIST")
                .to_string(),
            message: self
                .t(
                    &format!(
                        "「{name}」及其中的 {} 首曲目记录将被删除，此操作不可撤销。",
                        self.local_playlists
                            .get(id)
                            .map(|p| p.entries.len())
                            .unwrap_or(0)
                    ),
                    &format!(
                        "\"{name}\" and its track records will be deleted. This cannot be undone."
                    ),
                )
                .to_string(),
            confirm_label: self.t("删除", "DELETE").to_string(),
            action: ConfirmAction::DeleteLocalPlaylist(id),
        });
    }

    /// 真正执行删除：数据、当前视图、重命名目标、右键菜单一起清理。
    fn delete_local_playlist(&mut self, id: u64) {
        self.local_playlists.delete(id);
        self.persist_user_data();
        if self.user_view == UserView::LocalPlaylist(id) {
            self.select_user_view(UserView::None);
        }
        if self.rename_target == Some(id) {
            self.rename_target = None;
            self.rename_input.clear();
        }
        self.push_toast(self.t("歌单已删除", "PLAYLIST DELETED"));
    }

    /// 请求把一张已载入歌单移出历史（模态确认）。
    fn request_remove_loaded_playlist(&mut self, id: String) {
        let name = self
            .playlists
            .iter()
            .find(|p| p.id == id)
            .and_then(|p| p.name.clone())
            .unwrap_or_else(|| id.clone());
        self.confirm = Some(ConfirmDialog {
            title: self
                .t("移出已载入歌单", "REMOVE LOADED PLAYLIST")
                .to_string(),
            message: self.t(
                &format!("「{name}」将从左侧列表中移除（不影响云端歌单与正在播放的曲目）。"),
                &format!("\"{name}\" will be removed from the list. The cloud playlist and playback are unaffected."),
            ).to_string(),
            confirm_label: self.t("移出", "REMOVE").to_string(),
            action: ConfirmAction::RemoveLoadedPlaylist(id),
        });
    }

    /// 请求从账号歌单列表隐藏（仅本地，模态确认）。
    fn request_hide_account_playlist(&mut self, id: String) {
        let name = self
            .user_playlists
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| id.clone());
        self.confirm = Some(ConfirmDialog {
            title: self
                .t("隐藏账号歌单", "HIDE ACCOUNT PLAYLIST")
                .to_string(),
            message: self.t(
                &format!("「{name}」将从列表隐藏，直到下次刷新账号歌单。云端数据不受影响。"),
                &format!("\"{name}\" will be hidden until the next account sync. Cloud data is unaffected."),
            ).to_string(),
            confirm_label: self.t("隐藏", "HIDE").to_string(),
            action: ConfirmAction::HideAccountPlaylist(id),
        });
    }

    /// 确认模态返回后的动作分发。
    fn apply_confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::DeleteLocalPlaylist(id) => self.delete_local_playlist(id),
            ConfirmAction::RemoveLoadedPlaylist(id) => self.remove_loaded_playlist(&id),
            ConfirmAction::HideAccountPlaylist(id) => self.hide_account_playlist(&id),
        }
    }

    /// 把选中行在自建歌单内上移/下移一位。
    fn move_selected_in_playlist(&mut self, delta: isize) {
        let UserView::LocalPlaylist(id) = self.user_view else {
            return;
        };
        let Some(from) = self.selected else { return };
        let len = self.current_tracks().len();
        if len == 0 {
            return;
        }
        let to = from as isize + delta;
        if to < 0 || to as usize >= len {
            return;
        }
        let to = to as usize;
        if self.local_playlists.move_entry(id, from, to) {
            self.persist_user_data();
            self.refresh_user_views();
            self.selected = Some(to);
        }
    }

    /// 从自建歌单移除选中曲目。
    fn remove_selected_from_playlist(&mut self) {
        let UserView::LocalPlaylist(id) = self.user_view else {
            return;
        };
        let Some(index) = self.selected else { return };
        let Some(track) = self.current_tracks().get(index).cloned() else {
            return;
        };
        if self.local_playlists.remove_track(id, &track) {
            self.persist_user_data();
            self.refresh_user_views();
            let len = self.current_tracks().len();
            self.selected = if len == 0 {
                None
            } else {
                Some(index.min(len - 1))
            };
            self.push_toast(self.t("已从歌单移除", "REMOVED FROM PLAYLIST"));
        }
    }

    /// 切换「最近播放」时间窗。
    fn set_history_window(&mut self, hours: i64) {
        if self.history_hours == hours {
            return;
        }
        self.history_hours = hours;
        if self.user_view == UserView::History {
            self.refresh_user_views();
            let len = self.current_tracks().len();
            self.selected = if len == 0 { None } else { Some(0) };
        }
    }

    /// 统一处理行内动作（`res_row` / 结果列表共用）。
    ///
    /// 红心是**独立于选中态**的动作：点红心不该改变选中行、更不该开始播放，
    /// 所以先单独分流，剩下的才是「选中 / 播放」。
    fn apply_row_action(&mut self, action: RowAction, index: usize) {
        if action == RowAction::ContextMenu {
            self.open_track_menu(index);
            return;
        }
        if action == RowAction::ToggleFavorite {
            if let Some(track) = self.current_tracks().get(index).cloned() {
                self.toggle_like(&track);
            }
            return;
        }
        // 单击只选中；双击 / 行尾 ▶ 才播放（P0-1：误触不得打断正在播放的曲目）
        self.selected = Some(index);
        if action == RowAction::Play || action == RowAction::PlayNow {
            self.play_selected();
        }
    }

    // ------------------------------------------------------------------
    // 右键菜单
    // ------------------------------------------------------------------

    /// 打开某一行曲目的右键菜单。
    ///
    /// 条目按"能不能用"动态生成：没有专辑 ID 就不给「查看专辑」，没有自建歌单
    /// 就给一条灰显提示（见 [`super::context_menu::playlist_entries`]）。
    fn open_track_menu(&mut self, index: usize) {
        let Some(track) = self.current_tracks().get(index).cloned() else {
            return;
        };
        self.open_track_menu_for(track, index);
    }

    /// 同上，但曲目直接给定（离线每日推荐等不在 `current_tracks` 里的列表用）。
    fn open_track_menu_for(&mut self, track: Track, index: usize) {
        let Some(ctx) = self.ctx.clone() else { return };
        let Some(pos) = ctx.input(|i| i.pointer.interact_pos()) else {
            return;
        };
        self.open_track_menu_at(track, index, pos);
    }

    /// 在指定位置打开曲目菜单（探针与真实右键共用同一条路径）。
    fn open_track_menu_at(&mut self, track: Track, index: usize, pos: Pos2) {
        self.open_track_menu_common(track, Some(index), None, pos);
    }

    fn open_track_menu_common(
        &mut self,
        track: Track,
        index: Option<usize>,
        chart: Option<Vec<Track>>,
        pos: Pos2,
    ) {
        let Some(ctx) = self.ctx.clone() else { return };
        let entries = self.track_menu_entries(&track);
        self.menu_track = Some(track);
        self.menu_index = index;
        self.menu_chart = chart;
        self.menu =
            super::context_menu::ContextMenuState::open(pos, entries, ctx.cumulative_frame_nr());
    }

    /// 曲目菜单的条目（按"能不能用"动态生成）。
    ///
    /// 抽成独立方法是为了能单测：条目决定用户看到什么入口，漏一条就是功能缺失，
    /// 多一条（比如给本地曲目「查看专辑」）就是点开一个空页。
    fn track_menu_entries(&self, track: &Track) -> Vec<super::context_menu::MenuEntry> {
        use super::context_menu::{MenuAction, MenuEntry};
        let lang = self.lang;
        let mut entries = vec![
            MenuEntry::item(MenuAction::Play, self.t("播放", "PLAY")),
            MenuEntry::item(MenuAction::PlayNext, self.t("下一首播放", "PLAY NEXT")),
            MenuEntry::item(MenuAction::AddToQueue, self.t("加入队列", "ADD TO QUEUE")),
            MenuEntry::item(
                MenuAction::ToggleFavorite,
                if self.favorites.contains(track) {
                    self.t("取消收藏", "UNFAVORITE")
                } else {
                    self.t("收藏", "FAVORITE")
                },
            ),
        ];
        if track
            .album_id
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
        {
            entries.push(MenuEntry::item(
                MenuAction::GoToAlbum,
                self.t("查看专辑", "GO TO ALBUM"),
            ));
        }
        if primary_artist_id(track).is_some() {
            entries.push(MenuEntry::item(
                MenuAction::GoToArtist,
                self.t("查看歌手", "GO TO ARTIST"),
            ));
        }
        // 本地曲目没有云端身份，加进自建歌单仍然有意义（离线可播）
        let targets: Vec<(u64, String)> = self
            .local_playlists
            .lists
            .iter()
            .map(|p| (p.id, p.name.clone()))
            .collect();
        entries.extend(super::context_menu::playlist_entries(&targets, lang));
        // 只有"正在看某个自建歌单"时才有"从歌单移除"
        if let UserView::LocalPlaylist(_) = self.user_view {
            entries.push(MenuEntry::separator());
            entries.push(MenuEntry::danger(
                MenuAction::RemoveFromPlaylist,
                self.t("从歌单移除", "REMOVE FROM PLAYLIST"),
            ));
        }
        entries
    }

    /// 打开某个自建歌单卡的右键菜单（打开 / 删除，删除走模态确认）。
    fn open_playlist_menu(&mut self, id: u64) {
        let Some(ctx) = self.ctx.clone() else { return };
        let Some(pos) = ctx.input(|i| i.pointer.interact_pos()) else {
            return;
        };
        let entries = vec![
            super::context_menu::MenuEntry::item(
                super::context_menu::MenuAction::OpenLocalPlaylist(id),
                self.t("打开歌单", "OPEN PLAYLIST"),
            ),
            super::context_menu::MenuEntry::separator(),
            super::context_menu::MenuEntry::danger(
                super::context_menu::MenuAction::DeleteLocalPlaylist(id),
                self.t("删除歌单", "DELETE PLAYLIST"),
            ),
        ];
        self.menu_track = None;
        self.menu_index = None;
        self.menu_chart = None;
        self.menu =
            super::context_menu::ContextMenuState::open(pos, entries, ctx.cumulative_frame_nr());
    }

    /// 已载入歌单（历史）卡的右键菜单：打开 / 移出列表。
    ///
    /// 歌单页从阶段 17 起不再渲染「已载入」卡，这个入口暂时没有挂载点；
    /// 菜单与它背后的 `request_remove_loaded_playlist` / `remove_loaded_playlist`
    /// 一并保留（移出历史是数据操作，删掉会让「已载入」数据只能靠重启清理）。
    #[allow(dead_code)]
    fn open_loaded_playlist_menu(&mut self, id: String) {
        let Some(ctx) = self.ctx.clone() else { return };
        let Some(pos) = ctx.input(|i| i.pointer.interact_pos()) else {
            return;
        };
        let entries = vec![
            super::context_menu::MenuEntry::item(
                super::context_menu::MenuAction::OpenLoadedPlaylist(id.clone()),
                self.t("打开歌单", "OPEN PLAYLIST"),
            ),
            super::context_menu::MenuEntry::separator(),
            super::context_menu::MenuEntry::danger(
                super::context_menu::MenuAction::RemoveLoadedPlaylist(id),
                self.t("移出列表", "REMOVE FROM LIST"),
            ),
        ];
        self.menu_track = None;
        self.menu_index = None;
        self.menu_chart = None;
        self.menu =
            super::context_menu::ContextMenuState::open(pos, entries, ctx.cumulative_frame_nr());
    }

    /// 账号歌单卡的右键菜单：打开 / 从列表隐藏。
    fn open_account_playlist_menu(&mut self, id: String) {
        let Some(ctx) = self.ctx.clone() else { return };
        let Some(pos) = ctx.input(|i| i.pointer.interact_pos()) else {
            return;
        };
        let entries = vec![
            super::context_menu::MenuEntry::item(
                super::context_menu::MenuAction::OpenAccountPlaylist(id.clone()),
                self.t("载入歌单", "LOAD PLAYLIST"),
            ),
            super::context_menu::MenuEntry::separator(),
            super::context_menu::MenuEntry::danger(
                super::context_menu::MenuAction::HideAccountPlaylist(id),
                self.t("从列表隐藏", "HIDE FROM LIST"),
            ),
        ];
        self.menu_track = None;
        self.menu_index = None;
        self.menu_chart = None;
        self.menu =
            super::context_menu::ContextMenuState::open(pos, entries, ctx.cumulative_frame_nr());
    }

    /// 把曲目插到「下一首」或追加到队列末尾。
    fn enqueue_track(&mut self, track: Track, play_next: bool) {
        self.ensure_queue();
        if play_next {
            let at = self
                .queue_cursor()
                .map(|i| i + 1)
                .unwrap_or(0)
                .min(self.queue.len());
            self.queue.insert(at, track.clone());
        } else {
            self.queue.push(track.clone());
        }
        // 队列内容变了，旧的洗牌序列不再对应
        self.shuffle_order = None;
        let (zh, en) = if play_next {
            ("下一首播放", "PLAY NEXT")
        } else {
            ("已加入队列", "ADDED TO QUEUE")
        };
        self.push_toast(self.t(
            &format!("{zh}：{}", track.title),
            &format!("{en}: {}", track.title.to_uppercase()),
        ));
    }

    /// 执行菜单选中的动作。
    fn apply_menu_action(&mut self, action: super::context_menu::MenuAction) {
        use super::context_menu::MenuAction;
        let track = self.menu_track.take();
        let index = self.menu_index.take();
        let chart = self.menu_chart.take();
        match action {
            MenuAction::Play => {
                if let Some(queue) = chart {
                    // 队列 = 菜单自带快照（与双击/▶ 一致），
                    // 不能把搜索/歌单等当前浏览列表顶进队列。
                    if let Some(track) = &track {
                        self.selected = queue.iter().position(|t| t.id == track.id);
                    }
                    self.replace_queue(queue);
                } else if let Some(i) = index {
                    if i < self.current_tracks().len() {
                        self.selected = Some(i);
                        self.adopt_queue();
                    }
                }
                if let Some(track) = track {
                    self.play_track(track);
                }
            }
            MenuAction::PlayNext => {
                if let Some(track) = track {
                    self.enqueue_track(track, true);
                }
            }
            MenuAction::AddToQueue => {
                if let Some(track) = track {
                    self.enqueue_track(track, false);
                }
            }
            MenuAction::ToggleFavorite => {
                if let Some(track) = track {
                    self.toggle_like(&track);
                }
            }
            MenuAction::AddToPlaylist(id) => {
                if let Some(track) = track {
                    self.add_to_local_playlist(id, &track);
                }
            }
            MenuAction::RemoveFromPlaylist => {
                if let Some(i) = index {
                    self.selected = Some(i);
                    self.remove_selected_from_playlist();
                }
            }
            MenuAction::OpenLocalPlaylist(id) => {
                self.select_user_view(UserView::LocalPlaylist(id));
                self.set_page(Page::Playlist);
            }
            MenuAction::DeleteLocalPlaylist(id) => self.request_delete_local_playlist(id),
            MenuAction::OpenLoadedPlaylist(id) => {
                if let Some(idx) = self.playlists.iter().position(|p| p.id == id) {
                    self.clear_user_view();
                    self.active_local = false;
                    self.active_playlist = Some(idx);
                    self.selected = if self.playlists[idx].tracks.is_empty() {
                        None
                    } else {
                        Some(0)
                    };
                    self.set_page(Page::Playlist);
                }
            }
            MenuAction::RemoveLoadedPlaylist(id) => self.request_remove_loaded_playlist(id),
            MenuAction::OpenAccountPlaylist(id) => {
                self.playlist_input = id;
                self.submit_playlist();
            }
            MenuAction::HideAccountPlaylist(id) => self.request_hide_account_playlist(id),
            MenuAction::GoToAlbum => {
                if let Some(track) = track {
                    let id = track.album_id.clone().unwrap_or_default();
                    let name = track.album.clone().unwrap_or_default();
                    self.open_album_page(&id, &name);
                }
            }
            MenuAction::GoToArtist => {
                if let Some(track) = track {
                    // 名字要与 ID 同下标：`primary_artist_id` 找的是"第一个有 ID 的"，
                    // 这里按同一位置取名字，避免"点 A 显示 B"
                    let id = primary_artist_id(&track).map(str::to_string);
                    let name = track
                        .artist_ids
                        .iter()
                        .position(|s| !s.is_empty())
                        .and_then(|i| track.artists.get(i).cloned())
                        .unwrap_or_else(|| track.artists.join("/"));
                    if let Some(id) = id {
                        self.open_artist_page(&id, &name);
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // 收藏双向同步
    // ------------------------------------------------------------------

    /// 拉取账号歌单（幂等：已拉过 / 在途 / 未登录都不发请求）。
    fn ensure_user_playlists(&mut self) {
        if self.user_pl_loaded || self.user_pl_state.is_loading() {
            return;
        }
        if self.login.logged_in(self.lang).is_none() {
            return;
        }
        self.user_pl_seq = self.bridge.user_playlists(USER_PLAYLIST_LIMIT);
        self.user_pl_state.begin_load();
    }

    /// 启动一轮收藏同步（登录成功后自动跑；也可手动触发）。
    ///
    /// 幂等：已经在途时直接返回，避免反复点「同步」把请求叠起来。
    fn start_favorite_sync(&mut self) {
        if self.login.logged_in(self.lang).is_none() {
            self.push_toast(self.t("登录后才能同步收藏", "Sign in to sync your liked songs"));
            return;
        }
        if self.sync.active() {
            tracing::debug!("收藏同步已在途，忽略重复触发");
            return;
        }
        self.sync.reset();
        self.sync.ids_seq = self.bridge.liked_songs();
        self.push_toast(self.t("正在同步收藏…", "SYNCING LIKED SONGS…"));
    }

    /// 云端收藏 ID 列表回来了：算双向差集。
    fn on_sync_ids(&mut self, result: Result<Vec<String>, ProviderError>) {
        self.sync.ids_seq = 0;
        let cloud_ids = match result {
            Ok(ids) => ids,
            Err(e) => {
                self.sync.reset();
                self.push_toast(e.message(self.lang));
                return;
            }
        };
        let local: std::collections::HashSet<String> = self
            .favorites
            .tracks()
            .into_iter()
            .filter(|t| t.provider != ProviderId::Local)
            .map(|t| t.id)
            .collect();
        // 方向一：云端有、本地没有 → 待取详情补进本地
        // 方向二：本地在线收藏、云端没有 → 待推送
        let (missing, to_push) = favorite_sync_plan(&cloud_ids, &local);

        self.sync.details_pending = missing;
        self.sync.push_queue = to_push;
        self.sync.pulled = 0;
        self.sync.pushed = 0;
        self.sync.failed = 0;
        self.sync_fetch_details();
    }

    /// 取下一片详情（`details_pending` 为空时直接进入推送阶段）。
    fn sync_fetch_details(&mut self) {
        if self.sync.details_pending.is_empty() {
            self.push_next_favorite();
            return;
        }
        let take = SYNC_DETAILS_CHUNK.min(self.sync.details_pending.len());
        let chunk: Vec<String> = self.sync.details_pending.drain(..take).collect();
        self.sync.details_seq = self.bridge.song_details(chunk);
    }

    /// 详情回来了：补进本地收藏，然后继续取下一片 / 开始推送。
    fn on_sync_details(&mut self, result: Result<Vec<Track>, ProviderError>) {
        self.sync.details_seq = 0;
        match result {
            Ok(tracks) => {
                let now = super::history::now_secs();
                for track in tracks {
                    if self.favorites.insert_if_absent(&track, now) {
                        self.sync.pulled += 1;
                    }
                }
            }
            Err(e) => {
                // 详情拿不到不影响反方向推送：记一次失败继续走
                tracing::debug!("收藏同步：详情获取失败 {e}");
                self.sync.failed += 1;
            }
        }
        self.sync_fetch_details();
    }

    /// 推送下一首（串行：并发打 `song/like` 容易被限流）。
    fn push_next_favorite(&mut self) {
        while let Some(id) = self.sync.push_queue.pop() {
            if id.trim().is_empty() {
                continue;
            }
            self.sync.push_seq = self.bridge.like_song(id, true);
            return;
        }
        self.finish_favorite_sync();
    }

    /// 一轮同步收尾：落盘 + 刷新视图 + 汇报结果。
    fn finish_favorite_sync(&mut self) {
        let (pulled, pushed, failed) = (self.sync.pulled, self.sync.pushed, self.sync.failed);
        self.sync.reset();
        if pulled > 0 {
            self.persist_user_data();
            if self.user_view == UserView::Favorites {
                self.refresh_user_views();
                let len = self.current_tracks().len();
                self.selected = if len == 0 { None } else { Some(0) };
            }
        }
        if pulled + pushed + failed == 0 {
            tracing::info!("收藏同步：两端已一致，无需变更");
            return;
        }
        let msg = format!(
            "{} · {} {} / {} {}",
            self.t("收藏已同步", "LIKED SONGS SYNCED"),
            self.t("云端补入", "pulled"),
            pulled,
            self.t("本地推送", "pushed"),
            pushed
        );
        let msg = if failed > 0 {
            format!("{msg} / {} {}", self.t("失败", "failed"), failed)
        } else {
            msg
        };
        self.push_toast(msg);
    }

    /// 启动全局媒体键监听（幂等）。
    ///
    /// 只在用户没关掉开关时注册：`RegisterHotKey` 是进程级独占，被别的播放器
    /// 占走时注册会失败——那种情况只提示一次，绝不每帧重试。
    fn start_media_keys(&mut self) {
        if self.media_keys.is_some() || !self.media_keys_enabled {
            return;
        }
        self.media_keys = Some(super::media_keys::MediaKeys::start());
    }

    /// 停止媒体键监听（关闭开关时调用，释放全局热键）。
    fn stop_media_keys(&mut self) {
        // `MediaKeys` 的 Drop 会 `PostThreadMessageW(WM_QUIT)` 收掉消息泵线程
        self.media_keys = None;
        self.media_keys_warned = false;
    }

    /// 每帧轮询媒体键事件并映射到播放命令。
    fn poll_media_keys(&mut self) {
        let Some(keys) = self.media_keys.as_ref() else {
            return;
        };
        let commands = keys.poll();
        // 注册失败只提示一次（每帧都弹会把界面淹掉）
        if !self.media_keys_warned {
            match keys.status() {
                super::media_keys::MediaKeyStatus::Unavailable => {
                    self.media_keys_warned = true;
                    self.push_toast(self.t(
                        "媒体键被其它程序占用，快捷键不可用",
                        "MEDIA KEYS TAKEN BY ANOTHER APP",
                    ));
                }
                super::media_keys::MediaKeyStatus::Unsupported => {
                    self.media_keys_warned = true;
                    tracing::info!("当前平台不支持全局媒体键");
                }
                _ => {}
            }
        }
        for command in commands {
            match command {
                super::media_keys::MediaCommand::Toggle => {
                    if self.core.now_playing().is_none() {
                        self.play_selected();
                    } else if let Err(reason) = self.core.toggle() {
                        self.push_toast(reason.message(self.lang));
                    }
                }
                super::media_keys::MediaCommand::Next => self.advance_next(),
                super::media_keys::MediaCommand::Prev => self.advance_prev(),
                super::media_keys::MediaCommand::Stop => self.core.stop(),
            }
        }
    }

    // ------------------------------------------------------------------
    // SMTC（系统媒体传输控件）
    // ------------------------------------------------------------------

    /// 建立 SMTC 媒体会话（幂等；失败只提示一次）。
    ///
    /// 与媒体键共用 `media_keys_enabled` 开关：对用户来说"按播放键要能动"
    /// 是一件事，系统浮层与全局热键只是两条不同的来路。
    fn start_smtc(&mut self) {
        // `smtc_warned` 兼作"别再试了"的闸门：懒建路径每帧都会调到这里，
        // 失败后不拦住就会每帧重打一次 WinRT 激活。
        if self.smtc.is_some() || !self.media_keys_enabled || self.smtc_warned {
            return;
        }
        match super::smtc::Smtc::start() {
            Ok(smtc) => {
                tracing::info!("SMTC 媒体会话已建立");
                self.smtc = Some(smtc);
            }
            Err(e) => {
                // 只在首次失败时打扰用户：WinRT 激活失败基本不可恢复
                if !self.smtc_warned {
                    self.smtc_warned = true;
                    tracing::warn!("SMTC 建立失败: {e}");
                }
            }
        }
    }

    /// 注销 SMTC 会话（关闭开关时调用）。
    fn stop_smtc(&mut self) {
        self.smtc = None;
        self.smtc_warned = false;
    }

    /// 每帧：把当前曲目/播放态同步给系统，并取回系统发来的按钮事件。
    ///
    /// 同步走的是"只比标题"的短路（见 [`super::smtc::Smtc::set_now_playing`]），
    /// 所以每帧调用不会反复打 WinRT。
    fn poll_smtc(&mut self) {
        // 懒建：没有曲目时不在系统音量浮层里挂一张空媒体卡
        if self.smtc.is_none() && self.core.now_playing().is_some() {
            self.start_smtc();
        }
        let Some(smtc) = self.smtc.as_mut() else {
            return;
        };
        if let Some(now) = self.core.now_playing() {
            smtc.set_now_playing(
                &now.title,
                &now.artist,
                now.album.as_deref().unwrap_or_default(),
            );
        }
        smtc.set_playing(self.core.is_playing());
        let events = smtc.poll();
        if events.is_empty() {
            return;
        }
        for event in events {
            match event {
                super::smtc::SmtcButton::Play => {
                    if !self.core.is_playing() {
                        if let Err(reason) = self.core.toggle() {
                            self.push_toast(reason.message(self.lang));
                        }
                    }
                }
                super::smtc::SmtcButton::Pause => {
                    if self.core.is_playing() {
                        if let Err(reason) = self.core.toggle() {
                            self.push_toast(reason.message(self.lang));
                        }
                    }
                }
                super::smtc::SmtcButton::Next => self.advance_next(),
                super::smtc::SmtcButton::Previous => self.advance_prev(),
                super::smtc::SmtcButton::Stop => self.core.stop(),
            }
        }
    }

    // ------------------------------------------------------------------
    // 托盘
    // ------------------------------------------------------------------

    /// 开关托盘（同时落盘偏好）。
    ///
    /// 关掉托盘时若窗口已经被"收进托盘"，必须顺手把窗口放回来——否则用户会
    /// 得到一个不可见但还在跑的进程，只能去任务管理器杀。
    fn set_tray_enabled(&mut self, on: bool) {
        self.tray_enabled = on;
        if on {
            self.start_tray();
        } else {
            self.tray = None;
            self.tray_warned = false;
            if self.hidden_to_tray {
                self.hidden_to_tray = false;
                self.show_main_window();
            }
        }
        self.persist_prefs();
    }

    /// 创建托盘图标（幂等；失败只提示一次）。
    fn start_tray(&mut self) {
        if self.tray.is_some() || !self.tray_enabled {
            return;
        }
        let favorited = self
            .current_track
            .as_ref()
            .map(|t| self.favorites.contains(t))
            .unwrap_or(false);
        match super::tray::Tray::new(self.lang, self.core.is_playing(), favorited) {
            Ok(tray) => {
                tracing::info!("托盘图标已创建");
                self.tray = Some(tray);
            }
            Err(e) => {
                if !self.tray_warned {
                    self.tray_warned = true;
                    tracing::warn!("托盘创建失败: {e}");
                    self.push_toast(self.t(
                        "托盘图标创建失败，已保持关闭",
                        "Tray icon failed to start; kept off",
                    ));
                }
                self.tray_enabled = false;
            }
        }
    }

    /// 每帧：同步托盘菜单文案 + 取回菜单命令。
    fn poll_tray(&mut self) {
        let favorited = self
            .current_track
            .as_ref()
            .map(|t| self.favorites.contains(t))
            .unwrap_or(false);
        if let Some(tray) = self.tray.as_mut() {
            tray.sync(self.lang, self.core.is_playing(), favorited);
        }
        let Some(tray) = self.tray.as_ref() else {
            return;
        };
        let commands = tray.poll();
        for command in commands {
            match command {
                super::tray::TrayCommand::PlayPause => {
                    if self.core.now_playing().is_none() {
                        self.play_selected();
                    } else if let Err(reason) = self.core.toggle() {
                        self.push_toast(reason.message(self.lang));
                    }
                }
                super::tray::TrayCommand::Prev => self.advance_prev(),
                super::tray::TrayCommand::Next => self.advance_next(),
                super::tray::TrayCommand::ToggleFavorite => {
                    if let Some(track) = self.current_track.clone() {
                        self.toggle_like(&track);
                    } else {
                        self.push_toast(
                            self.t("当前没有播放中的曲目", "Nothing is playing right now"),
                        );
                    }
                }
                super::tray::TrayCommand::Show => {
                    self.hidden_to_tray = false;
                    self.show_main_window();
                }
                super::tray::TrayCommand::Quit => {
                    // 退出是真退出：先停掉媒体会话与托盘，避免留下孤儿图标
                    self.stop_smtc();
                    self.stop_media_keys();
                    self.tray = None;
                    self.quit_requested = true;
                }
            }
        }
    }

    /// 把主窗口叫回前台（托盘「显示主窗口」与关托盘时共用）。
    fn show_main_window(&mut self) {
        let ctx = self.ctx.clone();
        if let Some(ctx) = ctx {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }

    /// 离线每日推荐列表（未登录/拉取失败时的降级内容）。
    ///
    /// 数据来自本地曲库 + 播放历史，**不触网**：这里不做任何 Provider 请求，
    /// 所以断网、未登录都能用。
    fn ui_offline_daily(&mut self, ui: &mut Ui, height: f32) {
        let mut action: Option<(RowAction, usize)> = None;
        egui::ScrollArea::vertical()
            .id_salt("daily-offline")
            .max_height(height)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (index, track) in self.daily_local.iter().enumerate() {
                    let view = ResRow {
                        index,
                        title: &track.title,
                        artist: &track.artists.join(" / "),
                        time: &track.duration_text(),
                        current: false,
                        selected: false,
                        favorite: self.favorites.contains(track),
                        highlight: "",
                    };
                    let act = widgets::res_row(ui, ui.scope_id().with(("daily-off", index)), &view);
                    if act != RowAction::None {
                        action = Some((act, index));
                    }
                }
            });
        let Some((act, index)) = action else { return };
        let Some(track) = self.daily_local.get(index).cloned() else {
            return;
        };
        match act {
            RowAction::ContextMenu => {
                // 离线列表不在 `current_tracks` 里，直接按 Track 开菜单
                self.open_track_menu_for(track, index);
            }
            RowAction::ToggleFavorite => {
                self.toggle_like(&track);
            }
            RowAction::Select => {
                // 离线列表没有独立的选中态入口，单击只做无害确认
            }
            _ => {
                // 以离线推荐列表建立播放队列，下一首留在同一批里
                self.replace_queue(self.daily_local.clone());
                self.play_track(track);
            }
        }
    }

    /// 每帧检查：这首是否已经"听够"，够格就记一次历史。
    ///
    /// 用核心的真实播放时钟（`core.progress()`）而不是 UI 计时器：暂停时
    /// 时钟不走，所以"挂着不听"不会刷出历史；拖到 90% 再听 3 秒也会因为
    /// 位置过半而被判为听过（与教程 ch33 的语义一致）。
    fn tick_history(&mut self) {
        if self.history_marked {
            return;
        }
        let Some(track) = self.current_track.clone() else {
            return;
        };
        let p = self.core.progress();
        let played_ms = p.position.as_millis() as u64;
        let total_ms = p.total.map(|d| d.as_millis() as u64);
        if !super::history::qualifies(played_ms, total_ms) {
            return;
        }
        self.history
            .record(&track, played_ms, super::history::now_secs());
        self.history_marked = true;
        self.persist_user_data();
        // 正在看「最近播放」时把新条目顶上去
        if self.user_view == UserView::History {
            self.refresh_user_views();
        }
    }

    /// 按当前界面语言二选一（中文 / English）。
    ///
    /// 见 [`super::i18n`] 的取舍说明：不做 key 表，就地二选一。
    fn t<'a>(&self, zh: &'a str, en: &'a str) -> &'a str {
        pick(self.lang, zh, en)
    }

    /// 待落盘的偏好快照。
    ///
    /// 语言取 [`Self::pref_lang`] 而不是 [`Self::lang`]：后者可能被探针
    /// `REIMU_PROBE_LANG` 覆盖，探针值绝不落盘。同理透明度取
    /// [`Self::lyric_opacity_frozen`] 的原值（有探针时）。
    fn prefs_snapshot(&self) -> super::prefs::Prefs {
        super::prefs::Prefs {
            quality: self.bridge.quality(),
            lang: self.pref_lang,
            tray: self.tray_enabled,
            lyric_opacity: self
                .lyric_opacity_frozen
                .unwrap_or_else(|| self.lyric_control.opacity()),
            lyric_progress: self.lyric_control.progress(),
            // 静音时落盘"静音前音量"，下次取消静音仍是用户熟悉的响度
            volume: if self.muted {
                (self.pre_mute_volume.clamp(0.0, 1.0) * 100.0).round() as u8
            } else {
                (self.core.volume().clamp(0.0, 1.0) * 100.0).round() as u8
            },
            muted: self.muted,
            shuffle: self.core.shuffle,
            repeat: self.core.repeat_mode.code(),
            theme: theme::theme_index() as u8,
        }
    }

    /// 保存偏好（音质 + 语言 + 托盘）。
    fn persist_prefs(&self) {
        self.persist_prefs_impl(super::probe::forced_lang().is_none());
    }

    /// [`Self::persist_prefs`] 的可测内核。
    ///
    /// 测试构建不落盘：单元测试绝不能写用户真实的 `%APPDATA%\ReimuPlayer`
    /// （与本地曲库 `apply_library_dir(dir, false)` 同一条纪律）。
    /// `allow = false`（探针强制语言的自动化运行）同样不落盘：此时的设置改动
    /// 多为置顶窗口截获的误触，不得改写用户偏好（阶段 11 实测被改写过两次）。
    fn persist_prefs_impl(&self, allow: bool) {
        if allow {
            #[cfg(not(test))]
            super::prefs::save(&self.prefs_snapshot());
        } else {
            tracing::debug!("探针运行中：设置改动不落盘");
        }
    }

    fn push_toast(&mut self, message: impl Into<String>) {
        self.toast = Some((message.into(), self.t + TOAST_SECS));
    }

    /// 程序化封面（无真实 `cover_url` 时的兜底，也用作真实封面到达前的占位）。
    fn cover_texture(&mut self, ctx: &egui::Context, seed_src: &str) -> Option<TextureId> {
        let seed = widgets::hash_id(seed_src);
        let handle = self.covers.entry(seed).or_insert_with(|| {
            let rgba = halftone::procedural_cover(seed, COVER_PX, COVER_PX);
            let image = egui::ColorImage::from_rgba_unmultiplied([COVER_PX, COVER_PX], &rgba);
            ctx.load_texture(
                format!("reimu-player-cover-{seed}"),
                image,
                TextureOptions::NEAREST,
            )
        });
        Some(handle.id())
    }

    /// 真实封面 → 点阵 halftone 纹理（异步拉取，未就绪时回退程序化封面）。
    ///
    /// 三条状态线：
    /// - `cover_tex` 命中 → 直接返回（每帧零网络、零解码）；
    /// - 未拉过 → 发一次 `CoverImage` 并登记在途，**不阻塞本帧**；
    /// - 拉取/解码失败 → 记入 `cover_failed`，之后不再重试，永久回退程序化封面
    ///   （避免坏 URL 每帧重试打爆网络）。
    fn cover_texture_url(
        &mut self,
        ctx: &egui::Context,
        fallback_key: &str,
        url: Option<&str>,
    ) -> Option<TextureId> {
        let Some(url) = url.filter(|u| !u.is_empty()) else {
            return self.cover_texture(ctx, fallback_key);
        };
        if let Some(entry) = self.cover_tex.get_mut(url) {
            entry.used = self.cover_clock;
            return Some(entry.tex.id());
        }
        // 在途已经排满：这一帧先用占位图，下一帧再补。
        // 硬塞进封面池会被立即回执取消，销账后下一帧又重发、又被取消——
        // 同一行的真封面和占位图就这样逐帧来回闪。
        if self.cover_inflight.len() >= COVER_INFLIGHT_LIMIT {
            return self.cover_texture(ctx, fallback_key);
        }
        if !self.cover_failed.contains(url) && !self.cover_inflight.contains_key(url) {
            let seq = self.bridge.cover_image(url);
            self.cover_inflight.insert(url.to_string(), seq);
        }
        self.cover_texture(ctx, fallback_key)
    }

    /// 把下载到的封面字节合成点阵纹理并入库（失败则标记不再重试）。
    /// 上传已烘好的封面纹理（解码 + 网纹已在后台线程完成）。
    ///
    /// UI 线程这里只做 `load_texture`，成本与图片大小无关——这是把
    /// 「换曲瞬间卡一帧」移出 UI 线程的关键。
    fn install_cover(&mut self, ctx: &egui::Context, url: String, rgba: &[u8]) {
        if rgba.len() != COVER_PX * COVER_PX * 4 {
            tracing::debug!("封面像素长度不符（{}），回退程序化封面: {url}", rgba.len());
            self.cover_failed.insert(url);
            return;
        }
        let image = egui::ColorImage::from_rgba_unmultiplied([COVER_PX, COVER_PX], rgba);
        let tex = ctx.load_texture(
            format!("reimu-player-cover-{}", widgets::hash_id(&url)),
            image,
            TextureOptions::NEAREST,
        );
        // 上限保护：只淘汰「本帧没被画过」的那一条。
        // 整体清空会把满屏封面同时打回占位图，这些占位又在同一帧齐声重发请求、
        // 把缓存重新填回上限——于是闪烁自己养自己，永远停不下来。
        // 同理，**本帧画过的条目一律不许淘汰**：非虚拟化列表里每一张封面每帧
        // 都会刷新 `used`，淘汰其中任何一条，下一帧它就以占位图重来，形成
        // 「淘汰 → 重拉 → 淘汰」的自激循环。找不到可淘汰的旧条目时宁可不淘汰
        // （缓存暂时超出上限），也不要动正在显示的封面。
        if !self.cover_tex.contains_key(&url) && self.cover_tex.len() >= COVER_CACHE_LIMIT {
            let victim = self
                .cover_tex
                .iter()
                .filter(|(_, cached)| cached.used != self.cover_clock)
                .min_by_key(|(_, cached)| cached.used)
                .map(|(victim, _)| victim.clone());
            if let Some(victim) = victim {
                self.cover_tex.remove(&victim);
            }
        }
        self.cover_tex.insert(
            url,
            CachedCover {
                tex,
                used: self.cover_clock,
            },
        );
    }

    /// 当前曲目在播放队列中的位置（优先取正在播的，其次取选中项）。
    fn current_index(&self) -> Option<usize> {
        let tracks = self.playback_tracks();
        let now_id = self.core.now_playing().map(|n| n.id.clone());
        if let Some(id) = &now_id {
            if let Some(pos) = tracks.iter().position(|t| &t.id == id) {
                return Some(pos);
            }
        }
        self.selected.filter(|i| *i < tracks.len())
    }

    /// 把正在播放的曲目映射回浏览列表的选中行（切歌后高亮跟随）。
    fn reveal_in_browse(&mut self, track: &Track) {
        self.selected = self
            .current_tracks()
            .iter()
            .position(|t| t.provider == track.provider && t.id == track.id);
    }

    /// 当前列表指纹（内容变化 → 洗牌序列作废重洗）。
    fn list_fingerprint(&self) -> u64 {
        let mut h: u64 = 0x9E37_79B9_7F4A_7C15;
        for t in self.playback_tracks() {
            h ^= widgets::hash_id(&t.id);
            h = h.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
        h ^ (self.playback_tracks().len() as u64)
    }

    /// 确保随机洗牌序列有效（列表变了就重洗）。
    ///
    /// `shuffle_pos` 的语义统一为「当前曲在 `order` 中的位置」：
    /// next = pos+1、prev = pos-1、peek = pos+1，三者互为逆运算。
    fn ensure_shuffle_order(&mut self) {
        let fingerprint = self.list_fingerprint();
        let len = self.playback_tracks().len();
        let stale = self.shuffle_order.as_ref().map(|o| o.len()) != Some(len)
            || self.list_fingerprint != fingerprint;
        if !stale && self.shuffle_order.is_some() {
            return;
        }
        self.list_fingerprint = fingerprint;
        // 确定性洗牌：xorshift 种子单调递增，排完即按序列消费
        let seed = self.shuffle_seed.max(1);
        self.shuffle_seed = self.shuffle_seed.wrapping_add(1);
        let mut order: Vec<usize> = (0..len).collect();
        let mut rng: u64 = seed ^ 0x9E37_79B9_7F4A_7C15;
        for i in (1..len).rev() {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let j = (rng as usize) % (i + 1);
            order.swap(i, j);
        }
        // 游标落在当前曲的位置（没有当前曲 → 指向末尾，使下一首从 0 开始）
        let pos = self
            .current_index()
            .and_then(|cur| order.iter().position(|i| *i == cur))
            .unwrap_or_else(|| len.saturating_sub(1));
        self.shuffle_order = Some(order);
        self.shuffle_pos = pos;
    }

    /// 当前是否走洗牌序列（等价于用户的随机偏好）。
    fn shuffle_effective(&self) -> bool {
        self.core.shuffle
    }

    /// 播放模式键：四档循环（顺序 → 随机 → 单曲循环 → 列表循环 → 顺序）。
    ///
    /// 交互照播放键——点一下走一档，档位只由 [`PlayMode`] 决定，键面不另存状态。
    /// 四档合成一个键是因为「随机 + 单曲循环」本就互斥（单曲循环下自动续播永不换曲），
    /// 拆成两个键反而允许用户摆出这个走不通的组合。
    fn cycle_play_mode(&mut self) {
        let next = self.core.play_mode().next();
        self.core.set_play_mode(next);
        // 洗牌序列作废：离开随机档不需要它，回到随机档按当前列表重洗
        self.shuffle_order = None;
        // **随机与心动互斥**：随机在封闭列表里打乱，心动向开放流续曲，
        // 两者对「下一首从哪来」的答案互相矛盾。用户切到随机档，心动键必须
        // 当场灭掉并停止续批，而不是继续躲在后台改队列。
        if next == PlayMode::Shuffle && (self.heartbeat_on || self.heartbeat_pending) {
            // 别把心动进场前那个档位还原回来：用户这一刀切的是随机档，
            // 还原会把刚设好的 Shuffle 又盖掉。
            self.heartbeat_prev_mode = None;
            self.stop_heartbeat();
        }
        self.push_toast(self.play_mode_label(next));
        self.persist_prefs();
    }

    /// 某一档的文案（toast 与悬浮提示共用，避免两处各写一份漂移）。
    fn play_mode_label(&self, mode: PlayMode) -> String {
        match mode {
            PlayMode::Sequential => self.t("顺序播放", "SEQUENTIAL PLAY"),
            PlayMode::Shuffle => self.t("随机播放", "SHUFFLE"),
            PlayMode::RepeatOne => self.t("单曲循环", "REPEAT ONE"),
            PlayMode::RepeatAll => self.t("列表循环", "REPEAT ALL"),
        }
        .to_string()
    }

    // ------------------------------------------------------------------
    // 心动模式（智能播放）
    // ------------------------------------------------------------------

    /// 控制行「心动模式」键：开 → 关 / 关 → 开。
    fn toggle_heartbeat(&mut self) {
        if self.heartbeat_on || self.heartbeat_pending {
            self.stop_heartbeat();
        } else {
            self.start_heartbeat();
        }
    }

    /// 开心动模式：以「正在播的这首歌 + 它所属的歌单」为种子，向服务端要一批续曲。
    ///
    /// 与随机**互斥**（同一个"下一首从哪来"的问题只能有一个答案）：进场先把
    /// 播放模式压到顺序档并作废洗牌序列，原档位记在 [`Self::heartbeat_prev_mode`]
    /// 里，退出时还原。
    fn start_heartbeat(&mut self) {
        let Some(track) = self.now_playing_track() else {
            self.push_toast(self.t("先放一首歌再开心动模式", "PLAY A TRACK FIRST"));
            return;
        };
        if self.core.play_mode() != PlayMode::Sequential {
            self.heartbeat_prev_mode = Some(self.core.play_mode());
            self.core.set_play_mode(PlayMode::Sequential);
            self.shuffle_order = None;
            self.persist_prefs();
        }
        self.heartbeat_seen.clear();
        self.heartbeat_stall = 0;
        self.heartbeat_tail = None;
        self.heartbeat_start = None;

        match self.netease_song_id(&track) {
            Some(song_id) => {
                let pid = self.heartbeat_pid();
                self.request_heartbeat(&song_id, &pid, "");
            }
            // 本地曲目：先经 `search_match` 换成网易云歌曲 ID，再回来启动。
            // 期间键**不亮**——高亮只表示"数据源真的切到心动了"。
            None if track.provider == ProviderId::Local => {
                self.heartbeat_pending = true;
                self.push_toast(self.t("正在识别本地曲目…", "IDENTIFYING LOCAL TRACK…"));
                self.request_local_match(&track.id);
            }
            None => {
                self.push_toast(self.t(
                    "这首歌没有网易云对应曲目，开不了心动模式",
                    "NO NETEASE MATCH FOR THIS TRACK",
                ));
            }
        }
    }

    /// 关心动模式：停止续批，并把**还没播到**的心动续曲从队列里收回。
    ///
    /// 正在播的那一首不动——用户按的是"别再推荐了"，不是"把这首歌掐了"。
    /// 收回之后磁带架与队列重新等于"用户自己的列表"，不会留下一条看不懂的尾巴。
    fn stop_heartbeat(&mut self) {
        let was_on = self.heartbeat_on;
        self.heartbeat_on = false;
        self.heartbeat_pending = false;
        self.heartbeat_seed = None;
        self.heartbeat_seq = None;
        if let Some(start) = self.heartbeat_start.take() {
            let keep = self
                .queue_cursor()
                .map(|c| c + 1)
                .unwrap_or(start)
                .max(start)
                .min(self.queue.len());
            self.queue.truncate(keep);
            self.heartbeat_tail = None;
        }
        self.heartbeat_seen.clear();
        self.heartbeat_stall = 0;
        if let Some(prev) = self.heartbeat_prev_mode.take() {
            self.core.set_play_mode(prev);
            self.shuffle_order = None;
            self.persist_prefs();
        }
        if was_on {
            self.push_toast(self.t("心动模式已关闭", "HEARTBEAT OFF"));
        }
    }

    /// 发一批心动请求（单飞：已有在途就静默丢弃，不叠请求）。
    fn request_heartbeat(&mut self, song_id: &str, playlist_id: &str, start_id: &str) {
        if self.heartbeat_seq.is_some() {
            return;
        }
        self.heartbeat_seed = Some((song_id.to_string(), playlist_id.to_string()));
        let seq = self
            .bridge
            .intelligence_list(song_id, playlist_id, start_id, HEARTBEAT_BATCH);
        self.heartbeat_seq = Some(seq);
        tracing::debug!(target: "heartbeat", "请求智能播放 id={song_id} pid={playlist_id} sid={start_id}");
    }

    /// 收到一批智能播放曲目：去重后追加到播放队列末尾。
    fn apply_heartbeat_batch(&mut self, tracks: Vec<Track>) {
        // 心动续曲是**接在当前播放列表后面**的，不是另起一条队列。
        // 若还没有队列（直接从浏览列表播放、或点了单曲），先按浏览列表建一份，
        // 否则追加完 `queue_active = true` 会把用户原来那份列表整个换掉。
        self.ensure_queue();
        let mut added = 0usize;
        for track in tracks {
            if !self.heartbeat_seen.insert(track.id.clone()) {
                continue;
            }
            self.known.insert(track.id.clone(), track.clone());
            if self.heartbeat_start.is_none() {
                self.heartbeat_start = Some(self.queue.len());
            }
            self.queue.push(track);
            added += 1;
        }
        if added > 0 {
            self.heartbeat_tail = Some(self.queue.len() - 1);
            self.heartbeat_stall = 0;
            // 队列已经被心动续曲接管：磁带架必须显示同一份，否则"界面一套、播放一套"。
            self.queue_active = true;
            if !self.heartbeat_on {
                self.heartbeat_on = true;
                self.heartbeat_pending = false;
                self.push_toast(self.t("心动模式已开启", "HEARTBEAT ON"));
            }
            return;
        }
        // 这批一首新的都没有（全重复 / 服务端给空）。此时**不亮键**——
        // 亮着一盏背后没有内容的键，用户看到的是"开了但没反应"。
        if self.heartbeat_on {
            // 已在心动流里：当场再要一批，否则会在原地断流。连续要不到就收手，
            // 别对着限流的接口打转。
            self.heartbeat_stall += 1;
            if self.heartbeat_stall >= HEARTBEAT_MAX_STALL {
                self.stop_heartbeat();
                self.push_toast(self.t(
                    "暂时没有可推荐的续曲，心动模式已关闭",
                    "NO MORE RECOMMENDATIONS — HEARTBEAT OFF",
                ));
            } else {
                self.extend_heartbeat();
            }
        } else {
            self.heartbeat_pending = false;
            self.push_toast(self.t(
                "这首歌暂时没有可推荐的续曲",
                "NO RECOMMENDATIONS FOR THIS TRACK YET",
            ));
        }
    }

    /// 队列快见底时续批。每帧调用一次，代价是几次整数比较。
    fn extend_heartbeat(&mut self) {
        if !self.heartbeat_on || self.heartbeat_seq.is_some() {
            return;
        }
        let Some((seed, pid)) = self.heartbeat_seed.clone() else {
            return;
        };
        let tail = match self.heartbeat_tail {
            Some(t) => t,
            None => return,
        };
        let cursor = self.queue_cursor().unwrap_or(0);
        if tail.saturating_sub(cursor) > HEARTBEAT_LOOKAHEAD {
            return;
        }
        // 出发点换成"正在播的这首"，服务端据此继续往后推；歌单上下文不变。
        let start_id = self
            .now_playing_track()
            .and_then(|t| self.netease_song_id(&t))
            .unwrap_or_default();
        self.request_heartbeat(&seed, &pid, &start_id);
    }

    /// 正在播放曲目的 [`Track`] 视图。
    ///
    /// 为什么不直接用 [`crate::gui::player_core::PlayerCore::now_playing`]：
    /// 那份 `NowPlaying` 是**磁带面板视角**，本地曲目的 `id` 只剩文件名（注释里
    /// 自己写了"仅展示"）。心动模式要的是数字歌曲 ID、`search_match` 要的是完整
    /// 路径——拿文件名去发请求，只会查无此曲。
    ///
    /// 取值优先级：① 应用自己的曲目快照（`play_track` / `play_local_path` 每次
    /// 换曲都会写）；② `known` 表按 ID 回查；③ 都拿不到才从磁带面板拼一份
    /// （此时本地曲目只有文件名，匹配会失败，但至少不会凭空打不开）。
    fn now_playing_track(&self) -> Option<Track> {
        let np = self.core.now_playing()?;
        if let Some(track) = self.current_track.as_ref() {
            // 本地快照的 id 是完整路径，磁带面板的 id 是文件名，两者比不了；
            // 因此本地只用"来源一致"判定，在线才比 id。
            let same = if np.is_local {
                track.provider == ProviderId::Local
            } else {
                track.provider == ProviderId::Netease && track.id == np.id
            };
            if same {
                return Some(track.clone());
            }
        }
        if let Some(track) = self.known.get(&np.id) {
            return Some(track.clone());
        }
        Some(Track {
            provider: if np.is_local {
                ProviderId::Local
            } else {
                ProviderId::Netease
            },
            id: np.id.clone(),
            title: np.title.clone(),
            artists: vec![np.artist.clone()],
            album: np.album.clone(),
            album_id: None,
            artist_ids: Vec::new(),
            duration_ms: None,
            cover_url: np.cover_url.clone(),
        })
    }

    /// 本曲目对应的网易云数字歌曲 ID；本地曲目查匹配表。
    ///
    /// 返回 `None` 表示"没有在线身份"——心动模式接口要的是数字歌曲 ID，
    /// 拿文件名当 ID 只会得到一个查无此曲。
    fn netease_song_id(&self, track: &Track) -> Option<String> {
        let numeric = |id: &str| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit());
        match track.provider {
            ProviderId::Netease if numeric(&track.id) => Some(track.id.clone()),
            ProviderId::Local => self
                .local_match
                .get(&track.id)
                .map(|m| m.id.clone())
                .filter(|id| numeric(id)),
            _ => None,
        }
    }

    /// 心动的歌单上下文（接口 `pid`）。取当前载入的在线歌单 ID。
    ///
    /// 拿不到时传 `"0"`——它只是"我没有上下文"的占位：网易云适配层会把它换成
    /// 账号的「我喜欢的音乐」歌单（实测该接口只认这一张，`0` 会当场 `400
    /// 歌单不存在`），UI 这里不必、也不该去猜接口认哪张。
    fn heartbeat_pid(&self) -> String {
        let numeric = |id: &str| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit());
        if let Some(id) = self
            .active_playlist
            .and_then(|i| self.playlists.get(i))
            .map(|p| p.id.clone())
            .filter(|id| numeric(id))
        {
            return id;
        }
        let input = self.playlist_input.trim().to_string();
        if numeric(&input) {
            input
        } else {
            "0".to_string()
        }
    }

    // ------------------------------------------------------------------
    // 本地曲目 → 网易云信息（search_match）
    // ------------------------------------------------------------------

    /// 为一个本地文件请求网易云曲目信息（幂等：在途 / 已匹配都直接跳过）。
    ///
    /// 为什么要匹配：本地文件只有文件名，**没有封面、没有数字歌曲 ID**——
    /// 于是磁带架画的是程序化占位图，心动模式也拿不到种子。匹配上之后
    /// 两样都补齐了。
    fn request_local_match(&mut self, path: &str) {
        if self.local_match.contains_key(path) || self.match_inflight.contains_key(path) {
            return;
        }
        // 没标签又没标题可言（空文件名）就别浪费一次往返
        if path.trim().is_empty() {
            return;
        }
        let seq = self.bridge.match_local(path.to_string());
        self.match_inflight.insert(path.to_string(), seq);
    }

    /// 收到匹配结果：补进匹配表，并就地给正在播的本地曲目换上真封面。
    fn apply_local_match(&mut self, path: String, matched: Option<Track>) {
        self.match_inflight.remove(&path);
        let Some(matched) = matched else {
            if self.heartbeat_pending {
                self.heartbeat_pending = false;
                self.push_toast(self.t(
                    "这首歌在网易云没查到，开不了心动模式",
                    "NO NETEASE MATCH — HEARTBEAT UNAVAILABLE",
                ));
            }
            return;
        };
        let started_pending = self.heartbeat_pending;
        self.local_match.insert(path.clone(), matched.clone());

        // 两份持久存储是本地曲目的**唯一事实源**：改它们，磁带架 / 浏览列表 /
        // 视图快照（`refresh_user_views` 由它们重建）才会一起换成真封面。
        // 只改当前视图会留下"这张卡有封面、那张没有"的错位。
        let mut touched = false;
        let mut lib_touched = false;
        if let Some(lib) = self.library.as_mut() {
            for t in lib.tracks.iter_mut() {
                if t.id == path {
                    merge_local_match(t, &matched);
                    touched = true;
                    lib_touched = true;
                }
            }
        }
        for list in self.local_playlists.lists.iter_mut() {
            for entry in list.entries.iter_mut() {
                if entry.track.id == path {
                    merge_local_match(&mut entry.track, &matched);
                    touched = true;
                }
            }
        }
        if touched {
            self.refresh_user_views();
        }
        if lib_touched {
            if let Some(lib) = self.library.as_ref() {
                if let Err(e) = super::library::save(lib) {
                    tracing::warn!("曲库封面回填失败（不影响播放）：{}", e.message(self.lang));
                }
            }
        }
        self.local_playlists.save();

        if started_pending {
            self.heartbeat_pending = false;
            if let Some(track) = self.now_playing_track() {
                if let Some(song_id) = self.netease_song_id(&track) {
                    let pid = self.heartbeat_pid();
                    self.request_heartbeat(&song_id, &pid, "");
                }
            }
        }
    }

    /// **偷看**「下一首」的索引（不推进随机游标；预缓存用）。
    ///
    /// 与 [`Self::next_index`] 的区别：不消耗洗牌序列的游标。预缓存需要提前知道
    /// 下一首是哪首，但不能把游标推进——否则真正切歌时 `next_index` 再推一次，
    /// 会跳过刚预取的那一首。
    fn peek_next_index(&mut self) -> Option<usize> {
        let len = self.playback_tracks().len();
        if len == 0 {
            return None;
        }
        if self.shuffle_effective() && len > 1 {
            self.ensure_shuffle_order();
            let order = self.shuffle_order.as_ref().expect("just ensured");
            return Some(order[(self.shuffle_pos + 1) % len]);
        }
        let current = self.current_index().unwrap_or(usize::MAX);
        if current == usize::MAX {
            Some(0)
        } else if current + 1 < len {
            Some(current + 1)
        } else if self.core.repeat_mode.is_all() {
            Some(0)
        } else {
            None
        }
    }

    /// 计算「下一首」的索引（随机 / 顺序 / 列表循环共用一套逻辑）。
    ///
    /// 返回 `None` = 无下一首（顺序模式走到末尾且未开列表循环）。
    fn next_index(&mut self) -> Option<usize> {
        let len = self.playback_tracks().len();
        if len == 0 {
            return None;
        }
        if self.shuffle_effective() && len > 1 {
            self.ensure_shuffle_order();
            let order = self.shuffle_order.as_ref().expect("just ensured");
            self.shuffle_pos = (self.shuffle_pos + 1) % len;
            return Some(order[self.shuffle_pos]);
        }
        self.peek_next_index()
    }

    /// 计算「上一首」的索引（随机走洗牌序列回退，顺序/循环共用一套逻辑）。
    fn prev_index(&mut self) -> Option<usize> {
        let len = self.playback_tracks().len();
        if len == 0 {
            return None;
        }
        if self.shuffle_effective() && len > 1 {
            self.ensure_shuffle_order();
            // 游标 = 当前曲位置：前一首就是 pos-1（回绕）
            let order = self.shuffle_order.as_ref().expect("just ensured");
            self.shuffle_pos = (self.shuffle_pos + len - 1) % len;
            return Some(order[self.shuffle_pos]);
        }
        let current = self.current_index().unwrap_or(usize::MAX);
        if current == usize::MAX || current == 0 {
            if self.core.repeat_mode.is_all() {
                Some(len - 1)
            } else {
                // 到列表开头：交给调用方提示，不原地重播
                None
            }
        } else {
            Some(current - 1)
        }
    }

    /// 前进到「下一首」并播放（手动 NEXT 按钮 / 媒体键）。
    fn advance_next(&mut self) {
        let Some(next) = self.next_index() else {
            self.push_toast(self.t("播放列表已走完", "End of playlist"));
            return;
        };
        let Some(track) = self.playback_tracks().get(next).cloned() else {
            return;
        };
        self.reveal_in_browse(&track);
        self.play_track(track);
    }

    /// 后退到「上一首」并播放（手动 PREV 按钮 / 媒体键）。
    fn advance_prev(&mut self) {
        let Some(prev) = self.prev_index() else {
            self.push_toast(self.t("已到列表开头", "START OF PLAYLIST"));
            return;
        };
        let Some(track) = self.playback_tracks().get(prev).cloned() else {
            return;
        };
        self.reveal_in_browse(&track);
        self.play_track(track);
    }

    /// 舱体动画：机械时间线驱动（开舱→退带→装带→落座→合舱→咔哒，
    /// 时序与 `code (3).html` openTray/ejectTape/insertTape/closeTray 一致）。
    fn animate_deck(&mut self, dt: f32) {
        // 换带检测：当前磁带 ID 变化 → 启动装载时间线（HTML loadTape）
        let cur_id = self.core.now_playing().map(|t| t.id.clone());
        if cur_id != self.deck_tape_id {
            // 舱内已有磁带（含 boot 装入的空带壳）→ 先退带
            let had_tape = self.insert > 0.5;
            self.deck_tape_id = cur_id;
            if self.deck_tape_id.is_some() || had_tape {
                self.deck_seq.start_load(had_tape);
            }
        }
        let frame = self.deck_seq.tick(dt);
        self.tray_open = frame.open;
        self.insert = frame.insert;
        self.seat_dy = frame.seat_dy;
        self.well_dy = frame.well_dy;
        if frame.just_finished && self.deck_seq.is_boot() {
            self.push_toast(self.t("播放通道就绪", "PLAYBACK READY"));
        }
    }

    /// 歌词滚动焦点：仅换行帧请求滚到中心。
    fn update_lyric_focus(&mut self) {
        let active = self.core.active_lyric();
        self.lyric_focus = if active != self.last_lyric {
            active
        } else {
            None
        };
        self.last_lyric = active;
    }

    /// 向桌面歌词分发中心推一帧（接口预留，无渲染端时是空操作）。
    ///
    /// 门控在 [`desktop_lyric::DesktopLyricHub`] 内部，这里先做一次廉价短路，
    /// 避免每帧为无用的 frame 构造字符串。
    fn push_desktop_lyric(&mut self) {
        if !self.desktop_lyric.enabled() || !self.desktop_lyric.has_sink() {
            return;
        }
        let (title, artist, playing) = match self.core.now_playing() {
            Some(now) => (
                now.title.as_str(),
                now.artist.as_str(),
                self.core.engine_state() == "Playing",
            ),
            None => ("", "", false),
        };
        let lyrics = self.core.session().lyrics();
        let index = self.core.active_lyric();
        let line_of = |i: usize| lyrics.and_then(|l| l.lines.get(i)).map(|l| l.text.as_str());
        let line = index.and_then(line_of);
        let next = index.and_then(|i| line_of(i + 1));
        // 行内进度：本行起点 → 下一行起点（末行按 4 秒估，只用于逐字染色）
        let progress = match (index, lyrics) {
            (Some(i), Some(l)) => {
                let start = l.lines[i].start_ms;
                let end = l
                    .lines
                    .get(i + 1)
                    .map(|n| n.start_ms)
                    .unwrap_or(start + 4000);
                if end > start {
                    let pos = self.core.progress().position.as_millis() as u64;
                    ((pos.saturating_sub(start)) as f32 / (end - start) as f32).clamp(0.0, 1.0)
                } else {
                    0.0
                }
            }
            _ => 0.0,
        };
        let frame = LyricFrame {
            title,
            artist,
            line,
            next,
            line_progress: progress,
            playing,
        };
        self.desktop_lyric.update(&frame);
        // 心跳：deferred viewport 必须每个父 pass 被声明一次，否则 egui 会把
        // 它当"本 pass 未使用"销毁——暂停时去重不再 present，窗口就消失了。
        self.desktop_lyric.tick();
    }

    // ------------------------------------------------------------------
    // 窗口装配
    // ------------------------------------------------------------------

    fn ui_window_title(&mut self, ui: &mut Ui) -> Vec<WindowAction> {
        // 标题栏：条纹底 + closebox 三钮 + 居中标题 + 版本号（≈28px）
        let rect = ui
            .allocate_exact_size(
                vec2(ui.available_width(), window::TITLEBAR_HEIGHT),
                Sense::hover(),
            )
            .0;
        let painter = ui.painter();
        let mut y = rect.min.y;
        while y < rect.max.y {
            let band = Rect::from_min_max(
                pos2(rect.min.x, y),
                pos2(rect.max.x, (y + 2.0).min(rect.max.y)),
            );
            painter.rect_filled(band, radius_none(), pal().paper);
            let stripe = Rect::from_min_max(
                pos2(rect.min.x, (y + 2.0).min(rect.max.y)),
                pos2(rect.max.x, (y + 3.0).min(rect.max.y)),
            );
            painter.rect_filled(stripe, radius_none(), pal().titlebar_stripe);
            y += 3.0;
        }
        painter.rect_stroke(rect, radius_none(), stroke_hairline(), StrokeKind::Inside);

        // 三钮（System 6 closebox 风格）+ 标题栏拖动/双击
        let buttons = window::layout_title_buttons(rect);
        let maximized = ui.input(|i| i.viewport().maximized.unwrap_or(false));
        // 标题栏右侧已无会话按钮（登录入口移入设置面板），拖动热区就是整条标题栏
        let actions = window::titlebar_interact(ui, rect, &buttons, maximized, &[]);

        // 标题随页面切换（code (3).html winTitles），在「三钮右缘 ↔ 版本号左缘」居中
        let title_left = buttons.maximize.max.x + 8.0;
        let title_right = window::title_text_right(rect);
        widgets::paint_spec_text(
            ui,
            pos2((title_left + title_right) / 2.0, rect.center().y),
            Align2::CENTER_CENTER,
            self.page.title(self.lang),
            FONT_WINDOW_TITLE,
            TRACKING_WINDOW_TITLE,
            pal().ink,
            false,
            true,
        );
        widgets::paint_spec_text(
            ui,
            window::version_anchor(rect),
            Align2::RIGHT_CENTER,
            "v 2.001",
            FONT_TINY,
            TRACKING_TINY,
            pal().mid,
            true,
            false,
        );
        actions
    }

    /// 页面导航（贴标题栏下方居中；选中蓝底反白）。
    ///
    /// 五页：Player / Search / Rec / Daily / Playlist。Rec 与 Daily 紧挨排——
    /// 两者都是"发现音乐"，语义成组。宽度按 [`widgets::nav_button_width`] 实测
    /// 累加（早期用近似公式会把整排推到偏左，页签越多偏得越狠）。
    fn ui_page_nav(&mut self, ui: &mut Ui) {
        // 页签文案跟着语言走（见 `Page::nav_label`），图标与顺序不变
        let lang = self.lang;
        let pages = [
            (Page::Player, icons::PLAY, Page::Player.nav_label(lang)),
            (Page::Search, icons::SEARCH, Page::Search.nav_label(lang)),
            (Page::Rec, icons::STAR, Page::Rec.nav_label(lang)),
            (Page::Daily, icons::CLOCK, Page::Daily.nav_label(lang)),
            (Page::Playlist, icons::LIST, Page::Playlist.nav_label(lang)),
        ];
        ui.horizontal(|ui| {
            let mut widths = 0.0;
            for (_, _, label) in &pages {
                widths += widgets::nav_button_width(ui, label);
            }
            let gaps = NAV_GAP * (pages.len() - 1) as f32;
            let spare = (ui.available_width() - widths - gaps).max(0.0);
            ui.add_space(spare / 2.0);
            for (i, (page, glyph, label)) in pages.into_iter().enumerate() {
                if widgets::nav_button(
                    ui,
                    ui.scope_id().with(format!("nav-{i}")),
                    glyph,
                    label,
                    self.page == page,
                )
                .clicked()
                {
                    self.set_page(page);
                }
                if i + 1 < pages.len() {
                    ui.add_space(NAV_GAP);
                }
            }
        });
        ui.add_space(4.0);
    }

    // ------------------------------------------------------------------
    // Player 页
    // ------------------------------------------------------------------

    fn ui_page_player(&mut self, ui: &mut Ui) {
        // 一次性吃掉整页空间，再由它派生两列矩形（避免纵向 allocate 把右列推出视口）
        let (body, _) = ui.allocate_exact_size(
            vec2(ui.available_width(), ui.available_height()),
            Sense::hover(),
        );
        // .main-grid{grid-template-columns:1.02fr 1fr}；gap 10
        let (left_w, right_w) = super::layout::columns(body.width() - GAP_PANEL, 1.02, 1.0);
        let left_rect = Rect::from_min_size(body.min, vec2(left_w, body.height()));
        let right_rect = Rect::from_min_size(
            pos2(left_rect.max.x + GAP_PANEL, body.min.y),
            vec2(right_w, body.height()),
        );

        self.ui_player_left(ui, left_rect);
        self.ui_player_right(ui, right_rect);
    }

    /// 左列：Now Playing（舱体 + 曲目信息 + 波形走带 + 控制 + 歌词）。
    ///
    /// 面板矩形由 [`ui_page_player`] 给定并**撑满列高**（CSS grid 的 stretch）；
    /// art 与歌词按剩余高度分配，窗口变化时自动伸缩。
    fn ui_player_left(&mut self, ui: &mut Ui, rect: Rect) {
        let maximized = ui.input(|i| i.viewport().maximized.unwrap_or(false));
        let inner_h = (rect.height() - 2.0 * PAD_PANEL - 2.0 * STROKE_COMPONENT).max(0.0);
        // 固定块：panel_head 31 + 五个 12/10 gap + track 52 + wave 57 + controls 42 + utility 64
        const FIXED_H: f32 = 31.0 + 12.0 + 52.0 + 10.0 + 57.0 + 12.0 + 42.0 + 12.0 + 64.0 + 12.0;
        let art_h = super::layout::art_height(inner_h, maximized);
        // 歌词吃掉全部剩余高度（不设上限）：CSS grid 里 lyric 区是 1fr，会一直撑到列底；
        // 早期版本 clamp 到 260 会在窗口拉高时留出一条空白（实测 1100×860 下空 93px）。
        let lyrics_h = (inner_h - FIXED_H - art_h).max(24.0 + 112.0);

        widgets::panel_at(ui, rect, |ui| {
            // 机械动作期间显示 DECK 状态文案（HTML deckStatus）
            let status = self
                .deck_seq
                .status()
                .map(str::to_string)
                .unwrap_or_else(|| self.core.deck_status());
            widgets::panel_head(ui, self.t("正在播放", "Now Playing"), &status);
            self.ui_deck_art(ui, art_h);
            ui.add_space(12.0);
            self.ui_track_info(ui);
            ui.add_space(10.0);
            self.ui_wave_and_seek(ui);
            ui.add_space(12.0);
            self.ui_controls(ui);
            ui.add_space(12.0);
            self.ui_volume_row(ui);
            ui.add_space(12.0);
            self.ui_lyrics(ui, lyrics_h);
        });
    }

    /// art 区：蓝底条纹 + 贴纸 + 磁带舱（well/door/装带动画）。
    fn ui_deck_art(&mut self, ui: &mut Ui, height: f32) {
        let rect = ui
            .allocate_exact_size(vec2(ui.available_width(), height), Sense::hover())
            .0;
        let painter = ui.painter();
        painter.rect_filled(rect, radius_none(), pal().art_bg);
        let mut y = rect.min.y + 7.0;
        while y < rect.max.y {
            painter.rect_filled(
                Rect::from_min_max(
                    pos2(rect.min.x, y),
                    pos2(rect.max.x, (y + 1.0).min(rect.max.y)),
                ),
                radius_none(),
                pal().art_stripe,
            );
            y += 8.0;
        }
        painter.rect_stroke(
            rect.shrink(16.0),
            radius_none(),
            Stroke::new(1.0, pal().art_inset),
            StrokeKind::Inside,
        );

        // 贴纸（acid，旋转 -4°）
        let sticker_center = rect.min + vec2(52.0, 26.0);
        let (hw, hh) = (38.0, 11.0);
        let angle = -4.0_f32.to_radians();
        let (c, s) = (angle.cos(), angle.sin());
        let corners: Vec<Pos2> = [vec2(-hw, -hh), vec2(hw, -hh), vec2(hw, hh), vec2(-hw, hh)]
            .into_iter()
            .map(|v| sticker_center + vec2(v.x * c - v.y * s, v.x * s + v.y * c))
            .collect();
        painter.add(egui::Shape::convex_polygon(
            corners,
            pal().acid,
            Stroke::new(1.0, pal().hub_dark),
        ));
        widgets::paint_spec_text(
            ui,
            sticker_center,
            Align2::CENTER_CENTER,
            "FUTURE IS NOW!",
            8.0,
            0.08,
            pal().hub_dark,
            false,
            true,
        );
        // SIDE 标签放**右上角**：舱体+磁带硬投影会盖住右下角（早期放右下会被
        // 磁带边缘吃掉前几个字），右上只剩贴纸在左，互不干扰。
        widgets::paint_spec_text(
            ui,
            pos2(rect.max.x - 12.0, rect.min.y + 12.0),
            Align2::RIGHT_TOP,
            if self.core.now_playing().map(|n| n.is_local).unwrap_or(false) {
                "SIDE A — LOCAL"
            } else {
                "SIDE A — ONLINE"
            },
            FONT_TINY,
            TRACKING_TINY,
            fg_on(pal().art_bg),
            true,
            false,
        );

        // 磁带舱（居中，330px 上限，宽高比 1.56）
        let deck_w = (rect.width() * 0.84).min(330.0);
        let deck_rect = Rect::from_center_size(
            rect.center() + vec2(0.0, 6.0),
            vec2(deck_w, deck_w / widgets::CASSETTE_RATIO),
        );
        let now = self.core.now_playing().cloned();
        let (cover, title, artist, spin) = match now {
            Some(now) => {
                // 真实封面 → 点阵化；无 cover_url / 拉取失败 → 程序化点阵兜底
                let cover = self.cover_texture_url(ui.ctx(), &now.id, now.cover_url.as_deref());
                let playing = self.core.engine_state() == "Playing";
                let spin = if playing { self.t * 106.0 } else { 0.0 };
                (cover, now.title.clone(), now.artist.clone(), spin)
            }
            None => (
                None,
                self.t("未装磁带", "NO TAPE").to_string(),
                "—".to_string(),
                0.0,
            ),
        };
        let view = DeckView {
            cassette: CassetteView {
                title: &title,
                artist: &artist,
                cover,
                spin_deg: spin,
            },
            open: self.tray_open,
            insert: self.insert,
            seat_dy: self.seat_dy,
            well_dy: self.well_dy,
        };
        widgets::deck_at(ui, deck_rect, &view);
    }

    /// 曲目信息（track-title 24/900 + 艺人 + HI-FI 徽标）。
    fn ui_track_info(&mut self, ui: &mut Ui) {
        let (title, artist, tag) = match self.core.now_playing() {
            Some(now) => (
                now.title.clone(),
                if now.artist.is_empty() {
                    "—".to_string()
                } else {
                    now.artist.clone()
                },
                if now.is_local {
                    self.t("本地", "LOCAL")
                } else {
                    "HI-FI"
                },
            ),
            None => (
                self.t("未装磁带", "NO TAPE LOADED").to_string(),
                self.t(
                    "搜索/歌单选择曲目，或装载本地文件",
                    "Pick a track from search or a playlist, or load a local file",
                )
                .to_string(),
                self.t("空", "EMPTY"),
            ),
        };
        // 仅调试构建：探针可强制替换标题以校对走马灯（见 probe::forced_title）
        #[cfg(debug_assertions)]
        let title = super::probe::forced_title().unwrap_or(title);
        #[cfg(not(debug_assertions))]
        let title = title;
        let rect = ui
            .allocate_exact_size(vec2(ui.available_width(), 52.0), Sense::hover())
            .0;
        // 右侧徽标先画：它的左缘就是标题的**硬右界**
        let tag_job = widgets::spec_job(tag, FONT_TINY, TRACKING_TINY, pal().ink, true);
        let tag_rect = widgets::paint_job(ui, rect.right_top(), Align2::RIGHT_TOP, tag_job, false)
            .expand2(vec2(5.0, 3.0));
        ui.painter().rect_stroke(
            tag_rect,
            radius_none(),
            stroke_hairline(),
            StrokeKind::Outside,
        );

        // 标题：超宽时走马灯，且裁剪到徽标左侧 —— 绝不压住 LOCAL 标签
        if self.marquee_title != title {
            self.marquee_title = title.clone();
            self.marquee_t = 0.0;
        }
        self.marquee_t += ui.input(|i| i.stable_dt).min(0.1);
        let title_area = Rect::from_min_max(
            rect.left_top(),
            pos2(
                (tag_rect.min.x - 8.0).max(rect.min.x + 24.0),
                rect.min.y + FONT_TRACK_TITLE + 8.0,
            ),
        );
        let title_job = widgets::spec_job_weight(
            &title,
            FONT_TRACK_TITLE,
            TRACKING_TRACK_TITLE,
            pal().ink,
            false,
            widgets::Weight::Black,
        );
        let scrolling = widgets::paint_marquee(ui, title_area, title_job, true, self.marquee_t);
        if scrolling {
            // 滚动中必须连续重绘（静止时不必）
            ui.ctx().request_repaint();
        }

        widgets::paint_spec_text(
            ui,
            rect.left_top() + vec2(0.0, 30.0),
            Align2::LEFT_TOP,
            &artist,
            11.0,
            0.02,
            pal().ink,
            false,
            false,
        );
    }

    /// 波形走带 + 时间行（点击波形 seek）。
    fn ui_wave_and_seek(&mut self, ui: &mut Ui) {
        let ratio = self.core.progress_ratio();
        let (_, seek) = widgets::waveform(ui, ui.scope_id().with("wave"), ratio);
        if let Some(seek) = seek {
            if seek.commit {
                // 拖动中只显示预览线，松手才真正 seek：每帧 seek 会让音频卡顿
                self.seek_preview = None;
                if let Err(reason) = self.core.seek_ratio(seek.ratio) {
                    self.push_toast(reason.message(self.lang));
                }
            } else {
                self.seek_preview = Some(seek.ratio);
                ui.ctx().request_repaint();
            }
        }
        ui.add_space(6.0);
        let p = self.core.progress();
        let elapsed = format_duration(p.position);
        let total = if self.show_remaining {
            p.total
                .map(|t| format!("-{}", format_duration(t.saturating_sub(p.position))))
                .unwrap_or_else(|| "--:--".to_string())
        } else {
            p.total
                .map(format_duration)
                .unwrap_or_else(|| "--:--".to_string())
        };
        if widgets::time_row(ui, &elapsed, &total).clicked() {
            self.show_remaining = !self.show_remaining;
        }
    }

    /// 控制行（§3.4 五键：播放模式 / 上一曲 / 播放暂停（主键）/ 下一曲 / 心动模式）。
    ///
    /// 两端各一个键，交互照播放键：点一下走一档，键自身不做「按住下沉」位移。
    ///
    /// - 左键：四档播放模式循环 `→` / `⤨` / `↻1` / `↻`（顺序→随机→单曲循环→列表循环）。
    ///   按用户要求**四个档位都不加 acid 高亮**——当前档只靠字形自证；
    /// - 右键：心动模式开关（`heart-pulse` 字形，与「喜欢」的实心♥不同形），
    ///   只有进行中也即已切到心动数据源时才用与播放主键同色的 acid 底标注。
    ///
    /// 顺序按用户要求把「上一曲」与模式键左右互换。
    ///
    /// 整组居中用**手算 `add_space`**：egui 的 `Layout::with_main_align(Center)`
    /// 只影响单个子控件在分配矩形内的对齐，**不会**把一串子控件整体居中
    /// （实测会把整组推到左边）。所以这里按已知按钮尺寸精确累加。
    fn ui_controls(&mut self, ui: &mut Ui) {
        // 与 `widgets::control_size` 保持一致：普通键 37×34、主键 54×42、间距 8
        const GAP: f32 = 8.0;
        // 主键 42 + 2px 硬投影
        const ROW_H: f32 = 44.0;

        // 整行一次性分配，五个键**共用同一条中心线**（见 `control_row_layout`）。
        //
        // 不能用 `ui.horizontal`：普通键 34px 与主键 42px 混排时，egui 把它们
        // 顶边对齐，主键多出的 8px 全堆在下方，中心整整低 4px。
        let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::hover());
        let mode = self.core.play_mode();
        let mode_spec = ControlSpec {
            glyph: self.play_mode_glyph(mode),
            primary: false,
            toggled: false,
            // 用户要求：模式键四档都不亮，档位差异全压在字形上
            active: false,
        };
        let prev_spec = ControlSpec {
            glyph: icons::PREV,
            primary: false,
            toggled: false,
            active: false,
        };
        let playing = self.core.engine_state() == "Playing";
        let play_spec = ControlSpec {
            glyph: if playing { icons::PAUSE } else { icons::PLAY },
            primary: true,
            toggled: false,
            // 只有「播放中」才用 acid 强调；暂停时与面板背景同色
            active: playing,
        };
        let next_spec = ControlSpec {
            glyph: icons::NEXT,
            primary: false,
            toggled: false,
            active: false,
        };
        // 心动模式键只在**数据源真的切过去之后**才亮：按下去但请求还在路上、
        // 或者本地曲目压根没匹配上，键面都不给高亮——否则用户以为已经开了。
        let heart_spec = ControlSpec {
            glyph: icons::HEARTBEAT,
            primary: false,
            toggled: false,
            active: self.heartbeat_on,
        };
        let centers = widgets::control_row_layout(
            row,
            &[&mode_spec, &prev_spec, &play_spec, &next_spec, &heart_spec],
            GAP,
        );

        let mode_id = ui.scope_id().with("btn-play-mode");
        let mode_resp = widgets::control_button_at(ui, mode_id, &mode_spec, centers[0]);
        if let Some(badge) = self.play_mode_badge(mode) {
            // 角标贴在键面右下角，避开 ↻ 的环；用界面等宽字体而不是图标字库
            let rect = Rect::from_center_size(centers[0], widgets::control_size(&mode_spec));
            widgets::paint_job(
                ui,
                rect.right_bottom() - vec2(3.0, 2.0),
                Align2::RIGHT_BOTTOM,
                widgets::spec_job(badge, 9.0, 0.0, pal().ink, true),
                false,
            );
        }
        let next_label = self.play_mode_label(mode.next());
        if mode_resp
            .on_hover_text(self.t(
                &format!("{}（点击：{}）", self.play_mode_label(mode), next_label),
                &format!("{} (click: {})", self.play_mode_label(mode), next_label),
            ))
            .clicked()
        {
            self.cycle_play_mode();
        }
        if widgets::control_button_at(ui, ui.scope_id().with("btn-prev"), &prev_spec, centers[1])
            .clicked()
        {
            self.advance_prev();
        }
        if widgets::control_button_at(ui, ui.scope_id().with("btn-play"), &play_spec, centers[2])
            .clicked()
        {
            if self.core.now_playing().is_none() {
                self.play_selected();
            } else if let Err(reason) = self.core.toggle() {
                self.push_toast(reason.message(self.lang));
            }
        }
        if widgets::control_button_at(ui, ui.scope_id().with("btn-next"), &next_spec, centers[3])
            .clicked()
        {
            self.advance_next();
        }
        let heart_label = if self.heartbeat_on {
            self.t(
                "心动模式：进行中（点击关闭）",
                "Heartbeat: ON (click to turn off)",
            )
        } else if self.heartbeat_pending {
            self.t(
                "心动模式：正在识别本地曲目…",
                "Heartbeat: identifying local track…",
            )
        } else {
            self.t(
                "心动模式：关闭（点击开启）",
                "Heartbeat: OFF (click to turn on)",
            )
        };
        if widgets::control_button_at(
            ui,
            ui.scope_id().with("btn-heartbeat"),
            &heart_spec,
            centers[4],
        )
        .on_hover_text(heart_label)
        .clicked()
        {
            self.toggle_heartbeat();
        }
    }

    /// 某一档模式键的字形（四档四个不同形状，不靠颜色区分）。
    ///
    /// 单曲循环**不能**用 Font Awesome 的 `repeat-1`（U+F365）：随包的
    /// `fa-solid-900.ttf` 在该字体的 Unicode（cmap format 12）子表里没有这个码点，
    /// 实测渲染成一个空心豆腐块。所以这里用真正存在的 `repeat` 打底，再叠一个
    /// UI 等宽字体的 `1` 角标（见 [`Self::play_mode_badge`]）——形状差异仍然成立，
    /// 且不依赖缺字。
    fn play_mode_glyph(&self, mode: PlayMode) -> &'static str {
        match mode {
            PlayMode::Sequential => icons::SEQUENTIAL,
            PlayMode::Shuffle => icons::SHUFFLE,
            PlayMode::RepeatOne | PlayMode::RepeatAll => icons::REPEAT,
        }
    }

    /// 模式键的角标字形（用界面等宽字体画，不用图标字库）。
    fn play_mode_badge(&self, mode: PlayMode) -> Option<&'static str> {
        match mode {
            PlayMode::RepeatOne => Some("1"),
            _ => None,
        }
    }

    /// 音量行：`VOL` 标签 + 滑杆 + 数值。
    ///
    /// 这里原来还有两样东西，已按用户要求删除：
    /// - **模式胶囊**（`MODE: SEQUENTIAL` 等）——纯显示，占位却比音量条还宽；
    /// - **STOP 按钮**——它当初是从歌单页 LOCAL 行迁来的，现在播放器页也不再需要。
    ///   中断播放仍可用播放/暂停键（`PLAY` ↔ `PAUSE`）。
    /// 写入音量但不落盘（拖动中每帧都会调用）。
    fn set_volume_core(&mut self, volume: f32) {
        let volume = volume.clamp(0.0, 1.0);
        if volume > 0.0 {
            self.muted = false;
            self.pre_mute_volume = volume;
        } else {
            self.muted = true;
        }
        self.core.set_volume(volume);
    }

    /// 静音 / 取消静音（记住静音前音量）。
    fn toggle_mute(&mut self) {
        if self.muted {
            let restore = self.pre_mute_volume.max(0.05);
            self.muted = false;
            self.core.set_volume(restore);
        } else {
            let current = self.core.volume();
            if current > 0.0 {
                self.pre_mute_volume = current;
            }
            self.muted = true;
            self.core.set_volume(0.0);
        }
        self.persist_prefs();
    }

    /// 音量增减（滚轮 / 键盘；从静音态调大即自动取消静音）。
    fn adjust_volume(&mut self, delta: f32) {
        let base = if self.muted {
            self.pre_mute_volume
        } else {
            self.core.volume()
        };
        self.set_volume_core(base + delta);
        self.persist_prefs();
    }

    fn ui_volume_row(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let (vol_rect, _) = ui.allocate_exact_size(vec2(30.0, 18.0), Sense::click());
            let vol_resp = ui.interact(vol_rect, ui.scope_id().with("vol-label"), Sense::click());
            widgets::paint_spec_text(
                ui,
                vol_rect.center(),
                Align2::CENTER_CENTER,
                if self.muted { "MUTE" } else { "VOL" },
                FONT_TINY,
                TRACKING_TINY,
                if self.muted {
                    pal().error_mark
                } else {
                    pal().ink
                },
                true,
                false,
            );
            let vol_resp =
                vol_resp.on_hover_text(self.t("点击静音 / 取消静音", "Click to mute / unmute"));
            if vol_resp.clicked() {
                self.toggle_mute();
            }
            ui.add_space(8.0);
            let mut volume = if self.muted { 0.0 } else { self.core.volume() };
            let (changed, slider_resp) = widgets::range_slider(
                ui,
                ui.scope_id().with("vol"),
                &mut volume,
                0.0,
                1.0,
                92.0,
                false,
            );
            if changed {
                self.set_volume_core(volume);
            }
            if slider_resp.drag_stopped() || slider_resp.clicked() {
                self.persist_prefs();
            }
            // 滚轮调音量：指针停在滑杆上即可，无需先精确点中滑块
            if slider_resp.hovered() {
                let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                if scroll.abs() > 0.0 {
                    self.adjust_volume(scroll.signum() * VOLUME_STEP);
                }
            }
            ui.add_space(6.0);
            let num = ui.label(widgets::spec_job(
                &format!("{:02}", (volume * 100.0).round() as i32),
                FONT_TINY,
                TRACKING_TINY,
                if self.muted {
                    pal().error_mark
                } else {
                    pal().mid
                },
                true,
            ));
            if ui
                .interact(num.rect, ui.scope_id().with("vol-num"), Sense::click())
                .clicked()
            {
                self.toggle_mute();
            }

            // ---- 红心：音量行最右侧（"喜欢当前曲目"的唯一就地入口）----
            //
            // 放在这里而不是曲目信息面板：音量行右侧本来就是空的，而红心是
            // 播放时最常点的动作，贴着播放控件更顺手。
            // 图标必须走 `paint_spec_text`（图标字体），`tiny_button` 用
            // Monospace 家族，图标字形不一定命中。
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let track = self.current_track.clone();
                let liked = track.as_ref().is_some_and(|t| self.is_liked(t));
                let (rect, _) = ui.allocate_exact_size(vec2(24.0, 18.0), Sense::click());
                let resp = ui.interact(rect, ui.scope_id().with("btn-like"), Sense::click());
                let color = match (&track, liked) {
                    (None, _) => with_opacity(pal().mid, 0.35),
                    (Some(_), true) => pal().favorite,
                    (Some(_), false) => with_opacity(pal().ink, 0.55),
                };
                widgets::paint_spec_text(
                    ui,
                    rect.center(),
                    Align2::CENTER_CENTER,
                    icons::HEART,
                    14.0,
                    0.0,
                    color,
                    false,
                    false,
                );
                let hint = match (&track, liked) {
                    (None, _) => self.t("当前没有播放中的曲目", "Nothing is playing"),
                    (Some(_), true) => self.t(
                        "取消喜欢（同步到「我喜欢的音乐」）",
                        "Unlike (syncs to your liked songs)",
                    ),
                    (Some(_), false) => self.t(
                        "喜欢（同步到「我喜欢的音乐」）",
                        "Like (syncs to your liked songs)",
                    ),
                };
                let resp = resp.on_hover_text(hint);
                if resp.clicked() {
                    match track {
                        Some(track) => self.toggle_like(&track),
                        None => self.push_toast(hint),
                    }
                }
            });
        });
    }

    /// 歌词面板（时间轴同步 + 点击行 seek + 换行滚到中心）。
    fn ui_lyrics(&mut self, ui: &mut Ui, height: f32) {
        let note = self.core.session().lyric_note();
        let empty_text = String::new();
        let fallback;
        let rows_data: Vec<(String, String)> = match self.core.session().lyrics() {
            Some(lyrics) => lyrics
                .lines
                .iter()
                .map(|l| {
                    (
                        format_duration(Duration::from_millis(l.start_ms)),
                        l.text.clone(),
                    )
                })
                .collect(),
            None => {
                // `note` 是类型化的歌词侧提示（本地文件无歌词 / 暂无 / 失败），
                // 措辞在这里按界面语言渲染——核心层不产出用户可见文案
                fallback = note.map(|n| n.message(self.lang)).unwrap_or_else(|| {
                    self.t("装载后显示歌词", "Lyrics appear after loading")
                        .to_string()
                });
                vec![(String::new(), fallback.clone())]
            }
        };
        let _ = empty_text;
        let rows: Vec<LyricRow> = rows_data
            .iter()
            .map(|(t, x)| LyricRow {
                time_text: t.as_str(),
                text: x.as_str(),
            })
            .collect();
        let clicked = widgets::lyric_panel(
            ui,
            self.lang,
            ui.scope_id().with("lyrics"),
            &rows,
            self.core.active_lyric(),
            self.lyric_focus,
            height,
        );
        if let Some(i) = clicked {
            if let Some(lyrics) = self.core.session().lyrics() {
                if let Some(line) = lyrics.lines.get(i) {
                    let target = Duration::from_millis(line.start_ms);
                    if let Err(reason) = self.core.seek_to(target) {
                        self.push_toast(reason.message(self.lang));
                    }
                }
            }
        }
    }

    /// 右列：磁带架 + EQ + 曲目信息。
    fn ui_player_right(&mut self, ui: &mut Ui, rect: Rect) {
        // 曲目信息面板：3 行元数据 + 1 行音质 + 设置折叠行。
        // 折叠态 160；展开时把设置区高度加回来，避免内容被裁。
        //
        // 设置区预留高度：唯一事实源在 [`settings::PANEL_H`]，
        // 由单测 `settings_panel_fits_its_reserved_height` 用真实 egui
        // 上下文实测把关（**改设置面板内容只需改那里**）。
        // 踩过的坑：留 88 时 `OFF` 按钮正好被面板底边切掉一半（截图实测差 11px）。
        const MONITOR_H: f32 = 160.0;
        const SETTINGS_H: f32 = settings::PANEL_H;
        const EQ_H: f32 = 130.0;
        let monitor_h = if self.settings_open {
            MONITOR_H + SETTINGS_H
        } else {
            MONITOR_H
        };
        let shelf_h = (rect.height() - monitor_h - EQ_H - GAP_PANEL * 2.0).max(140.0);

        let shelf_rect = Rect::from_min_size(rect.min, vec2(rect.width(), shelf_h));
        let eq_rect = Rect::from_min_size(
            pos2(rect.min.x, shelf_rect.max.y + GAP_PANEL),
            vec2(rect.width(), EQ_H),
        );
        let monitor_rect = Rect::from_min_size(
            pos2(rect.min.x, eq_rect.max.y + GAP_PANEL),
            vec2(rect.width(), monitor_h),
        );

        // ---- 磁带架 ----
        let mut back = false;
        widgets::panel_at(ui, shelf_rect, |ui| {
            // 进专辑/艺人页时标题换成「专辑曲子」/「歌手曲子」，右侧给返回入口
            let title = self.shelf_title();
            let target_name = self
                .shelf_target
                .as_ref()
                .map(|t| t.name.clone())
                .unwrap_or_default();
            if self.shelf_target.is_some() {
                let right = self.t("← 返回", "← BACK");
                if widgets::panel_head_link(ui, title, right) {
                    back = true;
                }
                // 副标题：看的是哪张专辑/哪位歌手（标题保持稳定，见 user_view_title）
                let sub = widgets::fit_text(
                    ui,
                    &target_name,
                    FONT_TINY,
                    TRACKING_TINY,
                    pal().mid,
                    false,
                    true,
                    ui.available_width(),
                );
                widgets::paint_spec_text(
                    ui,
                    ui.cursor().min,
                    Align2::LEFT_TOP,
                    &sub,
                    FONT_TINY,
                    TRACKING_TINY,
                    pal().mid,
                    false,
                    true,
                );
                ui.add_space(14.0);
            } else {
                let head_right = format!(
                    "{:02} {}",
                    self.current_tracks().len(),
                    self.t("盘磁带", "TAPES")
                );
                widgets::panel_head(ui, title, &head_right);
            }
            self.ui_shelf(ui, ui.available_height());
        });
        if back {
            self.back_to_shelf();
        }

        // ---- EQ（真实频谱：FFT 2048 → 26 段对数频带）----
        // 引擎没有数据（暂停/无设备/样本不足）时走衰减，让柱子平滑落回底线，
        // 而不是瞬间归零或僵在最后一帧的形状上。
        let live = self.core.spectrum();
        for (i, slot) in self.eq_levels.iter_mut().enumerate() {
            let target = live.map(|b| b[i]).unwrap_or(0.0);
            *slot = if target >= *slot {
                target
            } else {
                (*slot * 0.82).max(target)
            };
        }
        widgets::eq_at(ui, eq_rect, &self.eq_levels, "FFT 2048 · LIVE", self.lang);

        // ---- 曲目信息（歌曲 / 艺人 / 专辑 / 音质）----
        self.ui_now_playing_panel(ui, monitor_rect);
    }

    /// 磁带架列表（spine 集；单击即装带播放）。
    fn ui_shelf(&mut self, ui: &mut Ui, height: f32) {
        let frame = self.frame_u64();
        if self.current_tracks().is_empty() {
            // 专辑/艺人页的空态要说清"是没加载完还是真没有"，不能一律写
            // "输入关键词搜索"——用户明明是从专辑名点进来的。
            let (badge_text, badge_color, hint) = if self.shelf_target.is_some() {
                let loading = self.album_state.is_loading() || self.artist_state.is_loading();
                if loading {
                    let (text, color) = widgets::badge_view(&self.album_state, frame, self.lang);
                    (
                        text,
                        color,
                        self.t("正在取曲子…", "FETCHING TRACKS…").to_string(),
                    )
                } else {
                    (
                        self.t("没有曲子", "NO TRACKS").to_string(),
                        pal().mid,
                        self.t(
                            "这张专辑/这位歌手暂时没有可播放的曲子",
                            "This album/artist has no playable tracks right now",
                        )
                        .to_string(),
                    )
                }
            } else {
                let (text, color) =
                    if self.playlist_state.is_loading() || self.search_state.is_loading() {
                        widgets::badge_view(&self.search_state, frame, self.lang)
                    } else {
                        (self.t("空磁带架", "EMPTY SHELF").to_string(), pal().mid)
                    };
                (
                    text,
                    color,
                    self.t(
                        "输入关键词搜索，或装载一个歌单",
                        "SEARCH A KEYWORD OR LOAD A PLAYLIST",
                    )
                    .to_string(),
                )
            };
            let (rect, _) = ui
                .allocate_exact_size(vec2(ui.available_width(), height.max(60.0)), Sense::hover());
            ui.painter().rect(
                rect,
                radius_none(),
                pal().paper,
                stroke_hairline(),
                StrokeKind::Inside,
            );
            widgets::paint_spec_text(
                ui,
                rect.center() - vec2(0.0, 10.0),
                Align2::CENTER_CENTER,
                &badge_text,
                FONT_SPINE_NAME,
                0.0,
                badge_color,
                false,
                false,
            );
            widgets::paint_spec_text(
                ui,
                rect.center() + vec2(0.0, 12.0),
                Align2::CENTER_CENTER,
                &hint,
                FONT_TINY,
                TRACKING_TINY,
                pal().mid,
                true,
                false,
            );
            return;
        }

        let accents = [pal().blue, pal().pink, pal().ink, pal().acid, pal().mid];
        let selected = self.selected;
        let mut clicked: Option<usize> = None;
        let mut menu_at: Option<usize> = None;
        egui::ScrollArea::vertical()
            .id_salt("tape-shelf")
            .max_height(height)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (index, track) in self.current_tracks().iter().enumerate() {
                    let view = SpineView {
                        title: &track.title,
                        artist: &track.artists.join("/"),
                        side: "A",
                        length: &track.duration_text(),
                        accent: accents[index % accents.len()],
                        selected: selected == Some(index),
                    };
                    let resp = widgets::spine_item(ui, index, &view);
                    if resp.clicked() {
                        clicked = Some(index);
                    }
                    // 右键：与结果行同一套菜单（磁带架也是曲目列表）
                    if resp.secondary_clicked() {
                        menu_at = Some(index);
                    }
                    if index + 1 < self.current_tracks().len() {
                        ui.add_space(GAP_SPINE);
                    }
                }
            });
        if let Some(index) = menu_at {
            self.open_track_menu(index);
            return;
        }
        if let Some(index) = clicked {
            self.selected = Some(index);
            self.play_selected();
        }
    }

    /// System Monitor：真实播放状态 / 网络状态 / 临时文件 / 超时控制。
    /// 右列底部面板：曲目信息（歌曲 / 艺人 / 专辑 / 音质）+ 可折叠的请求超时设置。
    ///
    /// 相比旧的 System Monitor（播放状态 / 网络 / 临时文件 + 常驻滑杆）：
    /// - 内容换成用户真正关心的磁带元数据，音质取自**真实来源**
    ///   （本地 lofty 流参数；在线接口 `level`/`type` + 下载后探测到的流参数）；
    /// - 常驻滑杆改为默认折叠，面板高度从 226 降到 162，磁带架多出 64px。
    fn ui_now_playing_panel(&mut self, ui: &mut Ui, rect: Rect) {
        // 先一次性把展示数据取成自有值，结束对 core 的借用
        let (title, artist, album, quality, head_right) = match self.core.now_playing() {
            Some(now) => (
                now.title.clone(),
                now.artist.clone(),
                now.album.clone().unwrap_or_default(),
                now.spec.describe(),
                if now.is_local {
                    self.t("本地文件", "LOCAL FILE").to_string()
                } else {
                    self.t("在线流", "ONLINE STREAM").to_string()
                },
            ),
            None => (
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                self.t("无磁带", "NO TAPE").to_string(),
            ),
        };
        let empty = title.is_empty();
        let rows = [
            (
                self.t("曲目", "TRACK"),
                if empty { "—".to_string() } else { title },
            ),
            (
                self.t("艺人", "ARTIST"),
                if empty { "—".to_string() } else { artist },
            ),
            (self.t("专辑", "ALBUM"), {
                if album.trim().is_empty() {
                    "—".to_string()
                } else {
                    album
                }
            }),
        ];
        let quality = if quality.is_empty() {
            "—".to_string()
        } else {
            quality
        };

        // 专辑/歌手跳转的目标 ID：取自当前曲目快照（`NowPlaying` 只有展示名）。
        // 本地曲目没有在线 ID → 两个都拿不到 → 行不可点（不伪造跳转）。
        let (album_id, artist_id) = match self.current_track.as_ref() {
            Some(t) => (
                t.album_id.clone().filter(|s| !s.trim().is_empty()),
                primary_artist_id(t).map(str::to_string),
            ),
            None => (None, None),
        };
        let album_label = rows[2].0;
        let album_value = rows[2].1.clone();
        let artist_label = rows[1].0;
        let artist_value = rows[1].1.clone();

        let settings_open = self.settings_open;
        let mut go_album = false;
        let mut go_artist = false;
        widgets::panel_at(ui, rect, |ui| {
            // 标题不能也叫 "Now Playing"——左上舱体面板已占用该名字，
            // 同一屏两个同名面板会让人以为是同一块内容。
            widgets::panel_head(ui, self.t("曲目信息", "Track Info"), &head_right);

            widgets::info_row(ui, rows[0].0, &rows[0].1, INFO_ROW_H, true);
            // 艺人 / 专辑：能拿到在线 ID 才可点（点击跳对应页）
            if widgets::info_row_link(
                ui,
                artist_label,
                &artist_value,
                INFO_ROW_H,
                true,
                artist_id.is_some(),
            ) {
                go_artist = true;
            }
            if widgets::info_row_link(
                ui,
                album_label,
                &album_value,
                INFO_ROW_H,
                true,
                album_id.is_some(),
            ) {
                go_album = true;
            }

            ui.add_space(6.0);
            let (line, _) =
                ui.allocate_exact_size(vec2(ui.available_width(), 1.0), egui::Sense::hover());
            ui.painter()
                .line_segment([line.left_top(), line.right_top()], stroke_hairline());
            ui.add_space(6.0);

            // 音质：真实采样率/位深/码率/编码，取不到就显示占位符
            widgets::info_row(ui, self.t("音质", "QUALITY"), &quality, INFO_ROW_H, false);

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                // 展开指示用 ASCII：`▸`/`▾` 不在 GB2312 子集字库里，会渲染成空白
                let text = if settings_open {
                    self.t("- 设置", "- SETTINGS")
                } else {
                    self.t("+ 设置", "+ SETTINGS")
                };
                if widgets::tiny_button(
                    ui,
                    ui.scope_id().with("btn-settings-toggle"),
                    text,
                    settings_open,
                )
                .clicked()
                {
                    self.settings_open = !self.settings_open;
                }
                ui.add_space(8.0);
                // 网络/装载状态放这里，占一行、不抢版面
                let status_line = self.core.status_text(self.lang);
                let status = widgets::fit_text(
                    ui,
                    &status_line,
                    FONT_TINY,
                    TRACKING_TINY,
                    pal().mid,
                    true,
                    false,
                    ui.available_width(),
                );
                ui.label(widgets::spec_job(
                    &status,
                    FONT_TINY,
                    TRACKING_TINY,
                    pal().mid,
                    true,
                ));
            });

            if settings_open {
                ui.add_space(GAP_PANEL);
                self.ui_settings(ui);
            }
        });

        // 跳转放在面板画完之后：`open_*_page` 会改 `user_view` 并重发请求，
        // 在面板内部改会让这一帧的磁带架内容与标题不一致。
        if go_album {
            if let Some(id) = album_id {
                self.open_album_page(&id, &album_value);
            }
        } else if go_artist {
            if let Some(id) = artist_id {
                self.open_artist_page(&id, &artist_value);
            }
        }
    }

    /// 设置区（折叠在曲目信息面板底部，默认收起）：音质上限 + 桌面歌词。
    ///
    /// 这里原来画的是「SEARCH TIMEOUT」滑杆，已按用户要求删除；
    /// 现在的两项都属"用户真的会调"的设置（见 [`super::settings`] 模块注释）。
    fn ui_settings(&mut self, ui: &mut Ui) {
        let view = self.settings_view();
        let action = settings::settings_panel(ui, &view);
        if !matches!(action, SettingsAction::None) {
            // 取证：判断"真实点击"还是"代码自触发"的最后一块拼图是**指针在哪**。
            // 指针落在按钮矩形上 = 真实点击；指针不在附近 = 控件自己报了 clicked。
            let input = ui
                .ctx()
                .input(|i| (i.pointer.latest_pos(), i.pointer.any_down()));
            tracing::debug!(
                ?action,
                pointer = ?input.0,
                any_down = input.1,
                t_ms = (self.t * 1000.0) as u64,
                "settings action hit"
            );
        }
        self.apply_settings_action(action);
    }

    /// 设置面板的只读快照。
    ///
    /// 从 [`Self::ui_settings`] 拆出来：`lyric_progress` 必须**读那个共享状态**
    /// （歌词窗口读的也是它），否则面板上显示的样式和实际画出来的对不上。
    fn settings_view(&self) -> SettingsView {
        SettingsView {
            quality: self.bridge.quality(),
            desktop_lyric: self.desktop_lyric.enabled(),
            lyric_progress: self.lyric_control.progress(),
            hook_ready: self.desktop_lyric.has_sink(),
            media_keys: self.media_keys_enabled,
            tray: self.tray_enabled,
            lang: self.lang,
            theme: theme::theme_index(),
            account: self.login.logged_in(self.lang).map(str::to_string),
        }
    }

    /// 设置面板动作分发。
    ///
    /// 从 [`Self::ui_settings`] 里拆出来，是为了让「点 LOGIN 开面板」
    /// 「切语言」这类**行为**能脱离 egui 上下文被单测钉住。
    fn apply_settings_action(&mut self, action: SettingsAction) {
        if !matches!(action, SettingsAction::None) {
            // 排查"设置被莫名改动"时的唯一线索。阶段 11 曾出现启动时语言自己切到
            // 英文，事后查了很久才确认是一次真实指针点击落在 ENGLISH 按钮上——
            // 当时没有任何日志可看。`RUST_LOG=debug` 即可复现式取证。
            // `t_ms` 是判据：启动几毫秒内触发 = 代码/egui 自触发，秒级 = 真人点击。
            tracing::debug!(
                ?action,
                lang = ?self.lang,
                t_ms = (self.t * 1000.0) as u64,
                "settings action applied"
            );
        }
        match action {
            SettingsAction::None => {}
            SettingsAction::SetQuality(level) => self.apply_quality(level),
            SettingsAction::SetLang(lang) => self.apply_lang(lang),
            SettingsAction::SetTheme(index) => self.apply_theme(index),
            SettingsAction::OpenLogin => self.open_login(),
            SettingsAction::SetDesktopLyric(on) => {
                self.desktop_lyric.set_enabled(on);
                // 关闭时把锁定态一起复位：否则「关掉再开」会得到一个刚打开就
                // 点不动的窗口，而用户此时根本不知道"锁定"这件事还留着。
                // 开窗时也复位一次，语义上是"新窗口从可交互开始"。
                self.lyric_control.set_locked(false);
                // 如实告知：开关是"分发门控"，没有渲染端时它不会显示任何东西
                let msg = if on && !self.desktop_lyric.has_sink() {
                    self.t(
                        "桌面歌词已开启（接口已预留，渲染端未接入）",
                        "Desktop lyric on (hook reserved, no renderer attached)",
                    )
                } else if on {
                    self.t("桌面歌词已开启", "Desktop lyric on")
                } else {
                    self.t("桌面歌词已关闭", "Desktop lyric off")
                };
                self.push_toast(msg);
            }
            SettingsAction::SetDesktopLyricProgress(style) => {
                self.lyric_control.set_progress(style);
                self.persist_prefs();
                let msg = format!(
                    "{}: {}",
                    self.t("歌词进度条", "Lyric progress bar"),
                    style.label(self.lang)
                );
                self.push_toast(msg);
            }
            SettingsAction::SetMediaKeys(on) => {
                self.media_keys_enabled = on;
                if on {
                    self.start_media_keys();
                    // SMTC 与媒体键同一个开关：系统浮层/蓝牙按键也要能用
                    self.start_smtc();
                    self.push_toast(self.t("媒体键与 SMTC 已开启", "Media keys and SMTC on"));
                } else {
                    self.stop_media_keys();
                    self.stop_smtc();
                    self.push_toast(self.t("媒体键与 SMTC 已关闭", "Media keys and SMTC off"));
                }
            }
            SettingsAction::SetTray(on) => {
                self.set_tray_enabled(on);
                let msg = if on {
                    self.t(
                        "托盘已开启：关闭窗口会收进托盘",
                        "Tray on: closing the window hides it to the tray",
                    )
                } else {
                    self.t("托盘已关闭", "Tray off")
                };
                self.push_toast(msg);
            }
            SettingsAction::SweepTempFiles => {
                // 用户手动触发：只清异常退出留下的旧目录，本进程与 1 小时内
                // 还在写的目录都不动（判据在 sweep_stale_scratch 内部）。
                let report = crate::music::online_playback::sweep_stale_scratch();
                let freed = if report.bytes_freed >= 1024 * 1024 {
                    format!("{:.1} MB", report.bytes_freed as f64 / (1024.0 * 1024.0))
                } else if report.bytes_freed >= 1024 {
                    format!("{:.1} KB", report.bytes_freed as f64 / 1024.0)
                } else {
                    format!("{} B", report.bytes_freed)
                };
                let msg = match (self.lang, report.dirs_removed, report.dirs_failed) {
                    (Lang::Zh, 0, 0) => "临时文件清扫：没有可清理的残留".to_string(),
                    (Lang::En, 0, 0) => "Temp sweep: nothing stale to clean".to_string(),
                    (Lang::Zh, dirs, 0) => format!(
                        "临时文件清扫：删除 {dirs} 个目录 / {} 个文件，释放 {freed}",
                        report.files_removed
                    ),
                    (Lang::En, dirs, 0) => format!(
                        "Temp sweep: removed {dirs} dirs / {} files, freed {freed}",
                        report.files_removed
                    ),
                    (Lang::Zh, dirs, failed) => format!(
                        "临时文件清扫：删除 {dirs} 个目录，释放 {freed}；{failed} 个目录未能删除",
                    ),
                    (Lang::En, dirs, failed) => format!(
                        "Temp sweep: removed {dirs} dirs, freed {freed}; {failed} could not be deleted"
                    ),
                };
                self.push_toast(msg);
            }
        }
    }

    /// 切换主题配色并持久化。
    ///
    /// 除了改 [`theme::pal()`] 背后的下标，还必须**重跑一次**
    /// [`theme::apply_global_style`]：egui 自己那批填充（面板底、选区、TextEdit
    /// 底、超链接色）是在那里一次性算出来的，不重跑就会得到"自绘的部分换了色、
    /// 内建控件还留在旧主题"的半截效果。
    fn apply_theme(&mut self, index: usize) {
        theme::set_theme_index(index);
        if let Some(ctx) = self.ctx.clone() {
            theme::apply_global_style(&ctx);
        }
        self.persist_prefs();
        let name = theme::THEMES
            .get(theme::theme_index())
            .map(|t| t.name)
            .unwrap_or("");
        self.push_toast(format!("{}: {}", self.t("主题", "Theme"), name));
    }

    /// 切换界面语言（并持久化）。
    fn apply_lang(&mut self, lang: Lang) {
        self.apply_lang_impl(lang, super::probe::forced_lang());
    }

    /// [`Self::apply_lang`] 的可测内核；`forced` 是探针强制语言（`None` = 正常运行）。
    ///
    /// 强制语言期间这里是**完全 no-op**：「强制」的语义就是当次运行钉死。放行的
    /// 后果实测过——自动化截图运行里混入真实指针点击（置顶探针窗口截获物理点击，
    /// 见 `docs/PHASE11_LOGIN_SETTINGS_I18N.md` §5），语言被切走、切换 toast 入镜、
    /// `prefs.json` 被反复改写，看起来像"语言自己变了"。
    fn apply_lang_impl(&mut self, lang: Lang, forced: Option<Lang>) {
        if forced.is_some() {
            tracing::debug!(?lang, "探针强制语言期间忽略语言切换请求");
            return;
        }
        if self.lang == lang {
            return;
        }
        self.lang = lang;
        // 悬浮窗控件条的文案跟着界面语言走（它不在主视口里，收不到重绘时的语言）
        self.desktop_lyric.set_lang(lang);
        // 用户主动切换才改偏好；探针的强制值只动 `lang`，从不经过这里
        self.pref_lang = lang;
        self.persist_prefs();
        let msg = self.t("界面语言：中文", "Language: English").to_string();
        self.push_toast(msg);
    }

    // ------------------------------------------------------------------
    // Search 页
    // ------------------------------------------------------------------

    fn ui_page_search(&mut self, ui: &mut Ui) {
        // 搜索条
        egui::Frame {
            fill: pal().panel_alt,
            stroke: stroke_component(),
            inner_margin: Margin::symmetric(14, 14),
            corner_radius: radius_none(),
            ..egui::Frame::NONE
        }
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                // 先取提示文案再借 `search_input`：TextEdit 持有 &mut，期间不能再借 self
                let hint = self.t(
                    "SEARCH / 输入关键词，或粘贴网易云分享链接…",
                    "SEARCH / keyword, or paste a NetEase share link…",
                );
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.search_input)
                        .hint_text(hint)
                        .desired_width(ui.available_width() - 190.0),
                );
                if self.focus_search {
                    response.request_focus();
                    self.focus_search = false;
                }
                let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if widgets::text_button(
                    ui,
                    ui.scope_id().with("btn-search"),
                    self.t("搜索", "SEARCH"),
                    true,
                    false,
                )
                .clicked()
                    || enter
                {
                    self.submit_search();
                }
                ui.add_space(8.0);
                if widgets::text_button(
                    ui,
                    ui.scope_id().with("btn-clear"),
                    self.t("清空", "CLEAR"),
                    false,
                    false,
                )
                .clicked()
                {
                    self.search_input.clear();
                    // 只清输入框会留下旧结果，看起来像"没清掉"
                    self.search_state.reset();
                    self.search_seq = u64::MAX;
                    self.local_hits.clear();
                    self.selected = None;
                    self.highlight_query.clear();
                }
            });
        });
        ui.add_space(GAP_PANEL);

        let total = ui.available_width();
        let height = ui.available_height();
        // .main-grid.wide{grid-template-columns:1.55fr 1fr}；gap 10（R1：显式并排矩形）
        let (left_w, right_w) = super::layout::columns(total - GAP_PANEL, 1.55, 1.0);
        let (left_rect, _) = ui.allocate_exact_size(vec2(left_w, height), Sense::hover());
        let right_rect = Rect::from_min_size(
            pos2(left_rect.max.x + GAP_PANEL, left_rect.min.y),
            vec2(right_w, height),
        );

        // ---- 结果 ----
        widgets::panel_at(ui, left_rect, |ui| {
            let count = self.search_state.get().map(|v| v.len()).unwrap_or(0);
            widgets::panel_head(
                ui,
                self.t("Results / 检索结果", "Results"),
                &format!("{:02} {}", count, self.t("条结果", "RESULTS")),
            );
            // 本地命中：有曲库且有命中时才占位，最多吃掉面板 40% 高度
            if !self.local_hits.is_empty() {
                let label = format!(
                    "{} {:02} {}",
                    self.t("本地曲库命中", "LOCAL HITS"),
                    self.local_hits.len(),
                    self.t("盘", "TAPES")
                );
                widgets::paint_spec_text(
                    ui,
                    ui.cursor().min,
                    Align2::LEFT_TOP,
                    &label,
                    FONT_TINY,
                    TRACKING_TINY,
                    pal().blue,
                    false,
                    true,
                );
                ui.add_space(12.0);
                let rows = self.local_hits.len() as f32;
                let block = (rows * 34.0).min((height * 0.4).max(68.0));
                self.ui_local_hits(ui, block);
                ui.add_space(8.0);
            }
            // 有本地命中时，在线结果为空只需一条紧凑提示——否则面板下半屏
            // 是一整块空框，看起来像界面坏了
            let rows_h = if self.local_hits.is_empty() {
                ui.available_height()
            } else {
                ui.available_height().min(96.0)
            };
            self.ui_result_rows(ui, rows_h);
        });

        // ---- 右列 ----
        ui.scope_builder(UiBuilder::new().max_rect(right_rect), |ui| {
            // Recent
            widgets::panel_frame().show(ui, |ui| {
                widgets::panel_head(
                    ui,
                    self.t("Recent / 最近搜索", "Recent"),
                    self.t("历史", "HISTORY"),
                );
                if self.recent.is_empty() {
                    ui.label(widgets::spec_job(
                        self.t("— 暂无搜索历史 —", "— NO HISTORY YET —"),
                        FONT_TINY,
                        TRACKING_TINY,
                        with_opacity(pal().ink, 0.6),
                        false,
                    ));
                } else {
                    let mut jump: Option<String> = None;
                    ui.horizontal_wrapped(|ui| {
                        for word in self.recent.clone() {
                            if ui
                                .add(egui::Button::new(widgets::spec_job(
                                    &word.to_uppercase(),
                                    FONT_TINY,
                                    0.12,
                                    pal().ink,
                                    true,
                                )))
                                .clicked()
                            {
                                jump = Some(word);
                            }
                        }
                    });
                    if let Some(word) = jump {
                        self.search_input = word;
                        self.submit_search();
                    }
                }
            });
            ui.add_space(GAP_PANEL);

            // Archive Index（真实统计）
            widgets::panel_frame().show(ui, |ui| {
                widgets::panel_head(ui, self.t("曲库索引", "Archive Index"), self.t("统计", "STATS"));
                let tracks = self.search_state.get().map(Vec::as_slice).unwrap_or(&[]);
                let artists: std::collections::BTreeSet<&str> =
                    tracks.iter().flat_map(|t| t.artists.iter().map(String::as_str)).collect();
                let total_ms: u64 = tracks
                    .iter()
                    .filter_map(|t| t.duration_ms)
                    .sum();
                for (label, value) in [
                    (
                        self.t("磁带总数", "TAPES INDEXED"),
                        format!("{:02}", tracks.len()),
                    ),
                    (self.t("艺人数", "ARTISTS"), format!("{:02}", artists.len())),
                    (
                        self.t("总时长", "TOTAL RUNTIME"),
                        format_duration(Duration::from_millis(total_ms)),
                    ),
                ] {
                    ui.horizontal(|ui| {
                        ui.label(widgets::spec_job(
                            label,
                            FONT_TINY,
                            TRACKING_TINY,
                            pal().mid,
                            true,
                        ));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(widgets::spec_job(
                                &value,
                                FONT_SPINE_NAME,
                                0.0,
                                pal().ink,
                                true,
                            ));
                        });
                    });
                    ui.add_space(4.0);
                }
            });
            ui.add_space(GAP_PANEL);

            // TIP
            ui.painter();
            egui::Frame {
                fill: pal().monitor_bg,
                stroke: Stroke::new(1.0, pal().hub_dark),
                inner_margin: Margin::same(12),
                corner_radius: radius_none(),
                ..egui::Frame::NONE
            }
            .show(ui, |ui| {
                widgets::paint_spec_text(
                    ui,
                    ui.cursor().min,
                    Align2::LEFT_TOP,
                    self.t("▣ 搜索指引", "▣ HOW TO SEARCH"),
                    FONT_TINY,
                    TRACKING_TINY,
                    pal().acid,
                    false,
                    true,
                );
                ui.add_space(14.0);
                ui.label(widgets::wrap_to(
                    widgets::spec_job(
                        self.t(
                            "输入关键词回车或按 SEARCH。也可以直接粘贴网易云分享链接（含《》分享文案）——单曲会直接装带，歌单会打开歌单页。",
                            "Type a keyword and hit Enter or SEARCH. You can also paste a NetEase share link (share text with 《》 works) — a song loads straight onto the deck, a playlist opens the playlist page.",
                        ),
                        10.0,
                        0.01,
                        with_opacity(Color32::WHITE, 0.86),
                        false,
                    ),
                    ui.available_width(),
                    8,
                ));
            });
        });
    }

    /// 本地曲库命中列表（搜索页结果面板顶部）。
    ///
    /// 与在线结果分开渲染的原因：两者的数据源与播放路径不同（本地走
    /// `play_local_path`，不触网），混进 `search_state` 会让「结果数」说谎。
    fn ui_local_hits(&mut self, ui: &mut Ui, height: f32) {
        let mut action: Option<(RowAction, usize)> = None;
        egui::ScrollArea::vertical()
            .id_salt("local-hits")
            .max_height(height)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (index, track) in self.local_hits.iter().enumerate() {
                    let view = ResRow {
                        index,
                        title: &track.title,
                        artist: &track.artists.join(" / "),
                        time: &track.duration_text(),
                        current: false,
                        selected: false,
                        favorite: self.favorites.contains(track),
                        highlight: &self.highlight_query,
                    };
                    let act = widgets::res_row(ui, ui.scope_id().with(("local-hit", index)), &view);
                    if act != RowAction::None {
                        action = Some((act, index));
                    }
                }
            });
        let Some((act, index)) = action else { return };
        let Some(track) = self.local_hits.get(index).cloned() else {
            return;
        };
        match act {
            RowAction::ContextMenu => {
                // 本地命中不在 `current_tracks` 里，按 Track 直接开菜单
                self.open_track_menu_for(track, index);
            }
            RowAction::ToggleFavorite => {
                self.toggle_like(&track);
            }
            RowAction::Select => {
                // 本地命中列表没有独立选中视觉；单击不播放
            }
            // 本地曲目一律走本地播放（不查在线曲库、不触网）
            _ => {
                self.replace_queue(self.local_hits.clone());
                self.play_track(track);
            }
        }
    }

    /// 检索结果行（搜索/歌单共用）。
    fn ui_result_rows(&mut self, ui: &mut Ui, height: f32) {
        let frame = self.frame_u64();
        if self.current_tracks().is_empty() {
            let (badge_text, badge_color) = if self.search_state.is_loading() {
                widgets::badge_view(&self.search_state, frame, self.lang)
            } else {
                (
                    self.t("没有找到磁带", "NO TAPES FOUND").to_string(),
                    pal().mid,
                )
            };
            let (rect, _) = ui
                .allocate_exact_size(vec2(ui.available_width(), height.max(60.0)), Sense::hover());
            ui.painter().rect(
                rect,
                radius_none(),
                pal().paper,
                stroke_hairline(),
                StrokeKind::Inside,
            );
            widgets::paint_spec_text(
                ui,
                rect.center(),
                Align2::CENTER_CENTER,
                &badge_text,
                FONT_SPINE_NAME,
                0.0,
                badge_color,
                false,
                false,
            );
            return;
        }

        // 表头（No / Title / Artist / Time / ▶）
        let head = ui
            .allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover())
            .0;
        ui.painter()
            .line_segment([head.left_bottom(), head.right_bottom()], stroke_hairline());
        for (label, x) in [
            (self.t("序", "No"), head.min.x + 8.0),
            (self.t("曲名", "Title"), head.min.x + 37.0),
            (
                self.t("艺人", "Artist"),
                head.min.x + 37.0 + (head.width() - 136.0) / 2.5 + 9.0,
            ),
            (self.t("时长", "Time"), head.max.x - 84.0),
        ] {
            widgets::paint_spec_text(
                ui,
                pos2(x, head.center().y),
                Align2::LEFT_CENTER,
                label,
                8.0,
                0.16,
                pal().ink,
                true,
                true,
            );
        }
        ui.add_space(4.0);

        let current_id = self.core.now_playing().map(|n| n.id.clone());
        let mut action: Option<RowAction> = None;
        let mut action_index = 0usize;
        // 列表内容变了就回顶部：否则换关键词后可能停在旧列表的空白区
        let fp = self.list_fingerprint();
        let reset_scroll = fp != self.res_scroll_fp;
        self.res_scroll_fp = fp;
        let mut area = egui::ScrollArea::vertical()
            .id_salt("res-rows")
            .max_height(height - 26.0)
            .auto_shrink([false, true]);
        if reset_scroll {
            area = area.vertical_scroll_offset(0.0);
        }
        area.show(ui, |ui| {
            for (index, track) in self.current_tracks().iter().enumerate() {
                let view = ResRow {
                    index,
                    title: &track.title,
                    artist: &track.artists.join(" / "),
                    time: &track.duration_text(),
                    current: current_id.as_deref() == Some(track.id.as_str()),
                    selected: self.selected == Some(index),
                    favorite: self.favorites.contains(track),
                    highlight: &self.highlight_query,
                };
                match widgets::res_row(ui, ui.scope_id().with(("res-row", index)), &view) {
                    RowAction::None => {}
                    other => {
                        action = Some(other);
                        action_index = index;
                    }
                }
            }
        });
        if let Some(action) = action {
            self.apply_row_action(action, action_index);
        }
    }

    // ------------------------------------------------------------------
    // Rec 页（推荐歌单 / 官方排行榜，风格与 Playlist 页统一）
    // ------------------------------------------------------------------

    /// 推荐页面板里的歌单详情（原地展开）。
    ///
    /// 视觉沿用专辑/艺人页那一套：页头左侧固定标题、右上「← 返回」，
    /// 下面一行副标题写歌单名，再往下是磁带架同款曲目 spine。
    fn ui_playlist_peek(&mut self, ui: &mut Ui, panel: PeekPanel) -> PeekHit {
        let Some(peek) = self.peek(panel) else {
            return PeekHit::None;
        };
        let mut hit = PeekHit::None;
        if widgets::panel_head_link(
            ui,
            self.t("Playlist Tracks / 歌单曲目", "Playlist Tracks"),
            self.t("← 返回", "← BACK"),
        ) {
            hit = PeekHit::Back;
        }
        let sub = widgets::fit_text(
            ui,
            &peek.subtitle,
            FONT_TINY,
            TRACKING_TINY,
            pal().mid,
            false,
            true,
            ui.available_width(),
        );
        widgets::paint_spec_text(
            ui,
            ui.cursor().min,
            Align2::LEFT_TOP,
            &sub,
            FONT_TINY,
            TRACKING_TINY,
            pal().mid,
            false,
            true,
        );
        ui.add_space(14.0);

        let tracks: &[Track] = match &peek.state {
            LoadState::Ready(list) => &list.tracks,
            _ => &[],
        };
        if tracks.is_empty() {
            let (badge_text, badge_color) =
                widgets::badge_view(&peek.state, self.frame_u64(), self.lang);
            let hint = if peek.state.is_loading() {
                self.t("正在取曲子…", "FETCHING TRACKS…").to_string()
            } else if peek.state.is_failed() {
                peek.state.status_hint(self.lang)
            } else {
                self.t(
                    "这张歌单暂时没有可播放的曲子",
                    "This playlist has no playable tracks right now",
                )
                .to_string()
            };
            let (rect, _) = ui.allocate_exact_size(
                vec2(ui.available_width(), ui.available_height().max(60.0)),
                Sense::hover(),
            );
            ui.painter().rect(
                rect,
                radius_none(),
                pal().paper,
                stroke_hairline(),
                StrokeKind::Inside,
            );
            widgets::paint_spec_text(
                ui,
                rect.center() - vec2(0.0, 10.0),
                Align2::CENTER_CENTER,
                &badge_text,
                FONT_SPINE_NAME,
                0.0,
                badge_color,
                false,
                false,
            );
            widgets::paint_spec_text(
                ui,
                rect.center() + vec2(0.0, 12.0),
                Align2::CENTER_CENTER,
                &hint,
                FONT_TINY,
                TRACKING_TINY,
                pal().mid,
                true,
                false,
            );
            return hit;
        }

        let accents = [pal().blue, pal().pink, pal().ink, pal().acid, pal().mid];
        let current_id = self.core.now_playing().map(|n| n.id.clone());
        let mut clicked: Option<usize> = None;
        egui::ScrollArea::vertical()
            .id_salt(panel.scroll_salt())
            .max_height(ui.available_height())
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (index, track) in tracks.iter().enumerate() {
                    let view = SpineView {
                        title: &track.title,
                        artist: &track.artists.join("/"),
                        side: "A",
                        length: &track.duration_text(),
                        accent: accents[index % accents.len()],
                        selected: current_id.as_deref() == Some(track.id.as_str()),
                    };
                    if widgets::spine_item(ui, index, &view).clicked() {
                        clicked = Some(index);
                    }
                    if index + 1 < tracks.len() {
                        ui.add_space(GAP_SPINE);
                    }
                }
            });
        if let Some(index) = clicked {
            hit = match hit {
                // 同一帧既点返回又点曲目属于误触：返回优先
                PeekHit::Back => PeekHit::Back,
                _ => PeekHit::Play(index),
            };
        }
        hit
    }

    fn ui_page_rec(&mut self, ui: &mut Ui) {
        let total = ui.available_width();
        let height = ui.available_height();
        // Rec 为保留页（设计稿无此页），列比沿用 0.56/0.44；R1：显式并排矩形
        let (left_w, right_w) = super::layout::columns(total - GAP_PANEL, 0.56, 0.44);
        let (left_rect, _) = ui.allocate_exact_size(vec2(left_w, height), Sense::hover());
        let right_rect = Rect::from_min_size(
            pos2(left_rect.max.x + GAP_PANEL, left_rect.min.y),
            vec2(right_w, height),
        );

        // ---- 左：推荐歌单（卡片）----
        ui.scope_builder(UiBuilder::new().max_rect(left_rect), |ui| {
            widgets::panel_in(ui, left_rect.height(), |ui| {
                // 卡片点开：在这个面板里原地展开详情，返回后回到原推荐列表
                if self.peek_open(PeekPanel::Rec) {
                    match self.ui_playlist_peek(ui, PeekPanel::Rec) {
                        PeekHit::Back => self.close_peek(PeekPanel::Rec),
                        PeekHit::Play(index) => self.play_peek_track(PeekPanel::Rec, index),
                        PeekHit::None => {}
                    }
                    return;
                }
                let items = self.rec_state.get().cloned().unwrap_or_default();
                widgets::panel_head(
                    ui,
                    self.t("Recommended / 推荐歌单", "Recommended"),
                    &format!("{:02} {}", items.len(), self.t("个歌单", "CASES")),
                );
                let frame = self.frame_u64();
                let (badge_text, badge_color) =
                    widgets::badge_view(&self.rec_state, frame, self.lang);
                ui.horizontal(|ui| {
                    widgets::load_badge(ui, &badge_text, badge_color);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::text_button(
                            ui,
                            ui.scope_id().with("btn-rec-reload"),
                            self.t("重载", "RELOAD"),
                            false,
                            false,
                        )
                        .clicked()
                        {
                            self.rec_state.reset();
                            self.ensure_featured();
                        }
                    });
                });
                ui.add_space(8.0);

                if items.is_empty() {
                    let (rect, _) = ui.allocate_exact_size(
                        vec2(ui.available_width(), widgets::empty_height(ui, 120.0, 0.0)),
                        Sense::hover(),
                    );
                    ui.painter().rect(
                        rect,
                        radius_none(),
                        pal().paper,
                        stroke_hairline(),
                        StrokeKind::Inside,
                    );
                    widgets::paint_spec_text(
                        ui,
                        rect.center(),
                        Align2::CENTER_CENTER,
                        if self.rec_state.is_loading() {
                            self.t("— 载入中 —", "— LOADING —")
                        } else if self.rec_state.is_failed() {
                            self.t("— 载入失败，点重载重试 —", "— LOAD FAILED, HIT RELOAD —")
                        } else {
                            self.t("— 暂无推荐 —", "— NO RECOMMENDATIONS —")
                        },
                        FONT_SPINE_NAME,
                        0.0,
                        pal().mid,
                        false,
                        false,
                    );
                    return;
                }

                let mut opened: Option<PlaylistSummary> = None;
                let accents = [pal().blue, pal().pink, pal().acid, pal().mid];
                egui::ScrollArea::vertical()
                    .id_salt("rec-cards")
                    .max_height(ui.available_height())
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for (i, item) in items.iter().enumerate() {
                            let mut sub_parts: Vec<String> = Vec::new();
                            if let Some(n) = item.track_count {
                                sub_parts.push(format!("{:02} {}", n, self.t("盘", "TAPES")));
                            }
                            if let Some(n) = item.play_count {
                                sub_parts.push(format!(
                                    "{} {}",
                                    Self::fmt_play_count(n),
                                    self.t("次播放", "PLAYS")
                                ));
                            }
                            if let Some(d) = &item.description {
                                sub_parts.push(d.clone());
                            }
                            let sub = if sub_parts.is_empty() {
                                "—".to_string()
                            } else {
                                sub_parts.join(" · ")
                            };
                            // 真实网易云封面 → 点阵 halftone 采样；未就绪时回退程序化封面
                            let cover = self.cover_texture_url(
                                ui.ctx(),
                                &format!("rec-{}", item.id),
                                item.cover_url.as_deref(),
                            );
                            if widgets::playlist_card(
                                ui,
                                ui.scope_id().with(("rec-card", item.id.clone())),
                                accents[i % accents.len()],
                                &item.name,
                                &sub,
                                cover,
                                false,
                                false,
                                false,
                            )
                            .clicked()
                            {
                                opened = Some(item.clone());
                            }
                            ui.add_space(GAP_SPINE);
                        }
                    });
                if let Some(item) = opened {
                    self.open_playlist_peek(PeekPanel::Rec, &item);
                }
            });
        });

        // ---- 右：官方排行榜（行）----
        ui.scope_builder(UiBuilder::new().max_rect(right_rect), |ui| {
            widgets::panel_in(ui, right_rect.height(), |ui| {
                // 榜单行点开：与推荐卡同一套原地展开 + 返回交互
                if self.peek_open(PeekPanel::Chart) {
                    match self.ui_playlist_peek(ui, PeekPanel::Chart) {
                        PeekHit::Back => self.close_peek(PeekPanel::Chart),
                        PeekHit::Play(index) => self.play_peek_track(PeekPanel::Chart, index),
                        PeekHit::None => {}
                    }
                    return;
                }
                let items = self.chart_state.get().cloned().unwrap_or_default();
                widgets::panel_head(
                    ui,
                    self.t("Toplists / 官方排行榜", "Toplists"),
                    &format!("{:02} {}", items.len(), self.t("个榜单", "CHARTS")),
                );
                let frame = self.frame_u64();
                let (badge_text, badge_color) =
                    widgets::badge_view(&self.chart_state, frame, self.lang);
                ui.horizontal(|ui| {
                    widgets::load_badge(ui, &badge_text, badge_color);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::text_button(
                            ui,
                            ui.scope_id().with("btn-chart-reload"),
                            self.t("重载", "RELOAD"),
                            false,
                            false,
                        )
                        .clicked()
                        {
                            self.chart_state.reset();
                            self.ensure_featured();
                        }
                    });
                });
                ui.add_space(8.0);

                // 表头（No / Chart / Update / Heat / Tapes）
                let head = ui
                    .allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover())
                    .0;
                ui.painter()
                    .line_segment([head.left_bottom(), head.right_bottom()], stroke_hairline());
                // 表头列位与 hot_row 同源（widgets::hot_columns），否则会漂到相邻列上
                let cols = widgets::hot_columns(head.width());
                for (label, x, anchor) in [
                    (
                        self.t("序", "No"),
                        head.min.x + cols.rank_x,
                        Align2::LEFT_CENTER,
                    ),
                    (
                        self.t("榜单", "Chart"),
                        head.min.x + cols.title_x,
                        Align2::LEFT_CENTER,
                    ),
                    (
                        self.t("更新", "Update"),
                        head.min.x + cols.artist_x,
                        Align2::LEFT_CENTER,
                    ),
                    (
                        self.t("磁盘", "Tapes"),
                        head.min.x + cols.plays_x,
                        Align2::LEFT_CENTER,
                    ),
                ] {
                    widgets::paint_spec_text(
                        ui,
                        pos2(x, head.center().y),
                        anchor,
                        label,
                        8.0,
                        0.16,
                        pal().ink,
                        true,
                        true,
                    );
                }
                ui.add_space(4.0);

                if items.is_empty() {
                    let (rect, _) = ui.allocate_exact_size(
                        vec2(ui.available_width(), widgets::empty_height(ui, 120.0, 0.0)),
                        Sense::hover(),
                    );
                    ui.painter().rect(
                        rect,
                        radius_none(),
                        pal().paper,
                        stroke_hairline(),
                        StrokeKind::Inside,
                    );
                    widgets::paint_spec_text(
                        ui,
                        rect.center(),
                        Align2::CENTER_CENTER,
                        if self.chart_state.is_loading() {
                            self.t("— 载入中 —", "— LOADING —")
                        } else if self.chart_state.is_failed() {
                            self.t("— 载入失败，点重载重试 —", "— LOAD FAILED, HIT RELOAD —")
                        } else {
                            self.t("— 暂无榜单 —", "— NO CHARTS —")
                        },
                        FONT_SPINE_NAME,
                        0.0,
                        pal().mid,
                        false,
                        false,
                    );
                    return;
                }

                // 热度条按真实播放量归一（无播放量则 0，不伪造）
                let max_plays = items
                    .iter()
                    .filter_map(|i| i.play_count)
                    .max()
                    .unwrap_or(1)
                    .max(1) as f32;
                let mut opened: Option<PlaylistSummary> = None;
                let mut play_now: Option<String> = None;
                egui::ScrollArea::vertical()
                    .id_salt("chart-rows")
                    .max_height(ui.available_height())
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for (i, item) in items.iter().enumerate() {
                            let plays = Self::fmt_track_count(item.track_count);
                            let view = HotRow {
                                rank: i + 1,
                                rise: 0,
                                title: &item.name,
                                artist: item.description.as_deref().unwrap_or("—"),
                                heat: item.play_count.map(|n| n as f32 / max_plays).unwrap_or(0.0),
                                plays: &plays,
                                current: false,
                                selected: self.chart_selected == Some(i),
                                loading: self.playlist_state.is_loading()
                                    && self.playlist_input == item.id,
                            };
                            match widgets::hot_row(ui, ui.scope_id().with(("chart-row", i)), &view)
                            {
                                RowAction::None
                                | RowAction::ContextMenu
                                | RowAction::ToggleFavorite => {}
                                RowAction::Select => {
                                    self.chart_selected = Some(i);
                                    opened = Some(item.clone());
                                }
                                RowAction::Play | RowAction::PlayNow => {
                                    self.chart_selected = Some(i);
                                    play_now = Some(item.id.clone());
                                }
                            }
                        }
                    });
                if let Some(id) = play_now {
                    self.play_playlist_id(&id);
                } else if let Some(item) = opened {
                    self.open_playlist_peek(PeekPanel::Chart, &item);
                }
            });
        });
    }

    // ------------------------------------------------------------------
    // Daily 页（每日推荐歌曲，需登录）
    // ------------------------------------------------------------------

    /// 每日推荐：**当天推荐的歌曲本身**，进入即整列表展开。
    ///
    /// 这里刻意没有任何"卡片 → 点开 → 看到曲目"的层级：用户要的是
    /// 「打开这一页就能看到今天给我推了哪些歌」。歌单卡片那套是
    /// `/recommend/resource`（每日**歌单**）的形态，与需求不是一回事。
    fn ui_page_daily(&mut self, ui: &mut Ui) {
        let total = ui.available_width();
        let height = ui.available_height();
        // 左栏是主体（整页就是一张当天推荐的曲目表），右栏只放会话与说明
        let (left_w, right_w) = super::layout::columns(total - GAP_PANEL, 1.5, 1.0);
        let (left_rect, _) = ui.allocate_exact_size(vec2(left_w, height), Sense::hover());
        let right_rect = Rect::from_min_size(
            pos2(left_rect.max.x + GAP_PANEL, left_rect.min.y),
            vec2(right_w, height),
        );

        // ---- 左：当天推荐的全部曲目 ----
        widgets::panel_at(ui, left_rect, |ui| {
            let tracks = self.daily_state.get().cloned().unwrap_or_default();
            let day = super::daily_clock::today();
            widgets::panel_head(
                ui,
                self.t("每日推荐 / Daily Recommend", "Daily Recommend"),
                &format!("{:02} {}", tracks.len(), self.t("首", "TUNES")),
            );
            let frame = self.frame_u64();
            let (badge_text, badge_color) =
                widgets::badge_view(&self.daily_state, frame, self.lang);
            ui.horizontal(|ui| {
                widgets::load_badge(ui, &badge_text, badge_color);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::text_button(
                        ui,
                        ui.scope_id().with("btn-daily-reload"),
                        self.t("重载", "RELOAD"),
                        true,
                        false,
                    )
                    .clicked()
                    {
                        self.refresh_daily();
                    }
                });
            });
            // 日期行：既回答"这是哪一天的推荐"，也是跨天自动刷新的可见证据
            widgets::paint_spec_text(
                ui,
                ui.cursor().min,
                Align2::LEFT_TOP,
                self.t(
                    &format!("{day} · 服务端按账号每天生成"),
                    &format!("{day} · GENERATED DAILY FOR THIS ACCOUNT"),
                ),
                FONT_TINY,
                TRACKING_TINY,
                pal().mid,
                false,
                true,
            );
            ui.add_space(20.0);

            if tracks.is_empty() {
                // 加载中不插兜底内容：否则会先闪一段离线列表、再被在线结果顶掉
                if !self.daily_state.is_loading()
                    && self.daily_offline
                    && !self.daily_local.is_empty()
                {
                    widgets::paint_spec_text(
                        ui,
                        ui.cursor().min,
                        Align2::LEFT_TOP,
                        self.t(
                            "离线兜底 · 来自本地曲库（同一天结果固定，非在线推荐）",
                            "OFFLINE FALLBACK · LOCAL LIBRARY (FIXED PER DAY, NOT ONLINE)",
                        ),
                        FONT_TINY,
                        TRACKING_TINY,
                        pal().acid,
                        false,
                        true,
                    );
                    ui.add_space(14.0);
                    let h = ui.available_height();
                    self.ui_offline_daily(ui, h);
                    return;
                }
                // 加载中 / 需要登录 / 加载失败 / 当天确实没有：四种原因分开说
                let hint = self.daily_state.status_hint(self.lang);
                self.ui_empty_state(ui, &hint, 120.0);
                return;
            }

            // 表头（与 FM / 检索结果同一套语言，风格统一）
            let head = ui
                .allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover())
                .0;
            ui.painter()
                .line_segment([head.left_bottom(), head.right_bottom()], stroke_hairline());
            for (label, x) in [
                (self.t("序", "No"), head.min.x + 8.0),
                (self.t("曲名", "Title"), head.min.x + 37.0),
                (
                    self.t("艺人", "Artist"),
                    head.min.x + 37.0 + (head.width() - 136.0) / 2.5 + 9.0,
                ),
                (self.t("时长", "Time"), head.max.x - 84.0),
            ] {
                widgets::paint_spec_text(
                    ui,
                    pos2(x, head.center().y),
                    Align2::LEFT_CENTER,
                    label,
                    8.0,
                    0.16,
                    pal().ink,
                    true,
                    true,
                );
            }
            ui.add_space(4.0);

            let current_id = self.core.now_playing().map(|n| n.id.clone());
            let mut action: Option<(RowAction, usize)> = None;
            egui::ScrollArea::vertical()
                .id_salt("daily-rows")
                .max_height(ui.available_height())
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for (index, track) in tracks.iter().enumerate() {
                        let view = ResRow {
                            index,
                            title: &track.title,
                            artist: &track.artists.join(" / "),
                            time: &track.duration_text(),
                            current: current_id.as_deref() == Some(track.id.as_str()),
                            selected: self.selected == Some(index),
                            favorite: self.favorites.contains(track),
                            highlight: &self.highlight_query,
                        };
                        match widgets::res_row(ui, ui.scope_id().with(("daily-row", index)), &view)
                        {
                            RowAction::None => {}
                            act => action = Some((act, index)),
                        }
                    }
                });

            let Some((act, index)) = action else { return };
            let Some(track) = tracks.get(index).cloned() else {
                return;
            };
            match act {
                RowAction::ContextMenu => {
                    // 菜单要带"这一行所在的列表"快照，`PLAY` 才能与双击同语义
                    let Some(ctx) = self.ctx.clone() else { return };
                    let Some(pos) = ctx.input(|i| i.pointer.interact_pos()) else {
                        return;
                    };
                    self.open_track_menu_common(track, None, Some(tracks.clone()), pos);
                }
                RowAction::ToggleFavorite => self.toggle_like(&track),
                RowAction::Select => {
                    // 单击只选中（与其它列表一致）；播放走双击 / ▶ / 右键 PLAY
                    self.selected = Some(index);
                }
                _ => {
                    // 以"今天这 30 首"为播放队列：下一首仍留在同一批里
                    self.play_daily_index(index);
                }
            }
        });

        // ---- 右：会话 + 说明 ----
        ui.scope_builder(UiBuilder::new().max_rect(right_rect), |ui| {
            self.ui_session_panel(ui);
            ui.add_space(GAP_PANEL);
            widgets::panel_frame().show(ui, |ui| {
                widgets::panel_head(
                    ui,
                    self.t("每日说明", "About Daily"),
                    self.t("每日", "DAILY"),
                );
                for line in [
                    self.t(
                        "每日推荐是服务端按账号口味、按天生成的歌曲列表，不是本地随机。",
                        "Daily recommendations are generated server-side per account and per day — not locally randomised.",
                    ),
                    self.t(
                        "同一天内重载拿到的仍是同一批；本地日期一变就自动换成当天数据。",
                        "Reloading on the same day returns the same batch; a new local date triggers a fresh fetch.",
                    ),
                    self.t(
                        "未登录时接口返回 301，这里会引导登录，绝不伪造推荐内容。",
                        "Without a session the API answers 301; we guide you to sign in instead of faking results.",
                    ),
                    self.t(
                        "拉取失败时用本地曲库按当天种子兜底，并明确标注为离线内容。",
                        "If the request fails, a local-library fallback runs and is clearly labelled offline.",
                    ),
                ] {
                    ui.label(widgets::wrap_to(
                        widgets::spec_job(line, 10.0, 0.01, pal().ink, false),
                        ui.available_width(),
                        3,
                    ));
                    ui.add_space(6.0);
                }
            });
        });
    }

    /// 以「当天推荐批次」为队列播放第 `index` 首。
    ///
    /// 抽成独立方法而不是写在渲染闭包里：队列语义（"列表即队列"）必须能单测，
    /// 而闭包里的代码测不到。播完这批就停，不会自动向服务端续批。
    fn play_daily_index(&mut self, index: usize) {
        let Some(tracks) = self.daily_state.get().cloned() else {
            return;
        };
        let Some(track) = tracks.get(index).cloned() else {
            return;
        };
        self.clear_user_view();
        self.active_local = false;
        self.active_playlist = None;
        self.selected = Some(index);
        self.replace_queue(tracks);
        self.play_track(track);
    }

    /// 会话面板（Daily 页右栏）：展示登录态并提供扫码入口。
    ///
    /// 这是登录的**第二个入口**（第一个是标题栏右侧 chip）：当用户因为
    /// "需要登录"被引导到这里时，页面上要有可以直接点的东西，而不是让他
    /// 自己去找标题栏那个小按钮。
    fn ui_session_panel(&mut self, ui: &mut Ui) {
        widgets::panel_frame().show(ui, |ui| {
            widgets::panel_head(
                ui,
                self.t("Session / 会话", "Session"),
                self.t("账户", "ACCOUNT"),
            );
            match self.login.logged_in(self.lang).map(str::to_string) {
                Some(name) => {
                    ui.label(widgets::spec_job(
                        self.t(&format!("已登录 · {name}"), &format!("SIGNED IN · {name}")),
                        FONT_SPINE_NAME,
                        0.0,
                        pal().ink,
                        false,
                    ));
                    ui.add_space(6.0);
                    ui.label(widgets::wrap_to(
                        widgets::spec_job(
                            self.t(
                                "登录态只存在内存里，关闭程序即失效。",
                                "The session lives in memory only and ends when the app closes.",
                            ),
                            9.0,
                            0.01,
                            pal().mid,
                            false,
                        ),
                        ui.available_width(),
                        2,
                    ));
                }
                None => {
                    ui.label(widgets::spec_job(
                        self.t(
                            "未登录 · 部分功能不可用",
                            "SIGNED OUT · some features unavailable",
                        ),
                        FONT_SPINE_NAME,
                        0.0,
                        pal().ink,
                        false,
                    ));
                    ui.add_space(8.0);
                    if widgets::text_button(
                        ui,
                        ui.scope_id().with("session-login"),
                        self.t("扫码登录", "SCAN LOGIN"),
                        true,
                        false,
                    )
                    .clicked()
                    {
                        self.open_login();
                    }
                }
            }
        });
    }

    /// 列表空态（加载中 / 失败 / 需登录 / 真的空）。
    ///
    /// `hint` 由 [`LoadState::status_hint`] 给出——把"为什么空"讲清楚，
    /// 而不是统一显示 "NO DATA"。
    fn ui_empty_state(&mut self, ui: &mut Ui, hint: &str, min_h: f32) {
        let (rect, _) = ui.allocate_exact_size(
            vec2(ui.available_width(), widgets::empty_height(ui, min_h, 0.0)),
            Sense::hover(),
        );
        ui.painter().rect(
            rect,
            radius_none(),
            pal().paper,
            stroke_hairline(),
            StrokeKind::Inside,
        );
        widgets::paint_spec_text(
            ui,
            rect.center(),
            Align2::CENTER_CENTER,
            hint,
            FONT_SPINE_NAME,
            0.0,
            pal().mid,
            false,
            false,
        );
    }

    /// 歌单页右栏的**封面区**：显示所选歌单「首个曲目」的封面。
    ///
    /// 封面跟着 `current_tracks()[0]` 走，所以「切换歌单 → 封面同步」是免费的：
    /// 歌单换了，首曲变了，纹理 key 跟着变。没有首曲（空歌单 / 未装载）时
    /// 只画一个空位，不伪造封面。
    fn ui_playlist_cover(&mut self, ui: &mut Ui) {
        /// 封面边长。
        const SIDE: f32 = 72.0;
        /// 封面与文字之间的间隙。
        const GAP: f32 = 12.0;

        let row_h = SIDE + 8.0;
        let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::hover());
        let cover_rect = Rect::from_min_size(row.min + vec2(0.0, 4.0), vec2(SIDE, SIDE));

        let first = self.current_tracks().first().cloned();
        let tex = match &first {
            Some(t) => self.cover_texture_url(
                ui.ctx(),
                &format!("pl-cover-{}", t.id),
                t.cover_url.as_deref(),
            ),
            None => None,
        };
        let painter = ui.painter();
        match tex {
            Some(texture) => painter.image(
                texture,
                cover_rect,
                // cover 语义：等比缩放 + 居中裁切，绝不拉伸
                widgets::cover_uv(Vec2::splat(1.0), cover_rect.size()),
                Color32::WHITE,
            ),
            None => painter.rect_filled(cover_rect, radius_none(), pal().mid),
        };
        painter.rect_stroke(
            cover_rect,
            radius_none(),
            stroke_hairline(),
            StrokeKind::Inside,
        );

        let text_x = cover_rect.max.x + GAP;
        let max_w = (row.max.x - text_x).max(40.0);
        widgets::paint_spec_text(
            ui,
            pos2(text_x, cover_rect.min.y + 8.0),
            Align2::LEFT_TOP,
            self.t("首个曲目封面", "FIRST TRACK COVER"),
            FONT_TINY,
            TRACKING_TINY,
            pal().mid,
            true,
            false,
        );
        match &first {
            Some(t) => {
                let title = widgets::fit_text(
                    ui,
                    &t.title,
                    FONT_SPINE_NAME,
                    0.0,
                    pal().ink,
                    false,
                    false,
                    max_w,
                );
                widgets::paint_spec_text(
                    ui,
                    pos2(text_x, cover_rect.min.y + 26.0),
                    Align2::LEFT_TOP,
                    &title,
                    FONT_SPINE_NAME,
                    0.0,
                    pal().ink,
                    false,
                    false,
                );
                let artist = widgets::fit_text(
                    ui,
                    &t.artists.join(" / "),
                    FONT_TINY,
                    TRACKING_TINY,
                    pal().mid,
                    false,
                    false,
                    max_w,
                );
                widgets::paint_spec_text(
                    ui,
                    pos2(text_x, cover_rect.min.y + 48.0),
                    Align2::LEFT_TOP,
                    &artist,
                    FONT_TINY,
                    TRACKING_TINY,
                    pal().mid,
                    false,
                    false,
                );
            }
            None => {
                widgets::paint_spec_text(
                    ui,
                    pos2(text_x, cover_rect.min.y + 28.0),
                    Align2::LEFT_TOP,
                    self.t("这盒磁带是空的", "THIS CASE IS EMPTY"),
                    FONT_TINY,
                    TRACKING_TINY,
                    pal().mid,
                    true,
                    false,
                );
            }
        }
        ui.add_space(8.0);
    }

    /// 用户数据视图的编辑工具条（收藏 / 最近播放 / 自建歌单）。
    ///
    /// 只在选中了某个用户视图时出现：其它来源（在线歌单/本地曲库）没有这些动作，
    /// 常驻一排禁用按钮只会让人以为是坏的。
    fn ui_user_view_toolbar(&mut self, ui: &mut Ui) {
        match self.user_view {
            // 专辑/艺人页没有编辑动作（只读浏览），与「无视图」同样不出工具条
            UserView::None | UserView::Album | UserView::Artist => {}
            UserView::Favorites => {
                widgets::paint_spec_text(
                    ui,
                    ui.cursor().min,
                    Align2::LEFT_TOP,
                    self.t(
                        "— 点行内 ♥ 可取消收藏 —",
                        "— CLICK ♥ IN A ROW TO UNFAVORITE —",
                    ),
                    FONT_TINY,
                    TRACKING_TINY,
                    pal().mid,
                    false,
                    true,
                );
                ui.add_space(16.0);
            }
            UserView::History => {
                ui.horizontal(|ui| {
                    widgets::paint_spec_text(
                        ui,
                        pos2(ui.cursor().min.x, ui.cursor().min.y + 8.0),
                        Align2::LEFT_TOP,
                        self.t("时间窗", "WINDOW"),
                        FONT_TINY,
                        TRACKING_TINY,
                        pal().mid,
                        false,
                        true,
                    );
                    ui.add_space(48.0);
                    for (hours, zh, en) in [
                        (24i64, "24 小时", "24H"),
                        (168, "7 天", "7D"),
                        (720, "30 天", "30D"),
                    ] {
                        if widgets::text_button(
                            ui,
                            ui.scope_id().with(("btn-hist-win", hours)),
                            self.t(zh, en),
                            self.history_hours == hours,
                            false,
                        )
                        .clicked()
                        {
                            self.set_history_window(hours);
                        }
                        ui.add_space(4.0);
                    }
                });
                ui.add_space(8.0);
            }
            UserView::LocalPlaylist(id) => {
                // 重命名态：输入框 + 确定/取消
                if self.rename_target == Some(id) {
                    ui.horizontal(|ui| {
                        let hint = self.t("新名称", "New name");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.rename_input)
                                .hint_text(hint)
                                .desired_width(ui.available_width() - 130.0),
                        );
                        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if widgets::text_button(
                            ui,
                            ui.scope_id().with("btn-rename-ok"),
                            self.t("确定", "OK"),
                            true,
                            false,
                        )
                        .clicked()
                            || enter
                        {
                            self.commit_rename_playlist();
                        }
                        if widgets::text_button(
                            ui,
                            ui.scope_id().with("btn-rename-cancel"),
                            self.t("取消", "CANCEL"),
                            false,
                            false,
                        )
                        .clicked()
                        {
                            self.rename_target = None;
                            self.rename_input.clear();
                        }
                    });
                    ui.add_space(8.0);
                    return;
                }

                // 加入当前播放曲目
                let current = self.current_track.clone();
                ui.horizontal(|ui| {
                    let can_add = current.is_some();
                    let label = self.t("＋ 加入当前曲目", "＋ ADD CURRENT");
                    if widgets::text_button(
                        ui,
                        ui.scope_id().with("btn-pl-add"),
                        label,
                        false,
                        false,
                    )
                    .clicked()
                    {
                        match &current {
                            Some(track) => self.add_to_local_playlist(id, track),
                            None => self.push_toast(
                                self.t("当前没有播放中的曲目", "Nothing is playing right now"),
                            ),
                        }
                    }
                    let _ = can_add;
                    ui.add_space(6.0);
                    if widgets::text_button(
                        ui,
                        ui.scope_id().with("btn-pl-rename"),
                        self.t("重命名", "RENAME"),
                        false,
                        false,
                    )
                    .clicked()
                    {
                        self.rename_target = Some(id);
                        self.rename_input = self
                            .local_playlists
                            .get(id)
                            .map(|p| p.name.clone())
                            .unwrap_or_default();
                    }
                    ui.add_space(6.0);
                    if widgets::text_button(
                        ui,
                        ui.scope_id().with("btn-pl-delete"),
                        self.t("删除", "DELETE"),
                        false,
                        false,
                    )
                    .clicked()
                    {
                        self.request_delete_local_playlist(id);
                    }
                });
                ui.add_space(6.0);
                // 排序与移除（作用于当前选中行）
                let has_sel = self.selected.is_some() && !self.current_tracks().is_empty();
                ui.horizontal(|ui| {
                    if widgets::text_button(
                        ui,
                        ui.scope_id().with("btn-pl-up"),
                        self.t("↑ 上移", "↑ UP"),
                        false,
                        false,
                    )
                    .clicked()
                        && has_sel
                    {
                        self.move_selected_in_playlist(-1);
                    }
                    ui.add_space(6.0);
                    if widgets::text_button(
                        ui,
                        ui.scope_id().with("btn-pl-down"),
                        self.t("↓ 下移", "↓ DOWN"),
                        false,
                        false,
                    )
                    .clicked()
                        && has_sel
                    {
                        self.move_selected_in_playlist(1);
                    }
                    ui.add_space(6.0);
                    if widgets::text_button(
                        ui,
                        ui.scope_id().with("btn-pl-remove"),
                        self.t("移除选中", "REMOVE"),
                        false,
                        false,
                    )
                    .clicked()
                        && has_sel
                    {
                        self.remove_selected_from_playlist();
                    }
                });
                ui.add_space(8.0);
            }
        }
    }

    /// 本地曲库工具条：状态文字靠左，扫描按钮贴面板右边缘。
    ///
    /// 不再有 "LOCAL LIBRARY" 标签——它把按钮挤到中间，既浪费横向空间，
    /// 又让这一行和上面的 PLAY ALL 对不齐。按钮贴右后，左端留给状态文字，
    /// 长路径名可以吃掉整段空位（`fit_text` 截断）。
    fn ui_library_toolbar(&mut self, ui: &mut Ui) {
        // 34（按钮高）+ 2（硬投影）
        const ROW_H: f32 = 36.0;
        const BTN_H: f32 = 34.0;

        let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::hover());
        let scan_label = self.t("扫描文件夹", "SCAN FOLDER");
        let btn_w = widgets::text_button_width(ui, scan_label);
        // 右缘再让 2px：按钮的硬投影向右下偏，贴着面板边会把投影挤出面板
        let btn_rect =
            Rect::from_min_size(pos2(row.max.x - btn_w - 2.0, row.min.y), vec2(btn_w, BTN_H));

        let mut clicked = false;
        ui.scope_builder(UiBuilder::new().max_rect(btn_rect), |ui| {
            clicked = widgets::text_button(
                ui,
                ui.scope_id().with("btn-scan-library"),
                scan_label,
                true,
                false,
            )
            .clicked();
        });

        let status = match self.library.as_ref() {
            Some(lib) => format!("{} · {:02} TAPES", lib.display_name(), lib.tracks.len()),
            None => "NOT SCANNED".to_string(),
        };
        // 与按钮留 10px 间隙，避免长路径贴到按钮描边上
        let max_w = (btn_rect.min.x - row.min.x - 10.0).max(24.0);
        let status = widgets::fit_text(
            ui,
            &status,
            FONT_SPINE_NAME,
            0.0,
            pal().ink,
            false,
            true,
            max_w,
        );
        // 文字基线对齐按钮中心（按钮本身 34 高，行高 36 含投影）
        widgets::paint_spec_text(
            ui,
            pos2(row.min.x, btn_rect.center().y),
            Align2::LEFT_CENTER,
            &status,
            FONT_SPINE_NAME,
            0.0,
            pal().ink,
            true,
            false,
        );

        if clicked {
            if let Some(dir) = library::pick_folder(self.lang) {
                self.load_library_dir(&dir);
            }
        }
    }

    /// 歌单页（Tape Cases）：左列只列**账号歌单 + 自建歌单**，右列是所选歌单的
    /// 封面区 + 曲目表。
    fn ui_page_playlist(&mut self, ui: &mut Ui) {
        let total = ui.available_width();
        let height = ui.available_height();
        // 歌单页只统计**两类真实来源**：账号歌单 + 自建歌单。
        // 本地曲库、收藏、最近播放、已载入历史各有自己的入口，不再混进这里。
        let loaded_count = self.user_playlists.len() + self.local_playlists.lists.len();
        let active_loaded_id = self
            .active_playlist
            .and_then(|i| self.playlists.get(i))
            .map(|p| p.id.clone());
        // .pl-layout{grid-template-columns:.86fr 1.14fr}；gap 10（R1：显式并排矩形）
        let (left_w, right_w) = super::layout::columns(total - GAP_PANEL, 0.86, 1.14);
        let (left_rect, _) = ui.allocate_exact_size(vec2(left_w, height), Sense::hover());
        let right_rect = Rect::from_min_size(
            pos2(left_rect.max.x + GAP_PANEL, left_rect.min.y),
            vec2(right_w, height),
        );

        // ---- 左：载入 + 歌单卡 ----
        ui.scope_builder(UiBuilder::new().max_rect(left_rect), |ui| {
            widgets::panel_in(ui, left_rect.height(), |ui| {
                widgets::panel_head(
                    ui,
                    self.t("Tape Cases / 歌单", "Tape Cases"),
                    &format!("{:02} {}", loaded_count, self.t("个歌单", "LISTS")),
                );
                ui.horizontal(|ui| {
                    // 同上：提示文案先算，避免与 &mut self.playlist_input 冲突
                    let hint = self.t(
                        "歌单 ID / 分享链接，如 24381616",
                        "Playlist ID / share link, e.g. 24381616",
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.playlist_input)
                            .hint_text(hint)
                            .desired_width(ui.available_width() - 90.0),
                    );
                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if widgets::text_button(
                        ui,
                        ui.scope_id().with("btn-pl-load"),
                        self.t("装载", "LOAD"),
                        true,
                        false,
                    )
                    .clicked()
                        || enter
                    {
                        self.submit_playlist();
                    }
                });
                ui.add_space(10.0);

                let frame = self.frame_u64();
                let (badge_text, badge_color) =
                    widgets::badge_view(&self.playlist_state, frame, self.lang);
                widgets::load_badge(ui, &badge_text, badge_color);
                ui.label(widgets::spec_job(
                    self.t(
                        "右键卡片：打开 / 删除或移出；× 亦可删除",
                        "Right-click a card: open / delete or remove; × also deletes",
                    ),
                    FONT_TINY,
                    TRACKING_TINY,
                    pal().mid,
                    true,
                ));
                ui.add_space(8.0);

                // 卡片列表放进滚动区：歌单多时不再溢出面板（R1 的纵向版本）。
                // 必须先给下方固定行留高度，否则滚动区会吃掉整块可用空间，
                // 把「新歌单名称 / 新建」和同步行顶到面板之外（被窗口裁掉）。
                let logged_in = self.login.logged_in(self.lang).is_some();
                let fixed_h = 4.0 + 34.0 + if logged_in { 4.0 + 34.0 } else { 0.0 };
                let list_h = (ui.available_height() - fixed_h).max(0.0);
                let out = egui::ScrollArea::vertical()
                    .id_salt("tape-cases")
                    .max_height(list_h)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        let mut pl_menu: Option<u64> = None;
                        let mut account_menu: Option<String> = None;
                        let mut account_pick: Option<String> = None;
                        let mut delete_local: Option<u64> = None;
                        let mut user_inner: Option<UserView> = None;
                        // ---- 自建歌单 ----
                        for pl in self.local_playlists.lists.clone() {
                            let sub = format!(
                                "{} {:02} {}",
                                self.t("自建", "LOCAL"),
                                pl.entries.len(),
                                self.t("盘", "TAPES")
                            );
                            let cover = self.cover_texture(ui.ctx(), &format!("localpl-{}", pl.id));
                            let card = widgets::playlist_card(
                                ui,
                                ui.scope_id().with(("card-localpl", pl.id)),
                                pal().mid,
                                &pl.name,
                                &sub,
                                cover,
                                self.user_view == UserView::LocalPlaylist(pl.id),
                                false,
                                true,
                            );
                            match card {
                                widgets::PlaylistCardAction::Open => {
                                    user_inner = Some(UserView::LocalPlaylist(pl.id));
                                }
                                widgets::PlaylistCardAction::ContextMenu => {
                                    pl_menu = Some(pl.id);
                                }
                                widgets::PlaylistCardAction::Delete => {
                                    delete_local = Some(pl.id);
                                }
                                widgets::PlaylistCardAction::None => {}
                            }
                            ui.add_space(GAP_SPINE);
                        }

                        // ---- 账号歌单（网易云个人歌单，登录后才有）----
                        for up in self.user_playlists.clone().iter() {
                            let sub = format!(
                                "{} · {} {}",
                                self.t("账号", "ACCOUNT"),
                                up.track_count.unwrap_or(0),
                                self.t("盘", "TAPES")
                            );
                            // 走 `cover_texture_url` 才会真的去下载这张封面。
                            // 之前误用了 `cover_texture`（程序化占位生成器）并把 URL
                            // 当种子喂进去——于是每张账号歌单都显示一块由 URL 哈希出来
                            // 的合成图案，永远不是真封面（用户反馈的「封面不准」）。
                            let cover = self.cover_texture_url(
                                ui.ctx(),
                                &format!("accpl-{}", up.id),
                                up.cover_url.as_deref(),
                            );
                            let loading =
                                self.playlist_state.is_loading() && self.playlist_input == up.id;
                            // 载入后高亮这张账号卡本身：它对应的曲目已经在右侧，
                            // 不再另建一张同名「已载入」卡来表达选中。
                            let selected = active_loaded_id.as_deref() == Some(up.id.as_str());
                            match widgets::playlist_card(
                                ui,
                                ui.scope_id().with(("card-account-pl", up.id.clone())),
                                pal().pink,
                                &up.name,
                                &sub,
                                cover,
                                selected,
                                loading,
                                false,
                            ) {
                                widgets::PlaylistCardAction::Open => {
                                    account_pick = Some(up.id.clone());
                                }
                                widgets::PlaylistCardAction::ContextMenu => {
                                    account_menu = Some(up.id.clone());
                                }
                                widgets::PlaylistCardAction::Delete
                                | widgets::PlaylistCardAction::None => {}
                            }
                            ui.add_space(GAP_SPINE);
                        }

                        // 「已载入历史」卡**不再画**：歌单页只呈现两类真实来源
                        // （账号歌单 / 自建歌单）。`self.playlists` 仍在用——它是
                        // 右侧曲目、封面区与播放队列的数据来源，只是不再占一张卡。
                        (
                            user_inner,
                            pl_menu,
                            account_menu,
                            account_pick,
                            delete_local,
                        )
                    });
                let (user_pick, pl_menu, account_menu, account_pick, delete_local) = out.inner;
                // ---- 新建自建歌单 ----
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let hint = self.t("新歌单名称", "New playlist name");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_playlist_name)
                            .hint_text(hint)
                            .desired_width(ui.available_width() - 74.0),
                    );
                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if widgets::text_button(
                        ui,
                        ui.scope_id().with("btn-new-pl"),
                        self.t("新建", "NEW"),
                        false,
                        false,
                    )
                    .clicked()
                        || enter
                    {
                        self.create_local_playlist();
                    }
                });

                if let Some(view) = user_pick {
                    self.select_user_view(view);
                }
                if let Some(id) = pl_menu {
                    self.open_playlist_menu(id);
                }
                if let Some(id) = account_menu {
                    self.open_account_playlist_menu(id);
                }
                if let Some(id) = delete_local {
                    self.request_delete_local_playlist(id);
                }
                if let Some(id) = account_pick {
                    // 账号歌单走同一个"装载歌单"通道：接口与在线歌单完全一致，
                    // 另开一条路只会多一份要维护的加载逻辑
                    self.playlist_input = id;
                    self.submit_playlist();
                }
                // ---- 账号同步入口（登录后才有）----
                if logged_in {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let label = if self.sync.active() {
                            self.t("同步中…", "SYNCING…")
                        } else {
                            self.t("↻ 同步收藏", "↻ SYNC LIKED")
                        };
                        if widgets::text_button(
                            ui,
                            ui.scope_id().with("btn-sync-liked"),
                            label,
                            self.sync.active(),
                            false,
                        )
                        .clicked()
                        {
                            self.start_favorite_sync();
                        }
                        ui.add_space(8.0);
                        let status = format!(
                            "{} {:02}",
                            self.t("账号歌单", "ACCOUNT LISTS"),
                            self.user_playlists.len()
                        );
                        let status = widgets::fit_text(
                            ui,
                            &status,
                            FONT_TINY,
                            TRACKING_TINY,
                            pal().mid,
                            true,
                            false,
                            ui.available_width(),
                        );
                        ui.label(widgets::spec_job(
                            &status,
                            FONT_TINY,
                            TRACKING_TINY,
                            pal().mid,
                            true,
                        ));
                    });
                }
            });
        });

        // ---- 右：曲目 ----
        ui.scope_builder(UiBuilder::new().max_rect(right_rect), |ui| {
            widgets::panel_in(ui, right_rect.height(), |ui| {
                let (name, meta) = if let Some(title) = self.user_view_title() {
                    // 用户数据视图（收藏 / 最近播放 / 自建歌单）：标题与计数直取视图
                    let count = self.current_tracks().len();
                    let extra = match self.user_view {
                        UserView::History => {
                            format!(" · {}", history_window_label(self.history_hours, self.lang))
                        }
                        _ => String::new(),
                    };
                    (
                        title,
                        format!("{:02} {}{}", count, self.t("盘", "TAPES"), extra),
                    )
                } else if self.active_local {
                    match &self.library {
                        Some(lib) => (
                            format!(
                                "{} / {}",
                                lib.display_name(),
                                self.t("本地曲库", "LOCAL LIBRARY")
                            ),
                            format!(
                                "{:02} {} · {}{}",
                                lib.tracks.len(),
                                self.t("盘", "TAPES"),
                                self.t("本地", "LOCAL"),
                                if lib.truncated {
                                    self.t(" · 已截断", " · TRUNCATED")
                                } else {
                                    ""
                                }
                            ),
                        ),
                        None => (
                            self.t("未装载磁带盒", "NO CASE LOADED").to_string(),
                            format!("00 {} · --:--", self.t("盘", "TAPES")),
                        ),
                    }
                } else {
                    match self.active_playlist.and_then(|i| self.playlists.get(i)) {
                        Some(p) => {
                            let total_ms: u64 = p.tracks.iter().filter_map(|t| t.duration_ms).sum();
                            (
                                p.name.clone().unwrap_or_else(|| {
                                    self.t("未命名混音", "UNTITLED MIX").to_string()
                                }),
                                format!(
                                    "{:02} {} · {}",
                                    p.tracks.len(),
                                    self.t("盘", "TAPES"),
                                    format_duration(Duration::from_millis(total_ms))
                                ),
                            )
                        }
                        None => (
                            self.t("未装载磁带盒", "NO CASE LOADED").to_string(),
                            format!("00 {} · --:--", self.t("盘", "TAPES")),
                        ),
                    }
                };
                widgets::panel_head(ui, &name, &meta);
                // 封面区：所选歌单**首个曲目**的封面。切换歌单时这里的曲目变了，
                // 封面自然跟着换——不需要额外的"同步"逻辑。
                self.ui_playlist_cover(ui);

                ui.horizontal(|ui| {
                    if widgets::text_button(
                        ui,
                        ui.scope_id().with("btn-pl-play"),
                        self.t("▶ 播放全部", "▶ PLAY ALL"),
                        true,
                        false,
                    )
                    .clicked()
                    {
                        if self.current_tracks().is_empty() {
                            self.push_toast(self.t("这盒磁带是空的", "THIS CASE IS EMPTY"));
                        } else {
                            self.selected = Some(0);
                            self.play_selected();
                        }
                    }
                    ui.add_space(8.0);
                    if widgets::text_button(
                        ui,
                        ui.scope_id().with("btn-pl-shuffle"),
                        self.t("⇄ 随机播放", "⇄ SHUFFLE"),
                        false,
                        false,
                    )
                    .clicked()
                    {
                        let len = self.current_tracks().len();
                        if len == 0 {
                            self.push_toast(self.t("这盒磁带是空的", "THIS CASE IS EMPTY"));
                        } else {
                            let seed = widgets::hash_id(&format!("shuf{}", self.t as u64));
                            self.selected = Some(seed as usize % len);
                            self.play_selected();
                        }
                    }
                });
                ui.add_space(8.0);
                // 用户数据视图工具条（收藏/最近播放/自建歌单各自的编辑动作）
                self.ui_user_view_toolbar(ui);
                // 本地曲库工具条（扫描文件夹 → 入库成持久化歌单）
                self.ui_library_toolbar(ui);
                ui.add_space(10.0);
                self.ui_result_rows(ui, ui.available_height());
            });
        });
    }

    // ------------------------------------------------------------------
    // 反馈层
    // ------------------------------------------------------------------

    /// 相对 seek（键盘 ←/→）；未知总长时不动。
    fn seek_by(&mut self, delta_secs: f32) {
        let p = self.core.progress();
        let Some(total) = p.total else { return };
        let total_s = total.as_secs_f32();
        if total_s <= 0.0 {
            return;
        }
        let target = (p.position.as_secs_f32() + delta_secs).clamp(0.0, total_s);
        if let Err(reason) = self.core.seek_ratio(target / total_s) {
            self.push_toast(reason.message(self.lang));
        }
    }

    /// 全局快捷键（输入框聚焦时不抢键）。
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if self.confirm.is_some() || ctx.egui_wants_keyboard_input() {
            return;
        }
        use egui::Key;
        let (
            mut play,
            mut prev,
            mut next,
            mut seek_back,
            mut seek_fwd,
            mut vol_up,
            mut vol_down,
            mut mute,
            mut fullscreen,
            mut escape,
            mut focus,
        ) = (
            false, false, false, false, false, false, false, false, false, false, false,
        );
        ctx.input(|i| {
            let cmd = i.modifiers.command;
            if i.key_pressed(Key::Space) {
                play = true;
            }
            if i.key_pressed(Key::ArrowLeft) {
                if cmd {
                    prev = true;
                } else {
                    seek_back = true;
                }
            }
            if i.key_pressed(Key::ArrowRight) {
                if cmd {
                    next = true;
                } else {
                    seek_fwd = true;
                }
            }
            if i.key_pressed(Key::ArrowUp) {
                vol_up = true;
            }
            if i.key_pressed(Key::ArrowDown) {
                vol_down = true;
            }
            if i.key_pressed(Key::M) && !cmd {
                mute = true;
            }
            if i.key_pressed(Key::F11) {
                fullscreen = true;
            }
            if i.key_pressed(Key::Escape) {
                escape = true;
            }
            if (cmd && i.key_pressed(Key::F)) || (!cmd && i.key_pressed(Key::Slash)) {
                focus = true;
            }
        });

        if focus {
            self.set_page(Page::Search);
            self.focus_search = true;
        }
        if fullscreen || (escape && self.fullscreen) {
            self.fullscreen = !self.fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
            self.push_toast(if self.fullscreen {
                self.t(
                    "全屏：开（F11 / Esc 退出）",
                    "FULLSCREEN ON (F11 / Esc to exit)",
                )
            } else {
                self.t("全屏：关", "FULLSCREEN OFF")
            });
        }
        if play {
            if self.core.now_playing().is_none() {
                self.play_selected();
            } else if let Err(reason) = self.core.toggle() {
                self.push_toast(reason.message(self.lang));
            }
        }
        if prev {
            self.advance_prev();
        }
        if next {
            self.advance_next();
        }
        if seek_back {
            self.seek_by(-SEEK_STEP_SECS);
        }
        if seek_fwd {
            self.seek_by(SEEK_STEP_SECS);
        }
        if vol_up {
            self.adjust_volume(VOLUME_STEP);
        }
        if vol_down {
            self.adjust_volume(-VOLUME_STEP);
        }
        if mute {
            self.toggle_mute();
        }
    }

    fn ui_overlays(&mut self, ctx: &egui::Context) {
        // 二次确认是最高优先级的应用内模态：开着时不再渲染/响应右键菜单
        if let Some(dialog) = self.confirm.clone() {
            self.menu = None;
            let cancel = self.t("取消", "CANCEL");
            match widgets::confirm_modal(
                ctx,
                &dialog.title,
                &dialog.message,
                &dialog.confirm_label,
                cancel,
            ) {
                Some(true) => {
                    self.confirm = None;
                    self.apply_confirm(dialog.action);
                }
                Some(false) => self.confirm = None,
                None => {}
            }
        } else if let Some(action) = super::context_menu::show(ctx, &mut self.menu) {
            // 右键菜单：画在登录面板/错误框**下面**——模态在的时候不该还能点菜单
            self.apply_menu_action(action);
        }
        // 仅调试构建：探针可强制常驻一条 toast（校对宽度/居中）
        #[cfg(debug_assertions)]
        if let Some(msg) = super::probe::forced_toast() {
            widgets::toast(ctx, &msg);
        }
        if let Some((message, expires)) = &self.toast {
            if self.t < *expires {
                widgets::toast(ctx, message);
            }
        }
        if let Some((title, message)) = self.error_modal.clone() {
            if widgets::error_modal(ctx, &title, &message, self.t("确定", "OK")) {
                self.error_modal = None;
            }
        }
        if self.fullscreen {
            let hint = self.t("F11 / ESC 退出全屏", "F11 / ESC TO EXIT FULLSCREEN");
            egui::Area::new(egui::Id::unique("reimu-player-fullscreen-hint"))
                .order(egui::Order::Foreground)
                .anchor(Align2::RIGHT_BOTTOM, vec2(-14.0, -10.0))
                .show(ctx, |ui| {
                    egui::Frame::NONE
                        .fill(with_opacity(pal().paper, 0.92))
                        .stroke(stroke_component())
                        .inner_margin(Margin::symmetric(10, 5))
                        .show(ui, |ui| {
                            ui.label(widgets::spec_job(
                                hint,
                                FONT_TINY,
                                TRACKING_TINY,
                                pal().mid,
                                true,
                            ));
                        });
                });
        }
        // 登录面板压在最上层（模态）：错误框关掉后才能操作它
        if self.error_modal.is_none() {
            let qr_tex = self.qr_texture.as_ref().map(|t| t.id());
            let action = login::login_panel(ctx, &mut self.login, qr_tex, self.lang);
            self.on_login_action(action);
        }
    }

    fn frame_u64(&self) -> u64 {
        (self.t * 60.0) as u64
    }

    /// 播放量紧凑显示（12.3K / 4.5M / 原样）。
    fn fmt_play_count(n: u64) -> String {
        match n {
            n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1_000_000.0),
            n if n >= 1_000 => format!("{:.1}K", n as f64 / 1_000.0),
            n => n.to_string(),
        }
    }

    /// 曲目数显示（缺失不伪造 → "—"）。
    fn fmt_track_count(n: Option<u64>) -> String {
        n.map(|v| v.to_string()).unwrap_or_else(|| "—".to_string())
    }
}

impl eframe::App for ReimuApp {
    /// 全透明清屏。
    ///
    /// eframe 用**同一个** `clear_color` 清每个视口的 framebuffer，而桌面歌词是
    /// 透明无边框窗口。默认值 `rgba(12,12,12,180)` 会把它清成一块**深色直角
    /// 矩形**——外面直角、里面圆角底板，正是"直角外窗内嵌圆角窗口"的成因。
    /// 改成 0 之后圆角底板四角透出桌面，视觉上就是一个圆角窗口。
    ///
    /// 主窗口的底色不受影响：它在 [`Self::ui`] 第一行自己铺一张不透明纸底。
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 纸底：`clear_color` 已改为全透明（为了歌词窗四角真透明），主窗口
        // 必须自己铺满整块视口，否则未绘制区域会漏出桌面/黑底。
        // 放在最前面画 = 压在所有内容之下（egui 按绘制调用顺序叠加）。
        ui.painter()
            .rect_filled(ui.max_rect(), egui::CornerRadius::ZERO, pal().paper);

        let dt = ui.input(|i| i.stable_dt).min(0.1);
        perf_log::sample(dt);
        self.t += dt;
        self.cover_clock = self.cover_clock.wrapping_add(1);

        // 原始输入取证（trace 级，默认关闭零成本）。排查"控件在没人操作时自己
        // 触发"这类问题，先看这里有没有真实事件进来：egui 的 `clicked()` 只能由
        // press→release 对、AccessKit Click 动作、或已聚焦 + Space/Enter 产生
        // （egui 0.37 `context.rs::get_response`）。`RUST_LOG=reimuplayer=trace`
        // 即可对照 `settings action hit` 的时间线。
        let t_ms = (self.t * 1000.0) as u64;
        ui.input(|i| {
            for e in &i.raw.events {
                match e {
                    egui::Event::PointerButton {
                        pos,
                        button,
                        pressed,
                        ..
                    } => tracing::trace!(?pos, ?button, pressed, t_ms, "raw input: pointer button"),
                    egui::Event::Key {
                        key,
                        pressed,
                        repeat,
                        ..
                    } => tracing::trace!(?key, pressed, repeat, t_ms, "raw input: key"),
                    egui::Event::AccessKitActionRequest(req) => {
                        tracing::trace!(action = ?req.action, t_ms, "raw input: accesskit action")
                    }
                    _ => {}
                }
            }
        });
        // 窗口标题随页面同步（code (3).html winTitles 行为）
        if self.title_page != self.page {
            self.title_page = self.page;
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Title(
                self.page.title(self.lang).to_string(),
            ));
        }

        // 拖放本地文件 → 本地播放（独立可用）
        let dropped: Vec<std::path::PathBuf> = ui
            .input(|i| i.raw.dropped_files.clone())
            .into_iter()
            .map(|f| f.path().to_path_buf())
            .collect();
        if let Some(path) = dropped.into_iter().next() {
            self.play_local_path(&path);
        }

        let ctx = ui.ctx().clone();
        // 托盘命令、设置动作都要在没有 `&mut Ui` 的地方发视口命令，先把上下文存下
        self.ctx = Some(ctx.clone());
        self.drain_responses(&ctx);
        self.tick_login_poll();
        // 每日推荐的跨天检查：每帧一次整数比较，跨天后自动换成当天数据
        self.tick_daily_rollover();
        // 心动模式续批检查：每帧几次整数比较，队列快见底才发一次请求
        self.extend_heartbeat();
        if self.core.tick() == TickAction::TrackEnded {
            self.auto_next();
        }
        self.poll_media_keys();
        self.poll_smtc();
        self.poll_tray();
        self.handle_shortcuts(&ctx);
        // 视觉校对探针：右键菜单画在 Foreground 层，是否被裁剪只能看像素
        #[cfg(debug_assertions)]
        if self.menu.is_none() && super::probe::wants_context_menu() {
            if let Some(track) = self.current_tracks().first().cloned() {
                self.open_track_menu_at(track, 0, pos2(600.0, 200.0));
            }
        }
        // 视觉校对探针：长文案确认框的折行/纸面高度只能看像素
        #[cfg(debug_assertions)]
        if self.confirm.is_none() && super::probe::wants_confirm_modal() {
            self.confirm = Some(ConfirmDialog {
                title: self.t("隐藏账号歌单", "HIDE ACCOUNT PLAYLIST").to_string(),
                message: self
                    .t(
                        "「【东方vocal】自选神曲篇」将从列表隐藏，直到下次刷新账号歌单。云端数据不受影响。",
                        "\"【Touhou vocal】Self-selected favorites\" will be hidden until the next account sync. Cloud data is unaffected.",
                    )
                    .to_string(),
                confirm_label: self.t("隐藏", "HIDE").to_string(),
                action: ConfirmAction::HideAccountPlaylist("probe".to_string()),
            });
        }
        // 视觉校对探针：专辑页（磁带架复用 + 返回入口）需要真实网络内容
        #[cfg(debug_assertions)]
        if self.shelf_target.is_none() {
            if let Some(id) = super::probe::forced_album() {
                self.open_album_page(&id, "PROBE ALBUM");
            }
        }
        // 视觉校对探针：推荐页原地详情（点开后的状态探针无法用鼠标触发）
        #[cfg(debug_assertions)]
        if self.rec_peek.is_none() && self.chart_peek.is_none() {
            if let Some((panel, id)) = super::probe::forced_peek() {
                let panel = match panel {
                    "chart" => PeekPanel::Chart,
                    _ => PeekPanel::Rec,
                };
                self.open_peek(panel, &id, "PROBE PLAYLIST");
                self.set_page(Page::Rec);
            }
        }
        self.tick_history();
        self.update_lyric_focus();
        self.push_desktop_lyric();
        self.animate_deck(dt);

        // 边缘缩放热区（6px 边带，8 向）：贴 viewport 最外缘，最先命中
        let viewport_rect = ui.max_rect();
        if !ui.input(|i| i.viewport().maximized.unwrap_or(false)) {
            window::edge_resize(ui, viewport_rect);
        }

        // 窗口框（2px ink 边界 + 10px 内距，无桌面底/无投影——R8：原生窗口不搬网页外壳）
        let mut title_actions: Vec<WindowAction> = Vec::new();
        egui::Frame {
            fill: pal().paper,
            stroke: stroke_structural(),
            inner_margin: margin_window(),
            corner_radius: radius_none(),
            ..egui::Frame::NONE
        }
        .show(ui, |ui| {
            ui.set_min_width(720.0);
            if self.fullscreen {
                // 全屏态隐藏自绘标题栏（没有窗口边框可拖），只留一条细间距
                ui.add_space(6.0);
            } else {
                title_actions = self.ui_window_title(ui);
                ui.add_space(GAP_PANEL);
            }
            self.ui_page_nav(ui);

            let (body, _) = ui.allocate_exact_size(
                vec2(ui.available_width(), ui.available_height().max(320.0)),
                Sense::hover(),
            );
            ui.scope_builder(UiBuilder::new().max_rect(body), |ui| match self.page {
                Page::Player => self.ui_page_player(ui),
                Page::Search => self.ui_page_search(ui),
                Page::Rec => self.ui_page_rec(ui),
                Page::Daily => self.ui_page_daily(ui),
                Page::Playlist => self.ui_page_playlist(ui),
            });
        });

        // 标题栏动作（拖动/双击已由 window 模块即时发命令）
        for action in title_actions {
            match action {
                WindowAction::Close => {
                    // 开了托盘就收进托盘（用户明确选了"关闭窗口不退出"）；
                    // 没开托盘维持原行为：淡出后真退出
                    if self.tray_enabled {
                        self.hidden_to_tray = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                    } else {
                        self.close_anim.start();
                    }
                }
                WindowAction::Minimize => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true))
                }
                WindowAction::ToggleMaximize => {
                    let maximized = ui.input(|i| i.viewport().maximized.unwrap_or(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }
            }
        }

        // 托盘「退出」：不走关闭动画，用户已经明确选过退出
        if self.quit_requested {
            self.quit_requested = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // 关闭淡出动画：桌面色渐显 ≈160ms 后才真正 Close（自然流畅的关闭动画）
        if self.close_anim.active() {
            if self.close_anim.tick(dt) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                let alpha = self.close_anim.alpha();
                ui.painter().rect_filled(
                    viewport_rect,
                    radius_none(),
                    with_opacity(pal().paper, alpha),
                );
            }
        }

        self.ui_overlays(&ctx);

        // 窗口不可见 / 失焦时 egui 没有输入事件就不会自己重绘，而托盘菜单事件、
        // SMTC 按钮事件、媒体键都是**通道轮询**——不重绘就收不到，表现为
        // 「收进托盘后右键菜单点了没反应」「耳机上的播放键要等鼠标动一下才生效」。
        // 这里在后台态按 4Hz 要帧：足够跟手，也不至于空转耗电。
        if self.hidden_to_tray || ui.input(|i| i.viewport().focused == Some(false)) {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
        // 视觉校对探针：仅在设置了 REIMU_PROBE_PAGE 时生效（生产路径无副作用）
        if let Some((target, page)) = super::probe::requested_page() {
            if !super::probe::free_nav() && self.page != page {
                self.set_page(page);
            }
            super::probe::drive(&ctx, target);
        }
        // 动画（舱体/加载点/频谱）+ 响应轮询。
        // 舱体动作与频谱需要 ~60fps；固定 100ms（10fps）会让开舱/装带明显卡顿，
        // 这是「载入阶段磁带动画一顿一顿」的直接原因。
        let frame_ms = if self.needs_smooth_frames() { 16 } else { 100 };
        ctx.request_repaint_after(Duration::from_millis(frame_ms));
    }
}

impl Drop for ReimuApp {
    fn drop(&mut self) {
        // 资源释放收口：停引擎 → 释放媒体 → 删净临时文件
        self.core.shutdown();
        // 桌面歌词悬浮窗是独立 viewport，不显式关闭会残留一个空窗
        self.desktop_lyric.detach();
        // 媒体键线程：置空即 Drop → PostThreadMessageW(WM_QUIT) 收掉消息泵
        self.media_keys = None;
        // 托盘图标：不显式移除会留一个"幽灵图标"在任务栏，鼠标划过才消失
        self.tray = None;
        // SMTC 会话：注销掉，避免系统媒体浮层继续显示一个已经退出的播放器
        self.smtc = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::music::provider::MusicProvider;

    /// 最小可替换 Mock：验证装配不依赖具体 Provider。
    struct AppMock(std::sync::atomic::AtomicU8);

    impl AppMock {
        fn new() -> Self {
            Self(std::sync::atomic::AtomicU8::new(
                QualityCeiling::default().index(),
            ))
        }
    }

    impl MusicProvider for AppMock {
        fn id(&self) -> crate::music::provider::ProviderId {
            crate::music::provider::ProviderId::Netease
        }

        fn set_quality(&self, ceiling: QualityCeiling) {
            self.0
                .store(ceiling.index(), std::sync::atomic::Ordering::Relaxed);
        }

        fn quality(&self) -> QualityCeiling {
            QualityCeiling::from_index(self.0.load(std::sync::atomic::Ordering::Relaxed))
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
        ) -> Result<Playlist, ProviderError> {
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

        async fn playback_source(
            &self,
            _song_id: &str,
        ) -> Result<crate::music::provider::PlaybackSource, ProviderError> {
            Err(ProviderError::PlaybackUrlUnavailable)
        }

        async fn lyrics(
            &self,
            _song_id: &str,
        ) -> Result<crate::music::lyrics::Lyrics, ProviderError> {
            Err(ProviderError::EmptyResult)
        }

        async fn login_status(&self) -> Result<crate::music::provider::LoginState, ProviderError> {
            Ok(crate::music::provider::LoginState::Anonymous)
        }

        async fn qr_login_start(
            &self,
        ) -> Result<crate::music::provider::QrLoginSession, ProviderError> {
            Ok(crate::music::provider::QrLoginSession {
                key: "mock-key".into(),
                qr_url: "https://example.invalid/qr".into(),
            })
        }

        async fn qr_login_poll(
            &self,
            _key: &str,
        ) -> Result<crate::music::provider::LoginState, ProviderError> {
            Ok(crate::music::provider::LoginState::QrWaiting)
        }

        async fn login_cellphone(
            &self,
            _phone: &str,
            _password: &str,
        ) -> Result<crate::music::provider::LoginState, ProviderError> {
            Ok(crate::music::provider::LoginState::Anonymous)
        }

        async fn login_cookie(
            &self,
            _cookie: &str,
        ) -> Result<crate::music::provider::LoginState, ProviderError> {
            Ok(crate::music::provider::LoginState::Anonymous)
        }

        async fn logout(&self) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    fn test_app() -> ReimuApp {
        // eframe::CreationContext 无法凭空构造；装配逻辑已拆出纯函数，
        // 这里只测与 UI 无关的请求/状态流转。引擎为空（无设备路径）。
        let factory: Arc<dyn Fn(ProviderTimeouts) -> Arc<dyn DynMusicProvider> + Send + Sync> =
            Arc::new(|_| Arc::new(AppMock::new()) as Arc<dyn DynMusicProvider>);
        let fetcher = super::super::bridge::noop_fetcher();
        let bridge = ProviderBridge::spawn(factory(ProviderTimeouts::default()), fetcher);
        ReimuApp::assemble(bridge, PlayerCore::new(None), None)
    }

    fn sample_track(id: &str) -> Track {
        Track {
            provider: crate::music::provider::ProviderId::Netease,
            id: id.to_string(),
            title: format!("Track {id}"),
            artists: vec!["Artist".to_string()],
            album: Some("Album".to_string()),
            duration_ms: Some(60_000),
            cover_url: None,
            ..Track::default()
        }
    }

    #[test]
    fn empty_keyword_does_not_dispatch_request() {
        let mut app = test_app();
        app.search_input = "   ".into();
        app.submit_search();
        assert_eq!(app.search_seq, 0, "空关键词不得发起请求");
        assert!(app.search_state.is_idle());
        assert!(app.toast.is_some(), "应有轻提示");
    }

    #[test]
    fn empty_playlist_id_does_not_dispatch_request() {
        let mut app = test_app();
        app.playlist_input = "".into();
        app.submit_playlist();
        assert_eq!(app.playlist_seq, 0);
        assert!(app.playlist_state.is_idle());
    }

    #[test]
    fn search_dispatch_enters_loading_and_sets_seq() {
        let mut app = test_app();
        app.search_input = "city pop".into();
        app.submit_search();
        assert_eq!(app.search_seq, 1);
        assert!(app.search_state.is_loading());
        assert_eq!(app.recent[0], "city pop", "应记入最近搜索");
    }

    #[test]
    fn handle_error_empty_result_is_toast_not_modal() {
        let mut app = test_app();
        app.search_input = "a".into();
        app.submit_search();
        app.handle_error("SEARCH FAILED", &ProviderError::EmptyResult);
        assert!(app.error_modal.is_none(), "空结果不该弹模态");
        assert!(app.toast.is_some());
        assert!(app.search_state.is_failed());
    }

    #[test]
    fn handle_error_hard_failure_opens_modal_with_human_message() {
        let mut app = test_app();
        app.search_input = "a".into();
        app.submit_search();
        app.handle_error("SEARCH FAILED", &ProviderError::RequestTimeout);
        let (title, message) = app.error_modal.clone().expect("硬错误应弹模态");
        // 默认语言是中文：标题也必须已经汉化
        assert_eq!(title, "搜索失败");
        assert!(message.contains("超时"), "人话提示: {}", message);
        assert!(!message.contains('{'), "不得暴露 JSON: {}", message);

        // 切到英文后同一个路由键给出英文标题（键本身不变）
        app.lang = Lang::En;
        app.error_modal = None;
        app.handle_error("SEARCH FAILED", &ProviderError::RequestTimeout);
        let (title_en, _) = app.error_modal.clone().expect("英文态同样要弹");
        assert_eq!(title_en, "SEARCH FAILED");
    }

    #[test]
    fn next_index_empty_list_does_not_crash() {
        let mut app = test_app();
        assert_eq!(app.next_index(), None);
        assert_eq!(app.prev_index(), None);
        assert_eq!(app.peek_next_index(), None);
    }

    #[test]
    fn end_of_list_wraps_only_in_repeat_all() {
        // 默认档是顺序播放（不循环）：末尾就是末尾，不回绕也不报错
        let mut app = test_app();
        app.active_local = false;
        app.active_playlist = None;
        app.search_state.finish(Ok(vec![
            sample_track("a"),
            sample_track("b"),
            sample_track("c"),
        ]));
        assert_eq!(app.core.play_mode(), PlayMode::Sequential, "默认档");
        app.selected = Some(2); // 停在末尾
        assert_eq!(app.next_index(), None, "顺序播放到末尾不该回绕");
        app.core.set_play_mode(PlayMode::RepeatAll);
        app.selected = Some(2);
        assert_eq!(app.next_index(), Some(0), "列表循环应回绕到 0");
        app.core.set_play_mode(PlayMode::RepeatOne);
        app.selected = Some(2);
        assert_eq!(app.next_index(), None, "单曲循环下手动下一曲不回绕");
    }

    #[test]
    fn shuffle_never_immediately_repeats_the_current_track() {
        // 随机（洗牌序列）首抽不得命中当前曲——旧实现有 1/len 概率原地重播
        let mut app = test_app();
        app.active_local = false;
        app.active_playlist = None;
        let tracks: Vec<Track> = (0..5).map(|i| sample_track(&i.to_string())).collect();
        app.search_state.finish(Ok(tracks));
        app.core.shuffle = true;
        app.selected = Some(0);
        let next = app.next_index().expect("随机下应有下一首");
        assert_ne!(next, 0, "随机播放不得原地重播当前曲");
    }

    #[test]
    fn shuffle_visits_every_track_before_repeating() {
        // 洗牌序列一轮应恰好覆盖所有索引各一次（无遗漏、无提前重复）
        let mut app = test_app();
        app.active_local = false;
        app.active_playlist = None;
        let tracks: Vec<Track> = (0..5).map(|i| sample_track(&i.to_string())).collect();
        app.search_state.finish(Ok(tracks));
        app.core.shuffle = true;
        app.selected = Some(0);
        let mut seen: Vec<usize> = (0..5).map(|_| app.next_index().unwrap()).collect();
        seen.sort_unstable();
        assert_eq!(seen, vec![0, 1, 2, 3, 4], "一轮应覆盖所有曲目");
    }

    #[test]
    fn mode_key_cycles_the_four_modes_in_order() {
        // 左键照播放键的交互：点一下走一档，八次点击正好两圈，不会停在中间态或跳档
        let mut app = test_app();
        let mut seen = vec![];
        for _ in 0..9 {
            app.cycle_play_mode();
            seen.push(app.core.play_mode());
        }
        assert_eq!(
            seen,
            vec![
                PlayMode::Shuffle,
                PlayMode::RepeatOne,
                PlayMode::RepeatAll,
                PlayMode::Sequential,
                PlayMode::Shuffle,
                PlayMode::RepeatOne,
                PlayMode::RepeatAll,
                PlayMode::Sequential,
                PlayMode::Shuffle,
            ],
            "四档应严格按 顺序→随机→单曲循环→列表循环 循环"
        );
    }

    #[test]
    fn mode_key_never_stacks_shuffle_on_repeat_one() {
        // 「随机 + 单曲循环」是走不通的组合（自动续播永不换曲），任何一档都不能留下它
        let mut app = test_app();
        for _ in 0..12 {
            app.cycle_play_mode();
            let stacked = app.core.shuffle && app.core.repeat_mode.is_one();
            assert!(
                !stacked,
                "随机与单曲循环不得同时生效：{:?}",
                app.core.play_mode()
            );
        }
    }

    /// 四档必须有四种**看得出来的**样子，而且每个字形都得真在图标字库里。
    ///
    /// 回归：单曲循环原本用 Font Awesome 的 `repeat-1`（U+F365），但随包字体的
    /// Unicode(cmap format 12) 子表里没有这个码点，键面直接画成空心豆腐块。
    /// 现在改成 `repeat` 打底 + 等宽字体 `1` 角标，两者都确定存在。
    #[test]
    fn four_modes_look_distinct_and_use_glyphs_the_font_has() {
        let app = test_app();
        let modes = [
            PlayMode::Sequential,
            PlayMode::Shuffle,
            PlayMode::RepeatOne,
            PlayMode::RepeatAll,
        ];
        let seen: Vec<(&str, Option<&str>)> = modes
            .iter()
            .map(|m| (app.play_mode_glyph(*m), app.play_mode_badge(*m)))
            .collect();
        assert_eq!(
            seen.iter().collect::<std::collections::HashSet<_>>().len(),
            4,
            "四档字形+角标组合必须互不相同：{seen:?}"
        );
        for (glyph, _) in &seen {
            assert_ne!(*glyph, icons::REPEAT_ONE, "缺字字形不得再出现在键面上");
            assert!(icons::is_icon_text(glyph), "{glyph:?} 应是图标字库字形");
        }
        // 角标用界面等宽字体，不能是 PUA（否则会被切进图标字库变成豆腐）
        assert_eq!(
            app.play_mode_badge(PlayMode::RepeatOne),
            Some("1"),
            "单曲循环必须有 1 角标"
        );
        for m in modes {
            if let Some(badge) = app.play_mode_badge(m) {
                assert!(!icons::is_icon_text(badge), "{badge:?} 不该走图标字库");
            }
        }
    }

    #[test]
    fn switching_back_to_sequential_resumes_list_order() {
        // 按钮显示与播放行为一致：切回顺序后，下一首必须是当前曲的下一索引
        let mut app = test_app();
        app.active_local = false;
        app.active_playlist = None;
        let tracks: Vec<Track> = (0..5).map(|i| sample_track(&i.to_string())).collect();
        app.search_state.finish(Ok(tracks));
        app.selected = Some(0);

        app.core.set_play_mode(PlayMode::Shuffle);
        assert!(app.shuffle_effective(), "随机档应真的随机");
        app.next_index();

        app.core.set_play_mode(PlayMode::Sequential);
        assert!(!app.shuffle_effective(), "顺序档不应再走洗牌序列");
        let current = app.selected.expect("选中曲仍在");
        assert_eq!(
            app.next_index(),
            Some(current + 1),
            "顺序播放的下一首应是列表的下一个索引"
        );
    }

    #[test]
    fn play_track_switches_page_and_enters_buffering() {
        let mut app = test_app();
        app.page = Page::Search;
        let track = sample_track("347230");
        app.play_track(track);
        assert_eq!(app.page, Page::Player, "装带后应跳回播放页");
        assert!(app.core.is_buffering(), "应进入装载/缓冲态");
        assert!(app.core.prepare_seq > 0, "应发起装载请求");
        assert!(app.core.lyric_seq > 0, "应发起歌词请求");
        assert!(app.known.contains_key("347230"), "曲目应登记进 known");
    }

    #[test]
    fn cancel_invalidates_inflight_fetch() {
        let mut app = test_app();
        app.play_track(sample_track("1"));
        app.core.cancel(&mut app.bridge);
        assert!(!app.core.is_buffering());
        assert_eq!(app.core.prepare_seq, 0);
    }

    #[test]
    fn local_play_dispatches_no_provider_requests() {
        let mut app = test_app();
        app.play_local_path(Path::new("Q:\\missing\\track.mp3"));
        assert_eq!(app.search_seq, 0);
        assert_eq!(app.playlist_seq, 0);
        assert_eq!(app.detail_seq, 0);
        assert_eq!(app.page, Page::Player);
    }

    #[test]
    fn core_event_failure_opens_modal_with_reason() {
        let mut app = test_app();
        app.on_core_event(CoreEvent::Failed(
            crate::gui::player_core::PlaybackFailure::Provider(
                ProviderError::PlaybackUrlUnavailable,
            ),
        ));
        let (title, message) = app.error_modal.clone().expect("失败应弹模态");
        assert_eq!(title, "播放失败");
        assert!(message.contains("无法播放"));
    }

    #[test]
    fn auto_next_on_empty_list_stops_gracefully() {
        let mut app = test_app();
        app.auto_next();
        assert_eq!(app.core.engine_state(), "NoDevice");
    }

    #[test]
    fn lyric_focus_only_on_line_change() {
        let mut app = test_app();
        app.update_lyric_focus();
        assert_eq!(app.lyric_focus, None);
        assert_eq!(app.last_lyric, None);
    }

    // ---- Rec 页（精选列表派发 / 错误语义）-------------------------------

    #[test]
    fn set_page_rec_dispatches_both_featured_requests() {
        let mut app = test_app();
        app.set_page(Page::Rec);
        assert_eq!(app.rec_seq, 1, "推荐歌单应发起请求");
        assert_eq!(app.chart_seq, 2, "官方排行榜应发起请求");
        assert!(app.rec_state.is_loading());
        assert!(app.chart_state.is_loading());
    }

    #[test]
    fn ensure_featured_is_lazy_and_never_redispatches_inflight() {
        let mut app = test_app();
        app.set_page(Page::Rec);
        app.set_page(Page::Player);
        app.set_page(Page::Rec); // 回到 Rec 页：加载中不得重发
        assert_eq!(app.rec_seq, 1);
        assert_eq!(app.chart_seq, 2);
    }

    #[test]
    fn featured_error_empty_result_is_toast_not_modal() {
        let mut app = test_app();
        app.rec_state.begin_load();
        app.rec_state.finish(Err(ProviderError::EmptyResult));
        app.report_featured_error("REC FAILED", &ProviderError::EmptyResult);
        assert!(app.error_modal.is_none(), "空结果不该弹模态");
        assert!(app.toast.is_some());
        assert!(app.rec_state.is_failed());
    }

    #[test]
    fn featured_error_hard_failure_opens_modal_with_human_message() {
        let mut app = test_app();
        app.chart_state.begin_load();
        app.report_featured_error("CHART FAILED", &ProviderError::RequestTimeout);
        let (title, message) = app.error_modal.clone().expect("硬错误应弹模态");
        assert_eq!(title, "榜单获取失败");
        assert!(!message.contains('{'), "不得暴露 JSON: {}", message);
    }

    #[test]
    fn load_playlist_dispatches_and_enters_loading() {
        let mut app = test_app();
        app.load_playlist_id("24381616", false, false);
        assert_eq!(app.playlist_input, "24381616");
        assert_eq!(app.playlist_seq, 1);
        assert!(app.playlist_state.is_loading());
    }

    #[test]
    fn featured_display_formatting_never_fabricates() {
        assert_eq!(ReimuApp::fmt_play_count(999), "999");
        assert_eq!(ReimuApp::fmt_play_count(12_345), "12.3K");
        assert_eq!(ReimuApp::fmt_play_count(4_500_000), "4.5M");
        assert_eq!(ReimuApp::fmt_track_count(Some(30)), "30");
        assert_eq!(ReimuApp::fmt_track_count(None), "—", "缺失不伪造");
    }

    // ---- 本地曲库 / 设置（阶段 G）--------------------------------------

    /// 测试临时目录（用例自建自清）。
    fn temp_dir(case: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "reimu-player-app-lib-{}-{case}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建测试目录");
        dir
    }

    #[test]
    fn load_library_dir_scans_and_activates_local_card() {
        let dir = temp_dir("scan");
        std::fs::write(dir.join("A - One.mp3"), b"").unwrap();
        std::fs::write(dir.join("Two.flac"), b"").unwrap();
        std::fs::write(dir.join("readme.txt"), b"").unwrap();

        let mut app = test_app();
        app.apply_library_dir(&dir, false);

        let lib = app.library.as_ref().expect("曲库应入库");
        assert_eq!(lib.tracks.len(), 2, "只收音频文件");
        assert!(app.active_local, "扫描后应选中本地曲库卡");
        assert_eq!(app.active_playlist, None);
        assert_eq!(app.selected, Some(0));
        assert!(app.toast.is_some(), "应有扫描摘要提示");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_library_dir_failure_keeps_existing_library() {
        let dir = temp_dir("keep");
        std::fs::write(dir.join("a.mp3"), b"").unwrap();
        let mut app = test_app();
        app.apply_library_dir(&dir, false);
        let before = app.library.clone();

        app.apply_library_dir(std::path::Path::new("Q:\\not\\a\\dir"), false);
        assert_eq!(app.library, before, "失败不得改动既有曲库");
        assert!(app.toast.is_some(), "失败应有人话提示");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn play_track_local_routes_without_network() {
        let mut app = test_app();
        app.page = Page::Search;
        let track = Track {
            provider: ProviderId::Local,
            id: "Q:\\missing\\track.mp3".to_string(),
            title: "Track".to_string(),
            artists: vec!["—".to_string()],
            album: None,
            duration_ms: None,
            cover_url: None,
            ..Track::default()
        };
        app.play_track(track);
        assert_eq!(app.page, Page::Player);
        assert_eq!(app.search_seq, 0, "本地播放不得触网");
        assert_eq!(app.playlist_seq, 0);
        assert_eq!(app.detail_seq, 0);
        assert_eq!(app.rec_seq, 0);
    }

    #[test]
    fn current_tracks_prefers_local_library_when_active() {
        let dir = temp_dir("cur");
        std::fs::write(dir.join("a.mp3"), b"").unwrap();
        let mut app = test_app();
        app.apply_library_dir(&dir, false);
        let expect = app.library.as_ref().unwrap().tracks.len();
        assert_eq!(app.current_tracks().len(), expect);

        app.active_local = false;
        assert!(app.current_tracks().is_empty(), "未选中时不混入本地曲目");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quality_change_takes_effect_without_rebuilding_provider() {
        let mut app = test_app();
        assert_eq!(app.bridge.quality(), QualityCeiling::Lossless);
        app.apply_quality(QualityCeiling::ExtraHigh);
        // 直接读回 Provider：证明没有重建、没有走请求队列
        assert_eq!(app.bridge.quality(), QualityCeiling::ExtraHigh);
        assert!(app.toast.is_some(), "切换后应给出提示");
    }

    #[test]
    fn setting_the_same_quality_is_a_noop() {
        let mut app = test_app();
        app.apply_quality(QualityCeiling::Lossless); // 与默认档相同
        assert!(app.toast.is_none(), "无变化不该弹提示");
    }

    /// 音频预取是"连续听歌零等待"的唯一来源：起播后必须给**实际下一首**
    /// 发一次装载请求（不是当前这首，也不是浏览列表的第一首）。
    #[test]
    fn started_fires_an_audio_prefetch_for_the_next_track() {
        let mut app = test_app();
        app.queue = vec![sample_track("1"), sample_track("2"), sample_track("3")];
        app.queue_active = true;
        app.selected = Some(0);

        app.prefetch_next();

        assert_ne!(app.prefetch_seq, 0, "必须给下一首发一次音频预取");
        assert_eq!(app.prefetch_song.as_deref(), Some("2"), "目标是下一首");
    }

    /// 同一首已经登记过就不再发第二次：`prefetch_next` 每次 `Started` 都会
    /// 跑一遍，重复发等于把装载 lane 塞满重复请求。
    #[test]
    fn a_second_prefetch_for_the_same_song_is_skipped() {
        let mut app = test_app();
        app.queue = vec![sample_track("1"), sample_track("2")];
        app.queue_active = true;
        app.selected = Some(0);

        app.prefetch_next();
        let first = app.prefetch_seq;
        app.prefetch_next();
        assert_eq!(app.prefetch_seq, first, "同一首不得重复预取");
    }

    /// 预取被掐掉（换台/取消）后必须能再发一次。
    ///
    /// 不清账的后果：那首歌被永久记成"已预取"，于是**永远不再被预取**——
    /// 用户感受到的是"每次切到它都要等整首下载"，且不会自愈。
    #[test]
    fn a_cancelled_prefetch_can_be_requested_again() {
        let mut app = test_app();
        app.queue = vec![sample_track("1"), sample_track("2")];
        app.queue_active = true;
        app.selected = Some(0);

        app.prefetch_next();
        let first = app.prefetch_seq;
        assert_ne!(first, 0);

        app.forget_inflight(first);
        assert_eq!(app.prefetch_seq, 0, "取消回执必须销账");
        assert!(app.prefetch_song.is_none());

        app.prefetch_next();
        assert_ne!(app.prefetch_seq, 0, "销账后必须能重新预取");
        assert_ne!(app.prefetch_seq, first, "新请求要拿新 seq");
    }

    /// 单曲循环时"下一首"就是自己，预取自己纯属空转（还会把正在播的那份
    /// 文件卷进淘汰判定）。
    #[test]
    fn a_repeat_one_track_never_prefetches_itself() {
        let mut app = test_app();
        app.queue = vec![sample_track("1")];
        app.queue_active = true;
        app.selected = Some(0);

        app.prefetch_next();
        assert_eq!(app.prefetch_seq, 0, "唯一一首不得预取自己");
    }

    // ------------------------------------------------------------------
    // 分享链接入口（粘贴即解析）
    // ------------------------------------------------------------------

    #[test]
    fn pasting_a_playlist_link_opens_playlist_not_search() {
        let mut app = test_app();
        app.search_input = "https://music.163.com/playlist?id=24381616".into();
        app.submit_search();
        assert_eq!(app.search_seq, 0, "链接不得被当成关键词搜");
        assert!(app.playlist_seq > 0, "应派发歌单请求");
        assert!(app.playlist_state.is_loading());
    }

    #[test]
    fn pasting_a_song_link_defers_play_until_details_arrive() {
        let mut app = test_app();
        app.search_input =
            "分享单曲《夜曲》: https://music.163.com/song?id=185811 （来自 @网易云音乐）".into();
        app.submit_search();
        assert_eq!(app.search_seq, 0);
        assert!(app.detail_seq > 0, "单曲链接要先拉详情");
        assert_eq!(app.link_song.as_deref(), Some("185811"));
        assert_eq!(app.page, Page::Player, "拉详情阶段不切页");
    }

    #[test]
    fn pasting_an_mv_link_is_rejected_with_a_toast() {
        let mut app = test_app();
        app.search_input = "https://music.163.com/mv?id=5436712".into();
        app.submit_search();
        assert_eq!(app.search_seq, 0);
        assert_eq!(app.playlist_seq, 0);
        assert_eq!(app.detail_seq, 0);
        let (msg, _) = app.toast.clone().expect("应给出拒绝提示");
        assert!(msg.contains("不支持"), "提示要说清不支持: {msg}");
    }

    #[test]
    fn short_link_triggers_expansion_instead_of_direct_load() {
        let mut app = test_app();
        app.search_input = "http://163cn.tv/AbCdEf".into();
        app.submit_search();
        assert_eq!(app.search_seq, 0);
        assert_eq!(app.playlist_seq, 0);
        assert_eq!(app.link_seq, 1, "短链要先展开");
        assert_eq!(app.link_pending.as_deref(), Some("http://163cn.tv/AbCdEf"));
    }

    #[test]
    fn plain_keyword_still_searches() {
        let mut app = test_app();
        app.search_input = "夜曲".into();
        app.submit_search();
        assert!(app.search_seq > 0);
        assert_eq!(app.playlist_seq, 0, "普通关键词不得走链接通道");
    }

    #[test]
    fn playlist_input_accepts_share_link_too() {
        let mut app = test_app();
        app.playlist_input = "https://music.163.com/playlist?id=777".into();
        app.submit_playlist();
        assert!(app.playlist_seq > 0);
        assert_eq!(app.playlist_input, "777", "解析后回填纯 ID");
    }

    // ------------------------------------------------------------------
    // 每日歌单 / 推荐详情
    // ------------------------------------------------------------------

    /// 推荐/每日里点播：磁带架换成这张歌单（替换），而不是只换队列。
    #[test]
    fn playing_a_peek_track_replaces_the_shelf_content() {
        let mut app = test_app();
        let list = crate::music::provider::Playlist {
            provider: crate::music::provider::ProviderId::Netease,
            id: "5001".into(),
            name: Some("推荐歌单".into()),
            total: Some(2),
            tracks: vec![sample_track("x"), sample_track("y")],
        };
        app.rec_peek = Some(PlaylistPeek {
            id: list.id.clone(),
            subtitle: "推荐歌单".into(),
            seq: 1,
            state: LoadState::Ready(list),
            expanded: true,
        });
        // 先让磁带架上有一份别的列表
        app.search_state.finish(Ok(vec![sample_track("old")]));
        app.active_local = false;

        app.play_peek_track(PeekPanel::Rec, 1);

        let shelf: Vec<&str> = app.current_tracks().iter().map(|t| t.id.as_str()).collect();
        assert_eq!(shelf, vec!["x", "y"], "磁带架必须换成这张歌单");
        assert_eq!(app.queue.len(), 2, "队列与磁带架同一份内容");
        assert_eq!(app.selected, Some(1), "选中就是刚点的那首");
    }

    #[test]
    fn entering_daily_page_requests_daily_songs_and_stamps_the_day() {
        // 每日页要的是「当天推荐的歌曲」本身：进页面就自动请求，
        // 并把"这批属于哪一天"记下来（跨天比较全靠这个日期键）。
        let mut app = test_app();
        app.set_page(Page::Daily);
        assert!(app.daily_seq > 0, "进页面必须自动请求，无需任何点击");
        assert!(app.daily_state.is_loading());
        assert_eq!(
            app.daily_day,
            super::super::daily_clock::today_key(),
            "请求发出时就该记下当天日期"
        );
    }

    #[test]
    fn daily_rollover_refetches_and_drops_the_stale_batch() {
        // 跨天：旧内容整批作废 + 重新请求。这条是"每日推荐随日期变化自动更新"
        // 的落地点——不重拉的话第二天看到的还是昨天那批。
        let mut app = test_app();
        app.set_page(Page::Daily);
        let first_seq = app.daily_seq;
        app.daily_state.finish(Ok(vec![sample_track("yesterday")]));
        let yesterday = super::super::daily_clock::today_key();
        app.daily_day = yesterday;

        // 同一天：不许乱发请求（否则每次检查都打一次接口）
        app.check_daily_rollover(yesterday);
        assert_eq!(app.daily_seq, first_seq, "同一天不应重新请求");
        assert!(app.daily_state.is_ready(), "同一天不应清掉已有内容");

        // 换一天：必须重拉，且旧数据立刻释放
        app.check_daily_rollover(yesterday + 1);
        assert!(app.daily_seq > first_seq, "跨天必须重新请求");
        assert!(app.daily_state.is_loading());
        assert!(app.daily_state.get().is_none(), "旧批次不得残留");
        assert_eq!(app.daily_day, super::super::daily_clock::today_key());
    }

    #[test]
    fn daily_rollover_ignores_a_never_loaded_page() {
        // 从没拉过（daily_day == 0）不该在后台偷偷发请求
        let mut app = test_app();
        app.check_daily_rollover(super::super::daily_clock::today_key());
        assert_eq!(app.daily_seq, 0);
        assert!(app.daily_state.is_idle());
    }

    #[test]
    fn playing_a_daily_row_uses_the_whole_batch_as_queue() {
        // 「今天这 30 首」就是队列：下一首仍留在同一批里，
        // 而不是把别的页面列表（搜索/歌单）顶进队列。
        let mut app = test_app();
        app.set_page(Page::Daily);
        let batch = vec![sample_track("d1"), sample_track("d2"), sample_track("d3")];
        app.daily_state.finish(Ok(batch.clone()));
        app.search_state
            .finish(Ok(vec![sample_track("search-only")]));

        app.play_daily_index(1);

        assert_eq!(
            app.queue.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["d1", "d2", "d3"],
            "队列应来自当天推荐批次"
        );
        assert_eq!(app.selected, Some(1));
        assert_eq!(
            app.current_track.as_ref().map(|t| t.id.as_str()),
            Some("d2")
        );
    }

    // ------------------------------------------------------------------
    // 登录面板
    // ------------------------------------------------------------------

    #[test]
    fn open_login_starts_qr_when_no_session() {
        let mut app = test_app();
        app.open_login();
        assert!(app.login.open);
        assert!(app.qr_seq > 0, "未登录时打开面板应立即发起二维码");
        assert!(app.login.error.is_none());
    }

    #[test]
    fn open_login_starts_qr_even_after_anonymous_status() {
        // 回归：启动时查登录状态会先把 state 置为 Anonymous。此时打开面板
        // 仍必须取码，否则面板永远停在 REQUESTING…（用户扫码无从谈起）。
        let mut app = test_app();
        app.login.state = Some(LoginState::Anonymous);
        app.open_login();
        assert!(app.login.open);
        assert!(app.qr_seq > 0, "匿名态（已查过登录状态）也必须发起二维码");
    }

    #[test]
    fn qr_texture_fits_panel_without_scaling() {
        // 纹理必须能 1:1 落进面板框：一旦被 painter 缩放，模块宽度会在
        // 3px/4px 间跳动，手机摄像头容易误读（"看着正常但扫不出来"）。
        let box_side = login::QR_BOX_SIDE as usize;
        for modules in [21, 25, 29, 33, 37, 41, 45, 49, 57] {
            let px = qr_module_px(modules);
            assert!(px >= 1, "模块像素至少 1");
            let natural = (modules + 8) * px;
            assert!(
                natural <= box_side,
                "modules={modules} 时纹理 {natural}px 超出框宽 {box_side}px"
            );
            // 整数模块：纹理边长必须是 (modules+8) 的整数倍
            assert_eq!(natural % (modules + 8), 0);
        }
    }

    #[test]
    fn qr_module_px_is_capped() {
        // 极小二维码（21 模块）不能算出过大的模块像素，否则纹理撑爆面板
        assert!(qr_module_px(21) <= QR_MODULE_PX_MAX);
    }

    #[test]
    fn login_poll_is_scheduled_only_with_a_key() {
        let mut app = test_app();
        app.login.open = true;
        // 没有 key：不发轮询
        app.tick_login_poll();
        assert_eq!(app.qr_poll_seq, 0);
        app.login.qr_key = Some("k".into());
        app.qr_poll_at = Some(app.t + QR_POLL_SECS);
        // 还没到点：不发轮询
        app.tick_login_poll();
        assert_eq!(app.qr_poll_seq, 0);
        // 到点：发一次，并把下一拍推后
        app.t += QR_POLL_SECS;
        app.tick_login_poll();
        assert_eq!(app.qr_poll_seq, 1);
        assert!(app.qr_poll_at.unwrap() > app.t);
    }

    #[test]
    fn login_gate_opens_panel_instead_of_error_modal() {
        let mut app = test_app();
        app.report_login_gate(
            "FM FAILED",
            &ProviderError::AuthenticationRequired,
            "私人 FM",
        );
        assert!(app.login.open, "需要登录应直接给出口（登录面板）");
        assert!(app.error_modal.is_none(), "门槛不是故障，不该弹错误框");
    }

    // ------------------------------------------------------------------
    // 阶段 11：登录入口迁入设置面板 / 中英界面切换
    // ------------------------------------------------------------------

    #[test]
    fn settings_action_open_login_opens_the_panel() {
        // 标题栏 chip 已删除，设置面板的 ACCOUNT 行是唯一入口
        let mut app = test_app();
        assert!(!app.login.open);
        app.apply_settings_action(SettingsAction::OpenLogin);
        assert!(app.login.open, "点 ACCOUNT 行必须打开登录面板");
        assert!(app.qr_seq > 0, "打开面板应立即发起二维码");
    }

    #[test]
    fn settings_action_set_lang_switches_language() {
        let mut app = test_app();
        assert_eq!(app.lang, Lang::Zh, "默认中文");
        app.apply_settings_action(SettingsAction::SetLang(Lang::En));
        assert_eq!(app.lang, Lang::En);
        // 提示语用**新**语言：切到英文后 toast 不能还是中文
        let (msg, _) = app.toast.clone().expect("切语言应给提示");
        assert!(msg.contains("English"), "toast 应跟随新语言: {msg}");
        assert!(!msg.contains("中文"), "toast 不得残留中文: {msg}");
    }

    #[test]
    fn apply_lang_is_idempotent() {
        let mut app = test_app();
        app.apply_lang(Lang::En);
        let first = app.toast.clone();
        app.apply_lang(Lang::En);
        assert_eq!(app.toast, first, "重复设同一语言不该再弹提示");
    }

    #[test]
    fn turning_the_lyric_window_off_resets_the_lock() {
        // 「关掉桌面歌词再打开」必须回到可交互状态：否则用户会遇到
        // "刚打开就点不动、又不知道该去哪解锁"（锁定按钮在歌词窗口自己身上，
        // 而窗口刚打开时他甚至还没看到它）。
        let mut app = test_app();
        app.lyric_control.set_locked(true);
        app.apply_settings_action(SettingsAction::SetDesktopLyric(false));
        assert!(!app.lyric_control.locked(), "关闭后锁定态必须复位");

        // 再打开也不能带着旧的锁定态回来
        app.lyric_control.set_locked(true);
        app.apply_settings_action(SettingsAction::SetDesktopLyric(true));
        assert!(!app.lyric_control.locked(), "开启时也应从可交互开始");
    }

    #[test]
    fn settings_action_cycles_the_progress_style() {
        // 需求：彩色条不再是默认，且保留可切换选项。
        // 这里钉住「面板动作 → 共享状态」这条缝：点一次必须真的换一档。
        let mut app = test_app();
        assert_eq!(
            app.lyric_control.progress(),
            super::super::lyric_control::ProgressStyle::Line,
            "默认必须是细白线"
        );
        let first = app.lyric_control.progress().next();
        app.apply_settings_action(SettingsAction::SetDesktopLyricProgress(first));
        assert_eq!(app.lyric_control.progress(), first);
        // 走满三档能回到起点（彩色是其中一档，但不默认）
        let second = first.next();
        app.apply_settings_action(SettingsAction::SetDesktopLyricProgress(second));
        let third = second.next();
        app.apply_settings_action(SettingsAction::SetDesktopLyricProgress(third));
        assert_eq!(
            app.lyric_control.progress(),
            super::super::lyric_control::ProgressStyle::Line
        );
        let (msg, _) = app.toast.clone().expect("切换样式应给提示");
        assert!(msg.contains("进度条"), "toast 应说明改的是进度条: {msg}");
    }

    #[test]
    fn settings_view_reads_the_shared_progress_style() {
        // 设置面板与歌词窗口读的是同一个对象，面板显示必须跟着实际样式走
        let app = test_app();
        assert_eq!(
            app.settings_view().lyric_progress,
            super::super::lyric_control::ProgressStyle::Line
        );
        app.lyric_control
            .set_progress(super::super::lyric_control::ProgressStyle::Acid);
        assert_eq!(
            app.settings_view().lyric_progress,
            super::super::lyric_control::ProgressStyle::Acid,
            "歌词窗口已切到彩色条，面板却还显示白线"
        );
    }

    #[test]
    fn lyric_appearance_reaches_the_prefs_snapshot() {
        // 透明度与进度条样式属于外观偏好，必须能落盘（否则每次启动都回默认）
        let app = test_app();
        app.lyric_control.set_opacity(37);
        app.lyric_control
            .set_progress(super::super::lyric_control::ProgressStyle::Acid);
        let prefs = app.prefs_snapshot();
        assert_eq!(prefs.lyric_opacity, 37);
        assert_eq!(
            prefs.lyric_progress,
            super::super::lyric_control::ProgressStyle::Acid
        );
    }

    #[test]
    fn probe_opacity_never_reaches_the_prefs_snapshot() {
        // 探针钉死的透明度只影响当次显示，落盘必须是用户原来的值。
        // 踩过的坑：一次 OPACITY=0 的截图跑完把 0% 写进了 prefs.json，
        // 下次正常启动只剩文字、背景全没了，看起来像"歌词窗不见了"。
        let mut app = test_app();
        app.lyric_control.set_opacity(72);
        // 模拟探针生效：显示值被钉到 0，落盘值冻结在 72
        app.lyric_control.set_opacity(0);
        app.lyric_opacity_frozen = Some(72);
        assert_eq!(app.lyric_control.opacity(), 0, "当次显示按探针走");
        assert_eq!(
            app.prefs_snapshot().lyric_opacity,
            72,
            "落盘必须回落到原偏好值，不能被探针带跑"
        );
    }

    #[test]
    fn probe_forced_language_never_reaches_the_prefs_file() {
        // 场景：用户偏好中文，探针 REIMU_PROBE_LANG=en 强制英文做视觉校对。
        // 探针运行里任何一次落盘（改音质也会）都不得把强制值写进用户偏好。
        let mut app = test_app();
        app.pref_lang = Lang::Zh;
        app.lang = Lang::En;
        assert_eq!(app.prefs_snapshot().lang, Lang::Zh, "探针值不得进 prefs");

        app.apply_quality(QualityCeiling::Standard);
        assert_eq!(
            app.prefs_snapshot().lang,
            Lang::Zh,
            "改音质落盘不得带出探针语言"
        );

        // 用户主动切语言，偏好才跟着走
        app.apply_lang(Lang::Zh);
        assert_eq!(app.pref_lang, Lang::Zh);
    }

    #[test]
    fn forced_language_run_ignores_switches_and_prefs_writes() {
        // REIMU_PROBE_LANG = "本次运行钉死"：自动化截图运行里置顶窗口截获的
        // 误触点击不得切语言、不得弹 toast（污染截图）、不得落盘（污染 prefs.json
        // ——阶段 11 实测 prefs 被误触改写过两次）。
        let mut app = test_app();
        app.lang = Lang::En;
        app.pref_lang = Lang::Zh;
        app.apply_lang_impl(Lang::Zh, Some(Lang::En));
        assert_eq!(app.lang, Lang::En, "强制期间语言必须钉死");
        assert!(app.toast.is_none(), "强制期间切换不得弹提示");
        assert_eq!(app.pref_lang, Lang::Zh, "强制期间偏好不得被改写");

        let before = app.prefs_snapshot();
        app.persist_prefs_impl(false);
        assert_eq!(app.prefs_snapshot().quality, before.quality);
        assert_eq!(app.prefs_snapshot().lang, before.lang);
    }

    #[test]
    fn english_mode_drops_chinese_from_user_facing_text() {
        // 文案本地化最容易漏的地方：错误框与提示条
        let mut app = test_app();
        app.lang = Lang::En;
        app.handle_error("SEARCH FAILED", &ProviderError::RequestTimeout);
        let (title, message) = app.error_modal.clone().expect("硬错误应弹模态");
        assert_eq!(title, "SEARCH FAILED");
        for text in [&message, &title] {
            assert!(
                !text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "英文界面不得出现中文: {text}"
            );
        }
    }

    // ------------------------------------------------------------------
    // 阶段 12：中文 UI 全量汉化（默认中文 + 无遗漏硬编码）
    // ------------------------------------------------------------------

    /// 取一段文本里是否含汉字。
    fn has_cjk(text: &str) -> bool {
        text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
    }

    #[test]
    fn default_language_is_chinese_for_a_fresh_install() {
        // 首次启动（无 prefs.json）必须落在中文：老用户观感不能被升级改掉
        let app = test_app();
        assert_eq!(app.lang, Lang::Zh);
        assert_eq!(app.pref_lang, Lang::Zh);
        assert_eq!(super::super::prefs::Prefs::default().lang, Lang::Zh);
    }

    #[test]
    fn every_page_title_carries_the_brand() {
        // 改名时最容易漏的就是这五对字面量（`Page::title` 为了 &'static 不能拼接）
        for page in [
            Page::Player,
            Page::Search,
            Page::Rec,
            Page::Daily,
            Page::Playlist,
        ] {
            for lang in Lang::ALL {
                let title = page.title(lang);
                assert!(
                    title
                        .to_uppercase()
                        .starts_with(&crate::BRAND.to_uppercase()),
                    "{lang:?} 页面标题没带品牌：{title}"
                );
                assert!(
                    !title.to_lowercase().contains("sonora"),
                    "{lang:?} 页面标题还留着旧名：{title}"
                );
            }
        }
    }

    #[test]
    fn every_surface_pair_has_both_sides() {
        // 逐面盘点：中文侧必须真有汉字，英文侧不许有汉字，两侧不许同文。
        // 这份清单就是「哪些字会出现在界面上」的事实源，新增文案时漏了会红。
        let mut app = test_app();
        let mut pairs: Vec<(&str, String, String)> = Vec::new();
        for page in [
            Page::Player,
            Page::Search,
            Page::Rec,
            Page::Daily,
            Page::Playlist,
        ] {
            pairs.push((
                "page.title",
                page.title(Lang::Zh).into(),
                page.title(Lang::En).into(),
            ));
            pairs.push((
                "page.nav_label",
                page.nav_label(Lang::Zh).into(),
                page.nav_label(Lang::En).into(),
            ));
        }
        for key in [
            "SEARCH FAILED",
            "DETAIL FAILED",
            "PLAYLIST FAILED",
            "REC FAILED",
            "CHART FAILED",
            "DAILY FAILED",
            "PLAYBACK FAILED",
        ] {
            app.lang = Lang::Zh;
            let zh = app.error_title(key).to_string();
            app.lang = Lang::En;
            let en = app.error_title(key).to_string();
            pairs.push(("error_title", zh, en));
        }
        for (what, zh, en) in &pairs {
            // FM 页签是通用缩写，两种语言同文且无汉字，属有意保留
            let is_fm_nav = *what == "page.nav_label" && zh == "FM";
            if !is_fm_nav {
                assert!(has_cjk(zh), "{what}: 中文侧没有汉字（`{zh}`）");
                assert_ne!(zh, en, "{what}: 两侧同文（`{zh}`）");
            }
            assert!(!has_cjk(en), "{what}: 英文侧残留中文（`{en}`）");
        }
    }

    #[test]
    fn playback_status_line_speaks_both_languages() {
        // 状态行挂在 Track Info 旁边，是最常看的字，必须跟语言
        use crate::gui::player_core::CoreStatus;
        let cases = [
            (CoreStatus::Standby, "待机", "STANDBY"),
            (CoreStatus::FetchingUrl, "缓冲", "BUFFERING"),
            (
                CoreStatus::PlayingLocal,
                "播放中（本地）",
                "PLAYING (LOCAL)",
            ),
            (
                CoreStatus::PlayingOnline,
                "播放中（在线）",
                "PLAYING (ONLINE)",
            ),
            (CoreStatus::Paused, "已暂停", "PAUSED"),
            (CoreStatus::Stopped, "已停止", "STOPPED"),
            (CoreStatus::Cancelled, "已取消", "CANCELLED"),
            (CoreStatus::Ended, "已播完", "ENDED"),
            (CoreStatus::Failed, "失败", "FAILED"),
            (CoreStatus::FailedWith("io".into()), "失败：", "FAILED / "),
        ];
        for (status, zh_part, en_part) in cases {
            let zh = status.text(Lang::Zh);
            let en = status.text(Lang::En);
            assert!(zh.contains(zh_part), "中文态 `{zh}` 应含 `{zh_part}`");
            assert!(en.contains(en_part), "英文态 `{en}` 应含 `{en_part}`");
            assert!(!has_cjk(&en), "英文态状态行残留中文: {en}");
        }
    }

    #[test]
    fn quality_and_share_labels_follow_the_language() {
        // 这两个是「方法返回固定串」的经典盲区：面板按钮和提示语都靠它
        for level in QualityCeiling::ALL {
            let zh = level.label(Lang::Zh);
            assert!(has_cjk(zh), "音质档中文名没汉字: {zh}");
            assert_eq!(
                zh,
                match level {
                    QualityCeiling::Standard => "标准",
                    QualityCeiling::Higher => "较高",
                    QualityCeiling::ExtraHigh => "极高",
                    QualityCeiling::Lossless => "无损",
                }
            );
            // 持久化键与语言无关：切语言不能让用户偏好读坏
            assert_eq!(level.code(), level.label(Lang::En));
        }
        for kind in [
            crate::music::link::ShareKind::Song,
            crate::music::link::ShareKind::Playlist,
            crate::music::link::ShareKind::Album,
        ] {
            assert!(has_cjk(kind.label(Lang::Zh)), "分享类型中文名没汉字");
            assert!(!has_cjk(kind.label(Lang::En)), "分享类型英文名残留中文");
        }
    }

    #[test]
    fn library_save_errors_are_localised() {
        // 曲库落盘失败会经 toast 显示；写死中文在英文界面就是漏字点
        use crate::gui::library::SaveError;
        let cases = [
            SaveError::NoConfigDir,
            SaveError::Write {
                path: "x.json".into(),
                detail: "denied".into(),
            },
            SaveError::Rename {
                path: "x.json".into(),
                detail: "denied".into(),
            },
            SaveError::Remove {
                path: "x.json".into(),
                detail: "denied".into(),
            },
            SaveError::CreateDir {
                path: "dir".into(),
                detail: "denied".into(),
            },
        ];
        for err in cases {
            let zh = err.message(Lang::Zh);
            let en = err.message(Lang::En);
            assert!(has_cjk(&zh), "中文态错误没汉字: {zh}");
            assert!(!has_cjk(&en), "英文态错误残留中文: {en}");
        }
    }

    #[test]
    fn prefs_language_survives_a_write_read_roundtrip() {
        // 切换后重启要回到用户选的语言；没有 lang 字段（阶段 10 老文件）回中文
        let dir = temp_dir("prefs-lang");
        let path = dir.join(super::super::prefs::PREFS_FILE);
        let mut prefs = super::super::prefs::Prefs::default();
        assert_eq!(prefs.lang, Lang::Zh, "默认必须中文");
        prefs.lang = Lang::En;
        super::super::prefs::save_to(&dir, &prefs).expect("落盘");
        let back = super::super::prefs::load_from(&dir);
        assert_eq!(back.lang, Lang::En);
        assert_eq!(back.quality, prefs.quality);

        // 老格式（只有 version + quality）→ 语言回中文
        std::fs::write(
            &path,
            format!(
                "{{\n  \"version\": {},\n  \"quality\": \"LOSSLESS\"\n}}\n",
                super::super::prefs::PREFS_VERSION
            ),
        )
        .unwrap();
        let legacy = super::super::prefs::load_from(&dir);
        assert_eq!(legacy.lang, Lang::Zh, "缺 lang 字段必须回中文");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------------
    // 阶段 12：长中文文案的排版（真实 egui 上下文实测宽度，不靠肉眼）
    // ------------------------------------------------------------------

    /// 用真实字体量一段文案的排版宽度。
    fn text_width(text: &str, size: f32, tracking: f32) -> f32 {
        let ctx = eframe::egui::Context::default();
        super::super::theme::setup_fonts(&ctx);
        let mut w = 0.0;
        let raw = eframe::egui::RawInput {
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(900.0, 620.0))),
            ..eframe::egui::RawInput::default()
        };
        let out = ctx.run_ui(raw, |ui| {
            let job = widgets::spec_job(text, size, tracking, pal().ink, false);
            w = ui.ctx().fonts_mut(|f| f.layout_job(job)).size().x;
        });
        out.drop_without_applying_deltas();
        w
    }

    #[test]
    fn nav_row_fits_the_minimum_window_in_every_language() {
        // 五个页签整排要能塞进最小窗口宽（900）。英文要是能过，中文更短，
        // 但中文必须逐个量一遍——文案改了没人能凭肉眼发现越界。
        for lang in Lang::ALL {
            let pages = [
                Page::Player,
                Page::Search,
                Page::Rec,
                Page::Daily,
                Page::Playlist,
            ];
            let total: f32 = pages
                .iter()
                .map(|p| text_width(&p.nav_label(lang).to_uppercase(), FONT_TINY, 0.12))
                .sum();
            // NAV_BUTTON_PAD 是每个按钮的内边距；留 2×24px 给窗口两侧
            let used = total
                + widgets::NAV_BUTTON_PAD * pages.len() as f32
                + NAV_GAP * (pages.len() - 1) as f32;
            assert!(
                used < 900.0 - 48.0,
                "lang={lang:?} 页签整排 {used:.1}px 塞不进最小窗口宽"
            );
        }
    }

    #[test]
    fn chinese_labels_fit_the_widths_they_are_given() {
        // 「中文比英文短」不是普适规律（「确定」比 OK 宽），真正的约束是
        // **塞得进所在的固定宽容器**。这里量三处有硬宽度的容器。
        use super::settings;

        // ① 设置行标签列：超了会压到右侧按钮上
        for label in ["账户", "语言", "音质", "桌面歌词"] {
            let w = text_width(label, FONT_TINY, TRACKING_TINY);
            assert!(
                w <= settings::LABEL_W - 4.0,
                "设置标签「{label}」宽 {w:.1}px 超过标签列 {}px",
                settings::LABEL_W
            );
        }

        // ② 音质四档连排 + 标签列：右列在最小窗口（900）下的可用宽度约 290px
        let mut row_w = settings::LABEL_W;
        for level in QualityCeiling::ALL {
            row_w += text_width(level.label(Lang::Zh), FONT_TINY, TRACKING_TINY) + 12.0 + 4.0;
        }
        assert!(
            row_w <= 290.0,
            "音质行中文 {row_w:.1}px 超过右列可用宽度（约 290px）"
        );

        // ③ 错误模态的确认键（按钮固定 54px）
        let ok = text_width("确定", FONT_LABEL, TRACKING_LABEL);
        assert!(
            ok <= 54.0 - 8.0,
            "「确定」宽 {ok:.1}px 会撑出 54px 的确认键"
        );
    }

    // ------------------------------------------------------------------
    // 阶段 14：用户数据（历史 / 收藏 / 自建歌单）与离线推荐接线
    // ------------------------------------------------------------------

    /// 造一份含 `n` 个音频文件的临时曲库（只建空文件，扫描按扩展名认）。
    fn app_with_library(case: &str, names: &[&str]) -> (ReimuApp, std::path::PathBuf) {
        let dir = temp_dir(case);
        for name in names {
            std::fs::write(dir.join(name), b"").expect("写测试文件");
        }
        let mut app = test_app();
        app.apply_library_dir(&dir, false);
        (app, dir)
    }

    #[test]
    fn play_track_marks_current_and_resets_history_flag() {
        let mut app = test_app();
        app.history_marked = true;
        app.play_track(sample_track("42"));
        assert_eq!(
            app.current_track.as_ref().map(|t| t.id.as_str()),
            Some("42"),
            "换曲必须刷新当前曲目快照"
        );
        assert!(!app.history_marked, "换曲后应允许重新记一次历史");
    }

    #[test]
    fn local_play_also_records_a_snapshot() {
        let mut app = test_app();
        app.play_local_path(Path::new("Q:\\missing\\song.mp3"));
        let track = app.current_track.clone().expect("本地播放也要有快照");
        assert_eq!(track.provider, ProviderId::Local);
        assert!(track.id.ends_with("song.mp3"));
    }

    #[test]
    fn history_not_recorded_before_qualifying() {
        let mut app = test_app();
        app.play_track(sample_track("1"));
        // 没有引擎 → 进度恒 0，远不到「听够」门槛
        app.tick_history();
        assert!(app.history.is_empty(), "没听够不该进历史");
        // 已标记后即便再 tick 也不会重复记
        app.history_marked = true;
        app.tick_history();
        assert!(app.history.is_empty());
    }

    #[test]
    fn toggle_favorite_round_trips() {
        let mut app = test_app();
        let track = sample_track("7");
        assert!(!app.favorites.contains(&track));
        assert!(app.toggle_favorite(&track), "第一次点击应加入收藏");
        assert!(app.favorites.contains(&track));
        assert!(!app.toggle_favorite(&track), "第二次点击应取消收藏");
        assert!(!app.favorites.contains(&track));
    }

    #[test]
    fn selecting_favorites_view_switches_current_tracks() {
        let mut app = test_app();
        app.favorites.toggle(&sample_track("a"), 0);
        app.favorites.toggle(&sample_track("b"), 0);
        app.select_user_view(UserView::Favorites);
        assert_eq!(app.current_tracks().len(), 2);
        assert_eq!(app.selected, Some(0), "进入视图应默认选中第一行");
        assert!(!app.active_local, "用户视图与本地曲库互斥");
        // 离开视图后回到在线结果
        app.clear_user_view();
        assert!(app.current_tracks().is_empty());
    }

    #[test]
    fn unfavoriting_inside_favorites_view_shrinks_the_list() {
        let mut app = test_app();
        let a = sample_track("a");
        app.favorites.toggle(&a, 0);
        app.favorites.toggle(&sample_track("b"), 0);
        app.select_user_view(UserView::Favorites);
        assert_eq!(app.current_tracks().len(), 2);
        app.toggle_favorite(&a);
        assert_eq!(app.current_tracks().len(), 1, "取消收藏后该行应立刻消失");
    }

    #[test]
    fn local_search_fills_hits_and_highlight() {
        let (mut app, dir) = app_with_library("search-hit", &["City Pop.mp3", "Night Drive.mp3"]);
        app.run_local_search("city");
        assert_eq!(app.local_hits.len(), 1, "只应命中 City Pop");
        assert_eq!(app.highlight_query, "city");
        // 空关键词清空命中（清空搜索框不该还挂着结果）
        app.run_local_search("   ");
        assert!(app.local_hits.is_empty());
        assert!(app.highlight_query.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_search_without_library_yields_nothing() {
        let mut app = test_app();
        app.run_local_search("anything");
        assert!(app.local_hits.is_empty());
        assert_eq!(app.highlight_query, "anything", "关键词仍应记录用于高亮");
    }

    #[test]
    fn local_playlist_crud_through_app() {
        let mut app = test_app();
        app.new_playlist_name = "My Mix".into();
        app.create_local_playlist();
        assert_eq!(app.local_playlists.lists.len(), 1);
        let id = app.local_playlists.lists[0].id;
        assert_eq!(app.user_view, UserView::LocalPlaylist(id));
        assert!(app.new_playlist_name.is_empty(), "新建后应清空输入框");

        app.add_to_local_playlist(id, &sample_track("1"));
        app.add_to_local_playlist(id, &sample_track("2"));
        assert_eq!(app.current_tracks().len(), 2);

        // 上移：选中第 2 行后上移，顺序应交换
        app.selected = Some(1);
        app.move_selected_in_playlist(-1);
        assert_eq!(app.selected, Some(0));
        let titles: Vec<&str> = app
            .current_tracks()
            .iter()
            .map(|t| t.title.as_str())
            .collect();
        assert_eq!(titles, vec!["Track 2", "Track 1"]);

        // 移除选中
        app.selected = Some(0);
        app.remove_selected_from_playlist();
        assert_eq!(app.current_tracks().len(), 1);

        // 删除走模态确认：请求阶段只弹确认框，确认后才真删
        app.request_delete_local_playlist(id);
        assert_eq!(app.local_playlists.lists.len(), 1, "确认前不应删除");
        let action = app.confirm.take().expect("应弹出确认框").action;
        app.apply_confirm(action);
        assert!(app.local_playlists.lists.is_empty(), "确认后才真删");
        assert_eq!(app.user_view, UserView::None, "删掉当前视图后应退出该视图");
    }

    #[test]
    fn duplicate_local_playlist_name_is_rejected() {
        let mut app = test_app();
        app.new_playlist_name = "Mix".into();
        app.create_local_playlist();
        app.new_playlist_name = " mix ".into();
        app.create_local_playlist();
        assert_eq!(
            app.local_playlists.lists.len(),
            1,
            "同名（忽略大小写）应被拒绝"
        );
    }

    #[test]
    fn account_playlist_does_not_duplicate_as_a_loaded_card() {
        // 回归：点开一张账号歌单后，历史里会再画一张同名「已载入」卡，
        // 看起来像"点一下又新建了一个同名歌单"。
        let mut app = test_app();
        app.user_playlists.push(PlaylistSummary {
            provider: ProviderId::Netease,
            id: "cloud-1".into(),
            name: "东方".into(),
            cover_url: None,
            track_count: Some(2),
            play_count: None,
            description: None,
        });
        app.remember_loaded_playlist(Playlist {
            provider: ProviderId::Netease,
            id: "cloud-1".into(),
            name: Some("东方".into()),
            total: Some(2),
            tracks: vec![sample_track("a"), sample_track("b")],
        });
        // 数据仍要保留：右侧曲目列表与播放都靠它
        assert!(
            app.playlists.iter().any(|p| p.id == "cloud-1"),
            "账号歌单的曲目数据不能丢"
        );
        assert!(
            !app.loaded_card_visible("cloud-1"),
            "账号区已有这张卡，历史区不能再画一张同名卡"
        );
        assert!(app.loaded_card_visible("not-in-account"));
    }

    #[test]
    fn loaded_playlist_updates_in_place_and_is_capped() {
        let mut app = test_app();
        let mk = |id: &str| Playlist {
            provider: ProviderId::Netease,
            id: id.to_string(),
            name: Some(format!("P{id}")),
            total: Some(1),
            tracks: vec![sample_track(id)],
        };
        app.remember_loaded_playlist(mk("a"));
        app.remember_loaded_playlist(mk("b"));
        app.remember_loaded_playlist(mk("a"));
        assert_eq!(app.playlists.len(), 2, "同 ID 应原地更新而不是追加");
        assert_eq!(app.playlists[0].id, "a", "已载入的歌单位置不应被挪到末尾");

        for i in 0..20 {
            app.remember_loaded_playlist(mk(&format!("n{i}")));
        }
        assert_eq!(
            app.playlists.len(),
            PLAYLIST_HISTORY_LIMIT,
            "历史必须有容量上限"
        );
        assert_eq!(
            app.playlists.last().map(|p| p.id.as_str()),
            Some("n19"),
            "最新载入的歌单不能被淘汰"
        );
    }

    #[test]
    fn rec_peek_opens_in_place_without_touching_loaded_playlists() {
        // 回归：点推荐卡曾直接走载入歌单的路径，歌单页因此多出一张同名卡。
        let mut app = test_app();
        let item = PlaylistSummary {
            provider: ProviderId::Netease,
            id: "rec-1".into(),
            name: "东方精选".into(),
            cover_url: None,
            track_count: Some(12),
            play_count: None,
            description: None,
        };
        app.open_playlist_peek(PeekPanel::Rec, &item);

        assert!(app.playlists.is_empty(), "原地展开不得写进已载入历史");
        assert!(app.playlist_state.is_idle(), "不该借用歌单页的载入状态");
        assert!(app.active_playlist.is_none());
        let peek = app.rec_peek.as_ref().expect("应在推荐面板原地展开");
        assert!(peek.expanded);
        assert!(peek.state.is_loading());
        assert_eq!(peek.id, "rec-1");
        assert!(peek.subtitle.contains("东方精选"));
        assert!(peek.subtitle.contains("12"));
        assert!(app.chart_peek.is_none(), "两个面板各自独立");

        // 返回 = 收起详情，推荐列表原样回来
        app.close_peek(PeekPanel::Rec);
        assert!(!app.peek_open(PeekPanel::Rec));
    }

    #[test]
    fn chart_menu_play_uses_the_chart_as_queue() {
        // 热榜行右键 PLAY 必须和双击/▶ 同语义：以榜单为队列，
        // 而不是把当前浏览列表（这里是搜索结果）顶进队列。
        use crate::gui::context_menu::MenuAction;
        let mut app = test_app();
        let a = sample_track("a");
        let b = sample_track("b");
        app.search_state
            .finish(Ok(vec![sample_track("search-only")]));
        app.queue = vec![sample_track("old")];
        app.queue_active = true;

        app.menu_track = Some(b.clone());
        app.menu_chart = Some(vec![a.clone(), b.clone()]);
        app.apply_menu_action(MenuAction::Play);

        assert_eq!(
            app.queue.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"],
            "队列应来自榜单快照"
        );
        assert_eq!(app.selected, Some(1), "选中行应指向榜单里的位置");
        assert_eq!(app.current_track.as_ref().map(|t| t.id.as_str()), Some("b"));
        assert!(app.menu_chart.is_none(), "动作执行后快照应被清掉");
    }

    #[test]
    fn peek_seq_match_routes_each_panel_to_its_own_seq() {
        // 两个面板的请求可能同时在飞，响应只能落进发起请求的那个面板。
        let mut app = test_app();
        let item = |id: &str| PlaylistSummary {
            provider: ProviderId::Netease,
            id: id.into(),
            name: format!("P{id}"),
            cover_url: None,
            track_count: Some(1),
            play_count: None,
            description: None,
        };
        app.open_playlist_peek(PeekPanel::Rec, &item("r"));
        app.open_playlist_peek(PeekPanel::Chart, &item("c"));

        let rec_seq = app.rec_peek.as_ref().unwrap().seq;
        let chart_seq = app.chart_peek.as_ref().unwrap().seq;
        assert_eq!(app.peek_seq_match(rec_seq), Some(PeekPanel::Rec));
        assert_eq!(app.peek_seq_match(chart_seq), Some(PeekPanel::Chart));
        assert_eq!(
            app.peek_seq_match(chart_seq + 1000),
            None,
            "陌生 seq 不归任何面板"
        );
    }

    #[test]
    fn peek_reuses_loaded_tracks_and_refetches_a_different_playlist() {
        let mut app = test_app();
        let item = |id: &str| PlaylistSummary {
            provider: ProviderId::Netease,
            id: id.into(),
            name: format!("P{id}"),
            cover_url: None,
            track_count: Some(1),
            play_count: None,
            description: None,
        };
        app.open_playlist_peek(PeekPanel::Chart, &item("a"));
        let seq = app.chart_peek.as_ref().unwrap().seq;
        app.chart_peek.as_mut().unwrap().state.finish(Ok(Playlist {
            provider: ProviderId::Netease,
            id: "a".into(),
            name: Some("Pa".into()),
            total: Some(1),
            tracks: vec![sample_track("a")],
        }));

        app.close_peek(PeekPanel::Chart);
        app.open_playlist_peek(PeekPanel::Chart, &item("a"));
        assert_eq!(
            app.chart_peek.as_ref().unwrap().seq,
            seq,
            "同一张歌单已取到曲目，再点开不该重发请求"
        );
        assert!(app.chart_peek.as_ref().unwrap().expanded);

        app.close_peek(PeekPanel::Chart);
        app.open_playlist_peek(PeekPanel::Chart, &item("b"));
        assert_ne!(
            app.chart_peek.as_ref().unwrap().seq,
            seq,
            "换一张歌单必须重新取曲目"
        );
    }

    #[test]
    fn peek_track_play_uses_the_peeked_playlist_as_queue() {
        let mut app = test_app();
        let item = PlaylistSummary {
            provider: ProviderId::Netease,
            id: "chart-1".into(),
            name: "热榜".into(),
            cover_url: None,
            track_count: Some(2),
            play_count: None,
            description: None,
        };
        app.open_playlist_peek(PeekPanel::Chart, &item);
        app.chart_peek.as_mut().unwrap().state.finish(Ok(Playlist {
            provider: ProviderId::Netease,
            id: "chart-1".into(),
            name: Some("热榜".into()),
            total: Some(2),
            tracks: vec![sample_track("x"), sample_track("y")],
        }));
        app.play_peek_track(PeekPanel::Chart, 1);

        assert!(app.queue_active);
        assert_eq!(
            app.queue.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["x", "y"],
            "队列应来自原地详情的曲目，而不是界面其它列表"
        );
        assert_eq!(app.current_track.as_ref().map(|t| t.id.as_str()), Some("y"));
        assert_eq!(app.page, Page::Player);
    }

    #[test]
    fn shuffle_prev_walks_back_more_than_once() {
        let mut app = test_app();
        app.queue = (0..5).map(|i| sample_track(&i.to_string())).collect();
        app.queue_active = true;
        app.core.shuffle = true;
        app.ensure_shuffle_order();
        let order = app.shuffle_order.clone().expect("应有洗牌序列");
        let start = app.shuffle_pos;
        let len = app.playback_tracks().len();
        let first = app.prev_index().expect("应能回退");
        assert_eq!(first, order[(start + len - 1) % len]);
        let second = app.prev_index().expect("应能继续回退");
        assert_eq!(second, order[(start + len - 2) % len]);
        assert_ne!(first, second, "连按上一曲必须真的往回走");
        let forward = app.next_index().expect("应能前进");
        assert_eq!(forward, first, "next 与 prev 应互为逆操作");
    }

    #[test]
    fn empty_playlist_name_is_rejected() {
        let mut app = test_app();
        app.new_playlist_name = "   ".into();
        app.create_local_playlist();
        assert!(app.local_playlists.lists.is_empty());
        assert!(app.toast.is_some(), "应给出提示而不是静默失败");
    }

    #[test]
    fn rename_local_playlist_through_app() {
        let mut app = test_app();
        app.new_playlist_name = "Old".into();
        app.create_local_playlist();
        let id = app.local_playlists.lists[0].id;
        app.rename_target = Some(id);
        app.rename_input = "New".into();
        app.commit_rename_playlist();
        assert_eq!(app.local_playlists.get(id).unwrap().name, "New");
        assert!(app.rename_target.is_none(), "提交后应退出重命名态");
    }

    #[test]
    fn history_window_switch_refilters_view() {
        let mut app = test_app();
        let now = crate::gui::history::now_secs();
        // 2 天前听过的一首
        app.history
            .record(&sample_track("old"), 40_000, now - 2 * 86_400);
        app.select_user_view(UserView::History);
        assert_eq!(
            app.current_tracks().len(),
            0,
            "24h 窗口内不该出现 2 天前的记录"
        );
        app.set_history_window(720);
        assert_eq!(app.current_tracks().len(), 1, "切到 30 天后应出现");
    }

    #[test]
    fn offline_daily_picks_from_library() {
        let (mut app, dir) = app_with_library("daily-off", &["a.mp3", "b.mp3", "c.mp3"]);
        app.rebuild_daily_offline();
        assert!(app.daily_offline);
        assert_eq!(app.daily_local.len(), 3);
        // 同一天重复计算必须得到同一批（确定性）
        let first: Vec<String> = app.daily_local.iter().map(|t| t.id.clone()).collect();
        app.rebuild_daily_offline();
        let second: Vec<String> = app.daily_local.iter().map(|t| t.id.clone()).collect();
        assert_eq!(first, second, "离线每日推荐必须同一天同结果");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn offline_daily_empty_without_library() {
        let mut app = test_app();
        app.rebuild_daily_offline();
        assert!(app.daily_local.is_empty());
        assert!(!app.daily_offline);
    }

    #[test]
    fn media_keys_toggle_controls_listener_lifetime() {
        let mut app = test_app();
        app.media_keys_enabled = false;
        app.start_media_keys();
        assert!(app.media_keys.is_none(), "关掉开关时不该注册热键");
        app.media_keys_enabled = true;
        app.start_media_keys();
        // 平台上可能注册失败，但监听器对象本身一定已建立
        assert!(app.media_keys.is_some());
        app.stop_media_keys();
        assert!(app.media_keys.is_none());
    }

    #[test]
    fn applying_library_dir_clears_user_view() {
        let (mut app, dir) = app_with_library("clear-view", &["a.mp3"]);
        app.favorites.toggle(&sample_track("x"), 0);
        app.select_user_view(UserView::Favorites);
        assert_eq!(app.user_view, UserView::Favorites);
        app.apply_library_dir(&dir, false);
        assert_eq!(app.user_view, UserView::None, "装载本地曲库应退出用户视图");
        assert_eq!(app.current_tracks().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------------
    // 阶段 14：新增界面路径的渲染冒烟（真实 egui 上下文，抓 panic/借用冲突）
    // ------------------------------------------------------------------

    /// 在真实 egui 上下文里跑一次绘制，返回是否成功跑完。
    fn render_once(app: &mut ReimuApp, draw: impl FnOnce(&mut ReimuApp, &mut Ui)) {
        let ctx = eframe::egui::Context::default();
        super::super::theme::setup_fonts(&ctx);
        let raw = eframe::egui::RawInput {
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(900.0, 620.0))),
            ..eframe::egui::RawInput::default()
        };
        // `run_ui` 的回调是 `FnMut`，而 `FnOnce` 只能消费一次 → 用 Option 兜住
        let mut draw = Some(draw);
        let out = ctx.run_ui(raw, |ui| {
            if let Some(f) = draw.take() {
                f(app, ui);
            }
        });
        out.drop_without_applying_deltas();
    }

    #[test]
    fn playlist_page_renders_in_every_user_view() {
        let mut app = test_app();
        app.favorites.toggle(&sample_track("a"), 0);
        app.new_playlist_name = "Mix".into();
        app.create_local_playlist();
        let id = app.local_playlists.lists[0].id;
        app.add_to_local_playlist(id, &sample_track("b"));
        for view in [
            UserView::None,
            UserView::Favorites,
            UserView::History,
            UserView::LocalPlaylist(id),
        ] {
            app.select_user_view(view);
            render_once(&mut app, |app, ui| app.ui_page_playlist(ui));
        }
        // 重命名态也要能画（输入框 + 确定/取消）
        app.rename_target = Some(id);
        render_once(&mut app, |app, ui| app.ui_page_playlist(ui));
    }

    #[test]
    fn playlist_page_keeps_create_row_inside_the_panel() {
        // 回归：滚动区若不预留底部固定行高度，会把「新歌单名称 / 新建」
        // 顶出面板，窗口底部直接裁掉。这里用足够多的歌单把滚动区撑满。
        let mut app = test_app();
        for i in 0..30 {
            app.new_playlist_name = format!("List {i}");
            app.create_local_playlist();
        }
        let screen_h = 620.0;
        let mut content_bottom = 0.0_f32;
        render_once(&mut app, |app, ui| {
            app.ui_page_playlist(ui);
            content_bottom = ui.min_rect().max.y;
        });
        assert!(
            content_bottom <= screen_h + 1.0,
            "歌单页内容底边 {content_bottom} 超出窗口 {screen_h}"
        );
    }

    #[test]
    fn search_page_renders_with_local_hits() {
        let (mut app, dir) = app_with_library("smoke-search", &["City Pop.mp3", "Night.mp3"]);
        app.run_local_search("city");
        render_once(&mut app, |app, ui| app.ui_page_search(ui));
        // 无本地命中的空态也要能画
        app.run_local_search("zzz");
        render_once(&mut app, |app, ui| app.ui_page_search(ui));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn daily_page_renders_offline_fallback() {
        let (mut app, dir) = app_with_library("smoke-daily", &["a.mp3", "b.mp3"]);
        app.rebuild_daily_offline();
        render_once(&mut app, |app, ui| app.ui_page_daily(ui));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn long_chinese_hints_stay_on_one_line_where_the_box_is_wide() {
        // 磁带架空态、「— 载入失败 —」这类居中提示如果比容器还宽会被硬裁。
        // 这里量的是"文案本身有没有长到必须换行"：磁带架在最小窗口下内宽
        // 约 250px，超过就必须靠 fit_text/wrap_to 兜底（两处调用都已接上），
        // 所以真正要钉的是**别超过它们的截断阈值还不走兜底**。
        let cases = [
            ("— 载入失败，点重载重试 —", 250.0),
            ("输入关键词搜索，或装载一个歌单", 250.0),
            ("— 暂无推荐 —", 250.0),
        ];
        for (text, budget) in cases {
            let w = self::tests::text_width(text, FONT_TINY, TRACKING_TINY);
            assert!(w > 0.0, "「{text}」必须有可测宽度");
            // 要么装得下，要么它所在的绘制点已经接 fit_text/wrap_to（本文件已接）
            if w > budget {
                assert!(
                    w < budget * 4.0,
                    "「{text}」宽 {w:.1}px 远超容器 {budget:.0}px，换行也救不回来"
                );
            }
        }
    }

    // ------------------------------------------------------------------
    // 收藏双向同步（纯逻辑）
    // ------------------------------------------------------------------

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn local_set(v: &[&str]) -> std::collections::HashSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn sync_plan_pulls_cloud_only_and_pushes_local_only() {
        // 最核心的一条：两个方向**不能**搞反。搞反的后果是"本地把云端没有的
        // 歌删掉"，界面上看不出来，下次登录才发现收藏少了。
        let cloud = ids(&["1", "2", "3"]);
        let local = local_set(&["3", "4"]);
        let (pull, push) = favorite_sync_plan(&cloud, &local);
        assert_eq!(pull, ids(&["1", "2"]), "云端独有要补进本地");
        assert_eq!(push, ids(&["4"]), "本地独有要推到云端");
    }

    #[test]
    fn sync_plan_is_empty_when_both_sides_agree() {
        let cloud = ids(&["7", "8"]);
        let local = local_set(&["7", "8"]);
        let (pull, push) = favorite_sync_plan(&cloud, &local);
        assert!(pull.is_empty() && push.is_empty());
    }

    #[test]
    fn sync_plan_handles_one_empty_side() {
        // 新账号：云端空、本地一堆 → 全部推送，不能"清空本地"
        let (pull, push) = favorite_sync_plan(&[], &local_set(&["a", "b"]));
        assert!(pull.is_empty());
        assert_eq!(push, ids(&["a", "b"]));
        // 全新机器：本地空、云端一堆 → 全部拉取
        let (pull, push) = favorite_sync_plan(&ids(&["x"]), &local_set(&[]));
        assert_eq!(pull, ids(&["x"]));
        assert!(push.is_empty());
    }

    #[test]
    fn sync_plan_push_order_is_deterministic() {
        // HashSet 迭代序随机：不排序的话同样的输入会给出不同顺序，
        // 日志与"失败重试"都变得不可复现
        let local = local_set(&["c", "a", "b"]);
        let (_, first) = favorite_sync_plan(&[], &local);
        let (_, second) = favorite_sync_plan(&[], &local);
        assert_eq!(first, second);
        assert_eq!(first, ids(&["a", "b", "c"]));
    }

    #[test]
    fn primary_artist_id_skips_blank_slots() {
        // `artist_ids` 与 `artists` 同下标，缺失位是空串：第一位没 ID 时要跳到
        // 第二位，而不是"空 ID → 打开一个空歌手页"
        let mut track = Track {
            id: "1".to_string(),
            title: "T".to_string(),
            artists: vec!["甲".to_string(), "乙".to_string()],
            artist_ids: vec![String::new(), "42".to_string()],
            ..Track::default()
        };
        assert_eq!(primary_artist_id(&track), Some("42"));
        track.artist_ids = vec![String::new(), String::new()];
        assert_eq!(primary_artist_id(&track), None, "全空时不得伪造 ID");
    }

    #[test]
    fn shelf_titles_switch_and_back_restores_the_previous_source() {
        // 「点专辑名 → 标题变专辑曲子 → 返回 → 回到原来那份列表」整条链路，
        // 只走状态机（不发网络请求：seq 由 bridge 分配，测试里不会真发）
        let mut app = test_app();
        // 用搜索结果当"原来的磁带架"（非空，才能验选中行恢复）
        app.search_state.finish(Ok(vec![
            Track {
                id: "a".to_string(),
                title: "A".to_string(),
                ..Track::default()
            },
            Track {
                id: "b".to_string(),
                title: "B".to_string(),
                ..Track::default()
            },
            Track {
                id: "c".to_string(),
                title: "C".to_string(),
                ..Track::default()
            },
        ]));
        app.selected = Some(1);
        app.open_album_page("123", "某专辑");
        assert_eq!(app.user_view, UserView::Album);
        assert_eq!(app.shelf_title(), "专辑曲子");
        assert_eq!(
            app.shelf_target.as_ref().map(|t| t.id.as_str()),
            Some("123")
        );
        app.open_artist_page("456", "某歌手");
        assert_eq!(app.shelf_title(), "歌手曲子");
        // 专辑 → 歌手 的连续跳转不该覆盖最初快照
        app.back_to_shelf();
        assert_eq!(app.shelf_target, None);
        assert_eq!(app.shelf_title(), "磁带架");
        assert_eq!(app.user_view, UserView::None, "返回后要回到原来的来源");
        assert_eq!(app.selected, Some(1), "返回后要回到原来选中的那一行");
        assert_eq!(app.current_tracks().len(), 3, "原来的列表要原样回来");
    }

    #[test]
    fn opening_album_without_id_is_refused() {
        // 本地曲目没有专辑 ID：点它不该把磁带架切成一个永远空的「专辑曲子」
        let mut app = test_app();
        app.open_album_page("   ", "无 ID");
        assert_eq!(app.user_view, UserView::None);
        assert_eq!(app.shelf_title(), "磁带架");
    }

    #[test]
    fn track_menu_offers_navigation_only_when_ids_exist() {
        use super::super::context_menu::{MenuAction, MenuEntry};
        let app = test_app();
        let has = |entries: &[MenuEntry], want: MenuAction| {
            entries
                .iter()
                .any(|e| matches!(e, MenuEntry::Item { action, .. } if *action == want))
        };

        // 本地曲目：没有专辑/艺人 ID → 不给两个跳转入口（点开只会是空页）
        let local = Track {
            provider: ProviderId::Local,
            id: "E:/music/a.mp3".to_string(),
            title: "本地".to_string(),
            ..Track::default()
        };
        let entries = app.track_menu_entries(&local);
        assert!(has(&entries, MenuAction::Play));
        assert!(has(&entries, MenuAction::ToggleFavorite));
        assert!(
            !has(&entries, MenuAction::GoToAlbum),
            "本地曲目不该有专辑入口"
        );
        assert!(
            !has(&entries, MenuAction::GoToArtist),
            "本地曲目不该有歌手入口"
        );

        // 在线曲目带 ID：两个跳转都要有
        let online = Track {
            provider: ProviderId::Netease,
            id: "123".to_string(),
            title: "在线".to_string(),
            artists: vec!["某人".to_string()],
            album_id: Some("456".to_string()),
            artist_ids: vec!["789".to_string()],
            ..Track::default()
        };
        let entries = app.track_menu_entries(&online);
        assert!(has(&entries, MenuAction::GoToAlbum));
        assert!(has(&entries, MenuAction::GoToArtist));
        // 没有自建歌单时也要有"加入歌单"分组（灰显提示），不能整段消失
        assert!(entries.iter().any(|e| matches!(e, MenuEntry::Heading(_))));
    }

    #[test]
    fn track_menu_only_offers_removal_inside_a_local_playlist() {
        use super::super::context_menu::{MenuAction, MenuEntry};
        let mut app = test_app();
        let track = Track {
            provider: ProviderId::Netease,
            id: "1".to_string(),
            title: "T".to_string(),
            ..Track::default()
        };
        let has_remove = |app: &ReimuApp| {
            app.track_menu_entries(&track).iter().any(|e| {
                matches!(
                    e,
                    MenuEntry::Item {
                        action: MenuAction::RemoveFromPlaylist,
                        ..
                    }
                )
            })
        };
        // 不在自建歌单里 → 不给"从歌单移除"（否则点了什么也不会发生）
        assert!(!has_remove(&app));
        let id = app
            .local_playlists
            .create("测试歌单", super::super::history::now_secs());
        app.user_view = UserView::LocalPlaylist(id);
        assert!(has_remove(&app));
    }

    // ------------------------------------------------------------------
    // 心动模式（playmode_intelligence_list）
    // ------------------------------------------------------------------

    /// 让 `search_state` 里有一份可播放列表，并以第 `at` 首为当前曲。
    ///
    /// 走真实的 [`ReimuApp::play_track`]：它同时写 `current_track`（应用侧快照）
    /// 和磁带面板，两者一起才有"正在播"这件事。
    fn app_with_queue(app: &mut ReimuApp, ids: &[&str], at: usize) {
        app.active_local = false;
        app.active_playlist = None;
        app.search_state
            .finish(Ok(ids.iter().map(|i| sample_track(i)).collect()));
        app.selected = Some(at);
        app.adopt_queue();
        app.play_track(sample_track(ids[at]));
    }

    #[test]
    fn heartbeat_seeds_from_the_numeric_song_id_and_playlist_context() {
        let mut app = test_app();
        app.playlist_input = "319".to_string();
        app_with_queue(&mut app, &["7", "123", "58"], 1);
        // 默认档是顺序播放：不该有"退出后还原"的档位
        app.start_heartbeat();
        assert_eq!(
            app.heartbeat_seed,
            Some(("123".to_string(), "319".to_string())),
            "种子必须是正在播的歌（磁带面板 id）+ 当前歌单 id"
        );
        assert!(app.heartbeat_seq.is_some(), "应已发出智能播放请求");
        assert!(!app.heartbeat_on, "首批数据没回来之前键面不亮");
    }

    #[test]
    fn heartbeat_needs_a_playing_track() {
        let mut app = test_app();
        app.start_heartbeat();
        assert!(app.heartbeat_seq.is_none(), "没放歌就不该发请求");
        assert!(!app.heartbeat_on);
    }

    #[test]
    fn heartbeat_appends_after_the_list_being_played() {
        let mut app = test_app();
        app_with_queue(&mut app, &["1", "2", "3"], 0);
        app.start_heartbeat();
        app.heartbeat_seq = None;
        app.apply_heartbeat_batch(vec![sample_track("900"), sample_track("901")]);
        assert!(app.heartbeat_on, "拿到续曲后键面才亮");
        let ids: Vec<&str> = app.queue.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["1", "2", "3", "900", "901"],
            "心动续曲接在原播放列表末尾，不替换它"
        );
    }

    #[test]
    fn heartbeat_batch_builds_a_queue_when_playing_without_one() {
        // 直接从浏览列表单曲播放（未建队列）时开心动：追加前必须先接管原列表
        let mut app = test_app();
        app_with_queue(&mut app, &["1", "2"], 0);
        app.queue.clear();
        app.queue_active = false;
        app.start_heartbeat();
        app.heartbeat_seq = None;
        app.apply_heartbeat_batch(vec![sample_track("900")]);
        let ids: Vec<&str> = app.queue.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["1", "2", "900"], "不能把用户的列表换成只有续曲");
    }

    #[test]
    fn heartbeat_dedupes_across_batches() {
        let mut app = test_app();
        app_with_queue(&mut app, &["1"], 0);
        app.start_heartbeat();
        app.heartbeat_seq = None;
        app.apply_heartbeat_batch(vec![sample_track("900")]);
        app.heartbeat_seq = None;
        app.apply_heartbeat_batch(vec![sample_track("900"), sample_track("901")]);
        let ids: Vec<&str> = app.queue.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["1", "900", "901"], "同一首不重复入队");
    }

    #[test]
    fn heartbeat_does_not_spin_when_the_server_has_no_more_tracks() {
        // 续批只在"队列确实长了"之外才可能再发；空批次不得无限自我重试
        let mut app = test_app();
        app_with_queue(&mut app, &["1"], 0);
        app.start_heartbeat();
        app.heartbeat_seq = None;
        app.apply_heartbeat_batch(vec![]);
        assert!(
            app.heartbeat_seq.is_none(),
            "首批空且没建立尾巴：不再追加请求，否则会打转"
        );
    }

    #[test]
    fn stopping_heartbeat_keeps_the_playing_track_and_drops_the_tail() {
        let mut app = test_app();
        app_with_queue(&mut app, &["1", "2", "3"], 1);
        app.start_heartbeat();
        app.heartbeat_seq = None;
        app.apply_heartbeat_batch(vec![sample_track("900"), sample_track("901")]);
        app.stop_heartbeat();
        let ids: Vec<&str> = app.queue.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["1", "2", "3"], "只收回没播到的心动续曲");
        assert!(!app.heartbeat_on);
        assert_eq!(app.core.now_playing().map(|n| n.id.as_str()), Some("2"));
    }

    #[test]
    fn shuffle_button_extinguishes_heartbeat() {
        // 用户明确的互斥要求：点随机播放后，心动键必须灭掉并停止续批
        let mut app = test_app();
        app.playlist_input = "319".to_string();
        app_with_queue(&mut app, &["1", "2", "3"], 0);
        app.start_heartbeat();
        app.heartbeat_seq = None;
        app.apply_heartbeat_batch(vec![sample_track("900"), sample_track("901")]);
        assert!(app.heartbeat_on);
        app.cycle_play_mode(); // 顺序 → 随机
        assert_eq!(app.core.play_mode(), PlayMode::Shuffle);
        assert!(!app.heartbeat_on, "随机档下心动键必须灭");
        assert_eq!(app.heartbeat_seq, None);
        assert_eq!(
            app.queue.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["1", "2", "3"],
            "随机接管后不得留下心动尾巴"
        );
    }

    #[test]
    fn heartbeat_restores_the_mode_it_replaced_but_shuffle_does_not() {
        // 心动进场会把随机压成顺序；正常退出还原随机
        let mut app = test_app();
        app_with_queue(&mut app, &["1", "2", "3"], 0);
        app.core.set_play_mode(PlayMode::Shuffle);
        app.start_heartbeat();
        assert_eq!(
            app.core.play_mode(),
            PlayMode::Sequential,
            "心动期间用顺序档"
        );
        assert_eq!(app.heartbeat_prev_mode, Some(PlayMode::Shuffle));
        app.stop_heartbeat();
        assert_eq!(app.core.play_mode(), PlayMode::Shuffle, "退出还原随机档");

        // 反过来：心动中途点随机，不能把"随机"再被还原逻辑覆盖成别的档位
        app.core.set_play_mode(PlayMode::Sequential);
        app.start_heartbeat();
        app.core.set_play_mode(PlayMode::Shuffle);
        app.heartbeat_seq = None;
        app.apply_heartbeat_batch(vec![sample_track("900")]);
        app.cycle_play_mode(); // 随机 → 单曲循环
        assert_eq!(app.core.play_mode(), PlayMode::RepeatOne);
    }

    #[test]
    fn heartbeat_refuses_a_track_without_an_online_identity() {
        // 非数字 ID（本地文件名等）不可能成为智能播放种子
        let mut app = test_app();
        app_with_queue(&mut app, &["not-a-song-id"], 0);
        app.start_heartbeat();
        assert!(app.heartbeat_seq.is_none(), "没有在线身份就不发请求");
        assert!(!app.heartbeat_pending);
    }

    #[test]
    fn now_playing_track_keeps_the_full_local_path() {
        // 磁带面板把本地曲目的 id 降级成文件名；心动种子与 search_match 都要完整路径
        let dir = temp_dir("heartbeat_local");
        let path = dir.join("YOASOBI - 夜に駆ける.mp3");
        std::fs::write(&path, b"not really audio").unwrap();
        let mut app = test_app();
        app.play_local_path(&path);
        let track = app.now_playing_track().expect("本地播放后应有曲目视图");
        assert_eq!(track.provider, ProviderId::Local);
        assert_eq!(
            track.id,
            path.to_string_lossy().to_string(),
            "本地视图的 id 必须是完整路径"
        );
        assert_eq!(app.netease_song_id(&track), None, "还没匹配上就没有在线 ID");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------------
    // 取消回执的销账（forget_inflight）
    // ------------------------------------------------------------------

    #[test]
    fn cancelling_inflight_lookups_lets_the_app_ask_again() {
        let mut app = test_app();
        // 本地匹配：走真实登记路径
        app.request_local_match("D:/M/a.flac");
        let mseq = *app
            .match_inflight
            .get("D:/M/a.flac")
            .expect("发出后应记为在途");
        // 封面：`cover_texture_url` 的登记方式（发请求 + 按 URL 记在途）
        let curl = "https://pic1/cover.jpg";
        let cseq = app.bridge.cover_image(curl);
        app.cover_inflight.insert(curl.to_string(), cseq);

        app.forget_inflight(mseq);
        app.forget_inflight(cseq);
        assert!(app.match_inflight.is_empty(), "取消必须销账");
        assert!(app.cover_inflight.is_empty(), "取消必须销账");
        assert!(!app.cover_failed.contains(curl), "取消不是失败：不该判死");

        // 销账之后同一个键必须能再问一次——否则磁带架永远缺这张封面
        app.request_local_match("D:/M/a.flac");
        assert!(
            app.match_inflight.contains_key("D:/M/a.flac"),
            "销账后必须可以重新发起"
        );
    }

    #[test]
    fn cancelling_a_search_stops_the_spinner() {
        let mut app = test_app();
        app.search_input = "city pop".into();
        app.submit_search();
        assert!(app.search_state.is_loading());
        let seq = app.search_seq;
        app.forget_inflight(seq);
        assert!(app.search_state.is_idle(), "取消后回到空闲，不是永远转圈");
        assert_eq!(app.search_seq, 0, "单飞登记必须清零");
    }

    // ------------------------------------------------------------------
    // 封面缓存的淘汰与限流（「真封面 ↔ 占位图来回闪烁」的回归）
    // ------------------------------------------------------------------

    #[test]
    fn cover_cache_overflow_evicts_one_not_everything() {
        let ctx = eframe::egui::Context::default();
        let mut app = test_app();
        let rgba = vec![0u8; COVER_PX * COVER_PX * 4];

        for n in 0..=COVER_CACHE_LIMIT {
            app.cover_clock += 1;
            app.install_cover(&ctx, format!("https://cdn/cold-{n}.jpg"), &rgba);
        }

        assert_eq!(
            app.cover_tex.len(),
            COVER_CACHE_LIMIT,
            "超限时只淘汰最久没画的那一条；整体清空会变成 0"
        );
        assert!(
            app.cover_tex
                .contains_key(&format!("https://cdn/cold-{COVER_CACHE_LIMIT}.jpg")),
            "刚装上的一张必须还在——它就是屏幕上正要画的那张"
        );
    }

    #[test]
    fn a_cover_drawn_every_frame_survives_cache_pressure() {
        let ctx = eframe::egui::Context::default();
        let mut app = test_app();
        let rgba = vec![0u8; COVER_PX * COVER_PX * 4];
        let hot = "https://cdn/hot.jpg";

        app.install_cover(&ctx, hot.to_string(), &rgba);
        for n in 0..COVER_CACHE_LIMIT - 1 {
            app.cover_clock += 1;
            app.install_cover(&ctx, format!("https://cdn/cold-{n}.jpg"), &rgba);
        }
        // 屏幕上那张每帧都被取用，LRU 刻度必须一直往前推
        for _ in 0..COVER_CACHE_LIMIT {
            app.cover_clock += 1;
            let entry = app
                .cover_tex
                .get_mut(hot)
                .expect("刚装上就被每帧取用的封面必须还在表里");
            entry.used = app.cover_clock;
        }
        // 再挤一批新封面进来
        for n in 0..16 {
            app.cover_clock += 1;
            app.install_cover(&ctx, format!("https://cdn/new-{n}.jpg"), &rgba);
        }

        assert!(
            app.cover_tex.contains_key(hot),
            "每帧都在画的封面不能被判成冷数据——它一没就变成占位图，闪一下"
        );
        assert_eq!(app.cover_tex.len(), COVER_CACHE_LIMIT, "总量仍受上限约束");
    }

    #[test]
    fn a_whole_playlist_page_of_covers_survives_many_frames() {
        // 歌单页把 263 张账号歌单卡一次画完（非虚拟化），每张封面每帧都要一次。
        // 缓存上限一旦小于这个数，LRU 就会淘汰本帧刚画过的封面——下一帧它退回
        // 占位图再重拉，实测稳态 510 次淘汰/秒，就是「封面与占位图来回交错闪」。
        const PAGE_COVERS: usize = 263;
        let ctx = eframe::egui::Context::default();
        let mut app = test_app();
        let rgba = vec![0u8; COVER_PX * COVER_PX * 4];
        let urls: Vec<String> = (0..PAGE_COVERS)
            .map(|n| format!("https://cdn/page-{n}.jpg"))
            .collect();

        for _ in 0..60 {
            app.cover_clock += 1;
            for (n, url) in urls.iter().enumerate() {
                if app.cover_tex.contains_key(url) {
                    app.cover_texture_url(&ctx, &format!("row-{n}"), Some(url));
                } else {
                    app.install_cover(&ctx, url.clone(), &rgba);
                    app.cover_texture_url(&ctx, &format!("row-{n}"), Some(url));
                }
            }
        }

        assert_eq!(app.cover_tex.len(), PAGE_COVERS, "整页封面必须都在缓存里");
        assert!(
            urls.iter().all(|url| app.cover_tex.contains_key(url)),
            "任何一张被淘汰都会让那一行掉回占位图，闪一下"
        );
    }

    #[test]
    fn installing_a_cover_never_evicts_one_drawn_in_the_same_frame() {
        let ctx = eframe::egui::Context::default();
        let mut app = test_app();
        let rgba = vec![0u8; COVER_PX * COVER_PX * 4];

        app.cover_clock = 7;
        let full: Vec<String> = (0..COVER_CACHE_LIMIT)
            .map(|n| format!("https://cdn/full-{n}.jpg"))
            .collect();
        for url in &full {
            app.install_cover(&ctx, url.clone(), &rgba);
            app.cover_tex.get_mut(url).expect("刚装上").used = 7;
        }
        // 同一帧里又来一张新封面：此时没有任何「旧」条目可淘汰
        app.install_cover(&ctx, "https://cdn/late.jpg".to_string(), &rgba);

        assert_eq!(
            app.cover_tex.len(),
            COVER_CACHE_LIMIT + 1,
            "宁可暂时超出上限，也不能淘汰这一帧正在显示的封面"
        );
        assert!(full.iter().all(|url| app.cover_tex.contains_key(url)));
    }

    #[test]
    fn cover_requests_stop_at_the_inflight_cap() {
        let ctx = eframe::egui::Context::default();
        let mut app = test_app();

        for n in 0..COVER_INFLIGHT_LIMIT * 2 {
            let url = format!("https://cdn/{n}.jpg");
            app.cover_texture_url(&ctx, &format!("row-{n}"), Some(&url));
        }

        assert_eq!(
            app.cover_inflight.len(),
            COVER_INFLIGHT_LIMIT,
            "在途必须封顶：超出封面池槽位的请求会被立刻取消，销账后下一帧又重发，\
             就成了逐帧闪烁"
        );
    }

    #[test]
    fn cancelling_an_old_request_does_not_disturb_the_current_one() {
        // 水位只作废旧请求；若把新请求的 seq 也清了，正在加载的列表会凭空消失
        let mut app = test_app();
        app.search_input = "a".into();
        app.submit_search();
        let stale = app.search_seq;
        app.forget_inflight(stale);
        app.search_input = "b".into();
        app.submit_search();
        let fresh = app.search_seq;
        assert!(app.search_state.is_loading());
        app.forget_inflight(stale);
        assert!(
            app.search_state.is_loading(),
            "重复销旧账不得影响在途的新账"
        );
        assert_eq!(app.search_seq, fresh);
    }

    #[test]
    fn cancelling_a_heartbeat_batch_unblocks_the_button() {
        let mut app = test_app();
        app_with_queue(&mut app, &["1", "2"], 0);
        app.start_heartbeat();
        let seq = app.heartbeat_seq.expect("应有在途请求");
        app.forget_inflight(seq);
        assert_eq!(app.heartbeat_seq, None, "在途登记清零后才能重新按");
        assert!(!app.heartbeat_pending);
    }

    #[test]
    fn local_match_fills_only_the_fields_that_were_missing() {
        let mut target = Track {
            provider: ProviderId::Local,
            id: "D:/M/a.flac".to_string(),
            title: "a".to_string(),
            artists: vec!["—".to_string()],
            ..Track::default()
        };
        let matched = sample_track("123");
        merge_local_match(&mut target, &matched);
        assert_eq!(target.id, "D:/M/a.flac", "在线 ID 不得覆盖本地路径");
        assert_eq!(target.title, "a", "本地已有标题不替换");
        assert_eq!(target.album.as_deref(), Some("Album"), "缺的专辑补齐");
        assert_eq!(target.duration_ms, Some(60_000), "缺的时长补齐");
        assert_eq!(target.artists, vec!["Artist"], "占位艺人应被真艺人换掉");

        // 已有真艺人时不动它
        let mut named = target.clone();
        named.artists = vec!["本地标签里的艺人".to_string()];
        merge_local_match(&mut named, &sample_track("456"));
        assert_eq!(named.artists, vec!["本地标签里的艺人"]);
    }
}
