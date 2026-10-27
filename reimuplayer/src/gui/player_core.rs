//! 播放编排核心：[`PlaySession`] + [`PlaybackController`] 的 GUI 侧状态机。
//!
//! 职责（规范 §6 线程模型 / §8 状态与过期防护）：
//! - **在线流**：`begin_online` → 桥 `PrepareTrack`/`Lyrics` → `on_prepared` →
//!   本地解码播放；网络 I/O 全在桥工作线程，本核心与 UI 线程零阻塞。
//! - **本地流**：`begin_local` + 同名 `.lrc` 侧车歌词，**不触网**，独立可用。
//! - **过期防护**：session generation 使迟到的装载结果被拒绝（文件不处置，
//!   它已进装载器的会话缓存）。
//! - **取消**：`cancel` 提升 generation + 桥 `cancel_pending`（两条 lane 全停）；
//!   换台走 `bridge.cancel_audio`，只掐下载、不作废元数据请求。
//! - **资源释放**：临时文件归 [`crate::music::OnlineTrackLoader`] 的会话缓存，
//!   由桥退出时 `cleanup()` 统一删除；[`PlayerCore::shutdown`] 只负责停引擎
//!   （关掉解码器句柄），好让那次删除不会撞上被占用的文件。
//!
//! 本核心不持有 UI 类型；`E` 为音频引擎（生产 `RodioEngine`，测试 `MockEngine`），
//! 无输出设备时 `player` 为 `None`，浏览/歌词功能不受影响。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::audio::AudioEngine;
use crate::music::lyrics::{parse_lrc, LyricKind, Lyrics};
use crate::music::models::{format_duration, AudioSpec, PlaybackState, Progress};
use crate::music::online_playback::PreparedTrack;
use crate::music::play_session::{FetchPhase, PlaySession};
use crate::music::playback_controller::PlaybackController;
use crate::music::provider::{ProviderError, Track};
use crate::music::stream_buffer::StreamBuffer;

use super::bridge::{ProviderBridge, Response};

/// 播放器状态行（**结构化**，文案按界面语言渲染）。
///
/// 为什么不是 `String`：状态行会挂在 Track Info 面板的设置扳手旁边，
/// 是用户常看的字，必须跟着语言切换。存英文原串就切不动了。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CoreStatus {
    /// 尚无装载，也没播过东西。
    #[default]
    Standby,
    /// 在线装载：正在取播放地址 / 下载。
    FetchingUrl,
    /// 本地文件播放中。
    PlayingLocal,
    /// 在线流播放中。
    PlayingOnline,
    /// 单曲循环重启（与普通播放区分，便于看出循环生效）。
    PlayingRepeat,
    /// 播放中（引擎回报的统一态）。
    Playing,
    /// 已暂停。
    Paused,
    /// 已停止（媒体保留）。
    Stopped,
    /// 在途请求被取消。
    Cancelled,
    /// 播放自然结束。
    Ended,
    /// 失败（原因另置，见 [`CoreStatus::FailedWith`]）。
    Failed,
    /// 失败并带原因（引擎原样文本，不翻译）。
    FailedWith(String),
}

impl CoreStatus {
    /// 状态行文案（`fit_text` 会按可用宽度截断，长英文不会顶掉徽标）。
    pub fn text(&self, lang: crate::i18n::Lang) -> String {
        use crate::i18n::pick;
        match self {
            CoreStatus::Standby => pick(lang, "待机", "STANDBY").to_string(),
            CoreStatus::FetchingUrl => pick(
                lang,
                "缓冲中 · 正在取播放地址",
                "BUFFERING / FETCHING PLAYBACK URL",
            )
            .to_string(),
            CoreStatus::PlayingLocal => pick(lang, "播放中（本地）", "PLAYING (LOCAL)").to_string(),
            CoreStatus::PlayingOnline => {
                pick(lang, "播放中（在线）", "PLAYING (ONLINE)").to_string()
            }
            CoreStatus::PlayingRepeat => {
                pick(lang, "播放中（单曲循环）", "PLAYING (REPEAT)").to_string()
            }
            CoreStatus::Playing => pick(lang, "播放中", "PLAYING").to_string(),
            CoreStatus::Paused => pick(lang, "已暂停", "PAUSED").to_string(),
            CoreStatus::Stopped => pick(lang, "已停止", "STOPPED").to_string(),
            CoreStatus::Cancelled => pick(lang, "已取消", "CANCELLED").to_string(),
            CoreStatus::Ended => pick(lang, "已播完", "ENDED").to_string(),
            CoreStatus::Failed => pick(lang, "失败", "FAILED").to_string(),
            // 原因来自引擎/系统，原样保留（翻译会失真）
            CoreStatus::FailedWith(reason) => {
                format!("{}{reason}", pick(lang, "失败：", "FAILED / "))
            }
        }
    }
}

/// 现在播放的展示元数据（在线来自 [`Track`]，本地来自 [`MediaInfo`] 标签）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    /// 在线=歌曲 ID；本地=文件名（仅展示）。
    pub id: String,
    pub is_local: bool,
    /// 封面图 URL（在线曲目有；本地曲目为 `None`）。
    ///
    /// 放在这里而不是让 UI 去 `known` 表里回查：`NowPlaying` 是"当前舱内磁带"
    /// 的唯一事实源，封面属于磁带属性；回查表在本地曲目路径下根本不存在。
    pub cover_url: Option<String>,
    /// 音频流参数（音质）。在线曲目在装载完成后才由接口补齐。
    pub spec: AudioSpec,
}

/// 播放失败原因。
///
/// 为什么不直接存 `String`：失败原因会进**错误模态框**，那是用户可见的，
/// 必须能跟着界面语言切换；类型化之后措辞统一在 [`PlaybackFailure::message`] 里出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackFailure {
    /// Provider 侧错误（网络 / 版权 / VIP / 地区限制…）。
    Provider(ProviderError),
    /// 本地音频引擎错误。
    Engine(EngineFailure),
}

/// 本地音频引擎失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineFailure {
    /// 没有可用的音频输出设备（无设备路径，播放降级）。
    NoDevice,
    /// 解码/装载失败（保留底层错误文本，便于诊断）。
    Decode(String),
    /// 播放控制失败（播放/暂停、seek）。底层文本来自引擎，原样保留。
    Transport(String),
}

impl EngineFailure {
    /// 按界面语言渲染人话。
    pub fn message(&self, lang: crate::i18n::Lang) -> String {
        match self {
            EngineFailure::NoDevice => crate::i18n::pick(
                lang,
                "无音频输出设备，无法解码播放",
                "No audio output device — cannot decode",
            )
            .to_string(),
            EngineFailure::Decode(raw) => crate::i18n::pick(
                lang,
                &format!("解码失败：{raw}"),
                &format!("Decode failed: {raw}"),
            )
            .to_string(),
            EngineFailure::Transport(raw) => crate::i18n::pick(
                lang,
                &format!("播放控制失败：{raw}"),
                &format!("Playback control failed: {raw}"),
            )
            .to_string(),
        }
    }
}

impl PlaybackFailure {
    /// 按界面语言渲染人话。
    pub fn message(&self, lang: crate::i18n::Lang) -> String {
        match self {
            PlaybackFailure::Provider(e) => e.message(lang),
            PlaybackFailure::Engine(e) => e.message(lang),
        }
    }
}

/// 循环档位。
///
/// 与 [`PlayerCore::shuffle`]（随机/顺序）合成 UI 上的四档 [`PlayMode`]：
/// - [`RepeatMode::Off`]：不循环——走到列表末尾即停（UI 上叫「顺序播放」）；
/// - [`RepeatMode::All`]：列表循环——走到末尾时从头再来；
/// - [`RepeatMode::One`]：单曲循环——当前曲结束即重播。
///
/// 语义归属：`One` 由 [`PlayerCore::tick`] 在本核心内处理（结束即 seek 0 重播）；
/// `All` 由 UI 层 [`crate::gui::app::ReimuApp`] 的 `auto_next` 处理（末尾回绕）；
/// `Off` 两者都不做，播到底停下。
///
/// 码值与老配置一致：`0=Off / 1=All / 2=One`，未知值回默认档。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepeatMode {
    /// 不循环（顺序播完即停）。
    #[default]
    Off,
    /// 列表循环。
    All,
    /// 单曲循环。
    One,
}

impl RepeatMode {
    /// 是否为单曲循环。
    pub fn is_one(self) -> bool {
        matches!(self, RepeatMode::One)
    }

    /// 是否为列表循环。
    pub fn is_all(self) -> bool {
        matches!(self, RepeatMode::All)
    }

    /// 持久化编码（`0=顺序不循环 / 1=列表循环 / 2=单曲循环`，与历史配置一致）。
    pub fn code(self) -> u8 {
        match self {
            RepeatMode::Off => 0,
            RepeatMode::All => 1,
            RepeatMode::One => 2,
        }
    }

    /// 从持久化编码恢复；未知值回默认档 [`RepeatMode::Off`]。
    pub fn from_code(code: u8) -> Self {
        match code {
            1 => RepeatMode::All,
            2 => RepeatMode::One,
            _ => RepeatMode::Off,
        }
    }
}

/// UI 播放模式键的四档（一次点击走一档）。
///
/// 为什么需要这个枚举而不是直接摆两个布尔：四档里 `Shuffle` 与 `RepeatAll`
/// 共用 `repeat = All`，光看 `repeat_mode` 分不清当前是哪一档，看 `shuffle`
/// 又分不清它是配列表循环还是配单曲循环。这里把「随机 + 单曲循环」这个
/// 自相矛盾的组合排除在外——随机档固定配列表循环，键的显示与行为才是一对一。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayMode {
    /// 顺序播放（不循环）。
    Sequential,
    /// 随机播放（配列表循环）。
    Shuffle,
    /// 单曲循环。
    RepeatOne,
    /// 列表循环。
    RepeatAll,
}

impl PlayMode {
    /// 循环顺序：顺序 → 随机 → 单曲循环 → 列表循环 → 顺序。
    pub fn next(self) -> Self {
        match self {
            PlayMode::Sequential => PlayMode::Shuffle,
            PlayMode::Shuffle => PlayMode::RepeatOne,
            PlayMode::RepeatOne => PlayMode::RepeatAll,
            PlayMode::RepeatAll => PlayMode::Sequential,
        }
    }

    /// 由当前 `(shuffle, repeat)` 读出档位。
    ///
    /// 随机优先：`shuffle` 为真时不论 `repeat` 是什么都算随机档——组合只在
    /// 恢复老配置时才可能出现（`shuffle=true` + `repeat=2`），此时按随机档显示，
    /// 下一次点击会把它归一到「随机 + 列表循环」。
    pub fn of(shuffle: bool, repeat: RepeatMode) -> Self {
        if shuffle {
            PlayMode::Shuffle
        } else {
            match repeat {
                RepeatMode::Off => PlayMode::Sequential,
                RepeatMode::All => PlayMode::RepeatAll,
                RepeatMode::One => PlayMode::RepeatOne,
            }
        }
    }

    /// 本档对应的 `(shuffle, repeat)`。
    pub fn parts(self) -> (bool, RepeatMode) {
        match self {
            PlayMode::Sequential => (false, RepeatMode::Off),
            PlayMode::Shuffle => (true, RepeatMode::All),
            PlayMode::RepeatOne => (false, RepeatMode::One),
            PlayMode::RepeatAll => (false, RepeatMode::All),
        }
    }
}

/// 播放编排核心对外事件（UI 据此提示/跳转）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreEvent {
    /// 装载完成并开始播放。
    Started,
    /// 装载/解码失败（类型化，文案由 UI 按语言渲染）。
    Failed(PlaybackFailure),
    /// 请求被取消（静默）。
    Cancelled,
    /// 歌词已就绪（含「暂无歌词」空槽）。
    LyricsReady,
    /// 迟到的过期结果被拒绝（不打扰用户）。
    Superseded,
}

/// `tick` 检测到曲目自然结束时的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickAction {
    None,
    /// 非单曲循环下播放结束 → 交给 UI 播下一曲。
    TrackEnded,
}

/// 播放编排核心。
pub struct PlayerCore<E: AudioEngine> {
    session: PlaySession,
    player: Option<PlaybackController<E>>,
    /// 桥请求序列号（装载）。
    pub prepare_seq: u64,
    /// 桥请求序列号（歌词）。
    pub lyric_seq: u64,
    /// 当前装载代号（与 [`PlaySession`] generation 绑定）。
    fetch_gen: u64,
    now: Option<NowPlaying>,
    volume: f32,
    pub shuffle: bool,
    pub repeat_mode: RepeatMode,
    /// 结束事件已上报（防重复触发下一曲）。
    end_notified: bool,
    /// 人话状态行（缓冲/失败等，UI 按语言渲染）。
    status: CoreStatus,
}

impl<E: AudioEngine> PlayerCore<E> {
    /// 构建核心。`player: None` 表示无音频输出设备（浏览/歌词仍可用）。
    pub fn new(player: Option<PlaybackController<E>>) -> Self {
        let mut core = Self {
            session: PlaySession::new(),
            player,
            prepare_seq: 0,
            lyric_seq: 0,
            fetch_gen: 0,
            now: None,
            volume: 0.72,
            shuffle: false,
            repeat_mode: RepeatMode::default(),
            end_notified: false,
            status: CoreStatus::Standby,
        };
        core.apply_volume();
        core
    }

    // ------------------------------------------------------------------
    // 播放入口
    // ------------------------------------------------------------------

    /// 在线播放：退役旧媒体 → 发装载/歌词请求。返回 session generation。
    pub fn play_online(&mut self, track: &Track, bridge: &mut ProviderBridge) -> u64 {
        self.play_online_with_lyrics(track, bridge, None)
    }

    /// 在线播放（可注入已缓存的歌词，避免对同一首歌重复请求）。
    ///
    /// 与 [`Self::play_online`] 的唯一区别：`cached_lyrics` 为 `Some` 时**不**发
    /// 歌词网络请求，直接把缓存结果喂给 session（歌词在预缓存阶段已取回）。
    /// 这是「下一首预缓存」的消费侧：切歌时命中缓存即可省一次网络往返。
    pub fn play_online_with_lyrics(
        &mut self,
        track: &Track,
        bridge: &mut ProviderBridge,
        cached_lyrics: Option<Lyrics>,
    ) -> u64 {
        self.stop_media();
        // 换台第一件事：掐掉上一首在途的下载。
        //
        // 不掐的后果不是"多下一次"——装载线程是串行的，旧下载会把新请求堵在
        // 队列里，用户点下一首要先等上一首下完（最长 60s）。这一步必须早于
        // `prepare_track` 分配 seq，理由见 `ProviderBridge::cancel_audio`。
        // 只掐音频 lane：刚发出的歌单/搜索/歌词请求要继续跑完。
        bridge.cancel_audio();
        let gen = self.session.begin_online(track.id.clone());
        self.fetch_gen = gen;
        self.prepare_seq = bridge.prepare_track(track.id.clone());
        match cached_lyrics {
            Some(lyrics) => {
                self.lyric_seq = 0;
                self.session.on_lyrics(gen, Ok(lyrics));
            }
            None => {
                self.lyric_seq = bridge.lyrics(track.id.clone());
            }
        }
        self.now = Some(NowPlaying {
            title: track.title.clone(),
            artist: track.artists.join("/"),
            album: track.album.clone(),
            id: track.id.clone(),
            is_local: false,
            cover_url: track.cover_url.clone(),
            // 音质要等 PrepareTrack 回来才知道，装载完成时补齐
            spec: AudioSpec::default(),
        });
        self.end_notified = false;
        self.status = CoreStatus::FetchingUrl;
        gen
    }

    /// 本地播放：加载文件直接解码，同名 `.lrc` 作歌词（侧车）；**不触网**。
    /// 返回 session generation。
    pub fn play_local(&mut self, path: &Path) -> u64 {
        self.stop_media();
        let gen = self.session.begin_local(path);
        self.fetch_gen = gen;
        self.prepare_seq = 0;
        self.lyric_seq = 0;
        self.end_notified = false;
        let info = crate::music::load_media(path);
        match info {
            Ok(media) => {
                self.now = Some(NowPlaying {
                    title: media.display_title(),
                    artist: media
                        .tags
                        .artist
                        .clone()
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or_else(|| "—".to_string()),
                    album: media.tags.album.clone(),
                    id: path
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.to_string_lossy().to_string()),
                    is_local: true,
                    cover_url: None,
                    spec: media.audio.clone(),
                });
                self.load_sidecar_lyrics(path, gen);
                match self.load_and_play(path) {
                    Ok(()) => {
                        self.session.on_loaded(gen);
                        self.status = CoreStatus::PlayingLocal;
                    }
                    Err(failure) => {
                        self.session
                            .on_load_error(gen, failure.message(crate::i18n::Lang::Zh));
                        self.status = CoreStatus::Failed;
                    }
                }
            }
            Err(e) => {
                // 本地文件读不出标签（损坏/权限/格式不支持）
                let failure = EngineFailure::Decode(e.to_string());
                self.session
                    .on_load_error(gen, failure.message(crate::i18n::Lang::Zh));
                self.status = CoreStatus::Failed;
            }
        }
        gen
    }

    /// 同名 `.lrc` 侧车歌词（缺失/不可读 → 空歌词「暂无歌词」，不伪造）。
    fn load_sidecar_lyrics(&mut self, path: &Path, gen: u64) {
        let lrc_path = path.with_extension("lrc");
        let result = match std::fs::read_to_string(&lrc_path) {
            Ok(text) => Ok(parse_lrc(&text)),
            Err(_) => Ok(Lyrics::empty(LyricKind::Plain)),
        };
        self.session.on_lyrics(gen, result);
    }

    // ------------------------------------------------------------------
    // 桥响应
    // ------------------------------------------------------------------

    /// 处理与播放相关的桥响应（装载/歌词/取消）。
    /// 与当前会话无关的响应返回 `None`，由 UI 层继续分发。
    pub fn on_response(&mut self, response: &Response) -> Option<CoreEvent> {
        match response {
            Response::Prepared { seq, result }
                if *seq == self.prepare_seq && self.prepare_seq != 0 =>
            {
                self.prepare_seq = 0;
                match result {
                    Ok(prepared) => self.accept_prepared(prepared.clone()),
                    Err(e) => {
                        self.session.on_prepare_error(self.fetch_gen, e.clone());
                        // 状态行只写语言中立的 FAILED，人话原因走错误模态框
                        self.status = CoreStatus::Failed;
                        Some(CoreEvent::Failed(PlaybackFailure::Provider(e.clone())))
                    }
                }
            }
            // 装载被取消（换台、队列满、显式取消）。
            //
            // 没有这个分支时 `prepare_seq` 永远留在原地、session 卡在
            // `FetchPhase::Fetching`：DECK 一直显示 BUFFERING，`toggle()` 被
            // `is_buffering()` 吞掉，界面**永久卡住**。换台现在会主动抬水位，
            // 取消从"罕见"变成"常态"，所以这条必须补。
            Response::Cancelled { seq } if *seq == self.prepare_seq && self.prepare_seq != 0 => {
                self.prepare_seq = 0;
                self.session
                    .on_prepare_error(self.fetch_gen, ProviderError::Cancelled);
                self.status = CoreStatus::Cancelled;
                Some(CoreEvent::Cancelled)
            }
            Response::Lyrics { seq, result } if *seq == self.lyric_seq && self.lyric_seq != 0 => {
                self.lyric_seq = 0;
                self.session.on_lyrics(self.fetch_gen, result.clone());
                Some(CoreEvent::LyricsReady)
            }
            Response::Cancelled { seq } if *seq == self.lyric_seq && self.lyric_seq != 0 => {
                self.lyric_seq = 0;
                self.session
                    .on_lyrics(self.fetch_gen, Err(ProviderError::Cancelled));
                Some(CoreEvent::LyricsReady)
            }
            _ => None,
        }
    }

    /// 装载结果落地（已通过 generation 门槛）。
    fn accept_prepared(&mut self, prepared: PreparedTrack) -> Option<CoreEvent> {
        if !self.session.on_prepared(self.fetch_gen, prepared.clone()) {
            // 过期结果：临时文件已登记退役，静默回收
            return Some(CoreEvent::Superseded);
        }
        // 接口这次才给出真实音质（level / type），加上后台线程探测到的流参数，
        // 一起补进 NowPlaying 供面板展示
        let mut spec = prepared.spec.clone();
        spec.quality = prepared.source.quality.as_ref().map(|q| q.to_string());
        if spec.codec.is_none() {
            spec.codec = prepared.source.content_type.clone();
        }
        if let Some(now) = self.now.as_mut() {
            now.spec = spec.clone();
        }
        match self.load_prepared(&prepared, &spec) {
            Ok(()) => {
                self.session.on_loaded(self.fetch_gen);
                self.status = if self.now.as_ref().map(|n| n.is_local).unwrap_or(false) {
                    CoreStatus::PlayingLocal
                } else {
                    CoreStatus::PlayingOnline
                };
                Some(CoreEvent::Started)
            }
            Err(failure) => {
                self.session
                    .on_load_error(self.fetch_gen, failure.message(crate::i18n::Lang::Zh));
                self.status = CoreStatus::Failed;
                Some(CoreEvent::Failed(PlaybackFailure::Engine(failure)))
            }
        }
    }

    /// 引擎装载并起播。错误转人话（规范 8.2：不泄原始错误/JSON）。
    fn load_and_play(&mut self, path: &Path) -> Result<(), EngineFailure> {
        let Some(player) = self.player.as_mut() else {
            return Err(EngineFailure::NoDevice);
        };
        player
            .load(path)
            .map_err(|e| EngineFailure::Decode(e.to_string()))?;
        player
            .play()
            .map_err(|e| EngineFailure::Decode(e.to_string()))?;
        self.apply_volume();
        Ok(())
    }

    /// 装载并起播：**交付形态决定入口**。
    ///
    /// `stream` 有值 = 那份文件**还没下完**，只能走
    /// [`Self::load_stream_and_play`]。用整首入口打开它，解码器会在水位之外
    /// 撞到尾部空洞——表现是「播到一半安静下来」而不是「缓冲中」，
    /// 比报错更难懂，所以这个选择不能留给调用点随手写。
    fn load_prepared(
        &mut self,
        prepared: &PreparedTrack,
        spec: &AudioSpec,
    ) -> Result<(), EngineFailure> {
        match prepared.stream.as_ref() {
            Some(buffer) => self.load_stream_and_play(Arc::clone(buffer), spec),
            None => self.load_and_play_probed(&prepared.path, spec),
        }
    }

    /// 边下边播：把还在长的缓冲交给解码泵，**起播不等整首下完**。
    ///
    /// 装载本身在泵线程构造解码器（读容器头），UI 线程只做一次 `play()`；
    /// 之后的「等字节」由环形缓冲自己说（[`Self::is_buffering`]）。
    fn load_stream_and_play(
        &mut self,
        buffer: Arc<StreamBuffer>,
        spec: &AudioSpec,
    ) -> Result<(), EngineFailure> {
        let Some(player) = self.player.as_mut() else {
            return Err(EngineFailure::NoDevice);
        };
        player
            .load_stream_probed(buffer, spec.clone())
            .map_err(|e| EngineFailure::Decode(e.to_string()))?;
        player
            .play()
            .map_err(|e| EngineFailure::Decode(e.to_string()))?;
        self.apply_volume();
        Ok(())
    }

    /// 在线曲目：用装载线程**已经探测好**的 spec 直接装载。
    ///
    /// 与 [`Self::load_and_play`] 的区别只有一处：不重新 `load_media`。
    /// 在线曲目的元数据在装载线程里已探过一遍，这一步在 UI 线程上再探一次
    /// 纯粹是每首歌多一次头部解析 + 文件打开，全压在 egui 帧里。
    fn load_and_play_probed(&mut self, path: &Path, spec: &AudioSpec) -> Result<(), EngineFailure> {
        let Some(player) = self.player.as_mut() else {
            return Err(EngineFailure::NoDevice);
        };
        player
            .load_probed(path, spec.clone())
            .map_err(|e| EngineFailure::Decode(e.to_string()))?;
        player
            .play()
            .map_err(|e| EngineFailure::Decode(e.to_string()))?;
        self.apply_volume();
        Ok(())
    }

    // ------------------------------------------------------------------
    // 播放控制
    // ------------------------------------------------------------------

    /// 播放/暂停切换（装载中忽略）。
    pub fn toggle(&mut self) -> Result<(), EngineFailure> {
        if self.session.phase().is_busy() {
            return Ok(());
        }
        let state = {
            let Some(player) = self.player.as_mut() else {
                return Err(EngineFailure::NoDevice);
            };
            player
                .toggle()
                .map_err(|e| EngineFailure::Transport(e.to_string()))?
                .clone()
        };
        self.status = match state {
            PlaybackState::Playing => CoreStatus::Playing,
            PlaybackState::Paused => CoreStatus::Paused,
            PlaybackState::Ended => CoreStatus::Ended,
            PlaybackState::Failed(reason) => CoreStatus::FailedWith(reason),
            // Idle/Ready：引擎没在播，界面照实显示"已停止"
            PlaybackState::Idle | PlaybackState::Ready => CoreStatus::Stopped,
        };
        Ok(())
    }

    /// 停止（保留媒体，可再播）。
    pub fn stop(&mut self) {
        self.stop_media();
        self.status = CoreStatus::Stopped;
    }

    fn stop_media(&mut self) {
        if let Some(player) = self.player.as_mut() {
            let _ = player.stop();
            let _ = player.clear();
        }
        self.end_notified = false;
    }

    /// 按比例 seek（波形/进度条，ratio 0..=1）。
    pub fn seek_ratio(&mut self, ratio: f32) -> Result<(), EngineFailure> {
        let Some(player) = self.player.as_mut() else {
            return Err(EngineFailure::NoDevice);
        };
        player
            .seek_percent(ratio.clamp(0.0, 1.0) * 100.0)
            .map_err(|e| EngineFailure::Transport(e.to_string()))
    }

    /// seek 到绝对位置（歌词行点击）。
    pub fn seek_to(&mut self, position: Duration) -> Result<(), EngineFailure> {
        let Some(player) = self.player.as_mut() else {
            return Err(EngineFailure::NoDevice);
        };
        player
            .seek(position)
            .map_err(|e| EngineFailure::Transport(e.to_string()))
    }

    /// 音量（0..=1）。
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        self.apply_volume();
    }

    fn apply_volume(&mut self) {
        if let Some(player) = self.player.as_mut() {
            let _ = player.set_volume(self.volume);
        }
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    /// 取消在途装载/歌词：提升 generation + 桥中断在途网络请求。
    ///
    /// [`ProviderBridge::cancel_pending`] 会把**两条 lane** 的水位一起拉高
    /// （音频 + 元数据）；只想掐下载请直接用 `cancel_audio`。
    pub fn cancel(&mut self, bridge: &mut ProviderBridge) {
        bridge.cancel_pending();
        self.session.cancel_fetch();
        self.prepare_seq = 0;
        self.lyric_seq = 0;
        self.status = CoreStatus::Cancelled;
    }

    // ------------------------------------------------------------------
    // 帧驱动
    // ------------------------------------------------------------------

    /// 每帧轮询引擎状态；自然结束返回 [`TickAction::TrackEnded`]（防重复）。
    pub fn tick(&mut self) -> TickAction {
        let state = match self.player.as_mut() {
            Some(player) => player.poll().clone(),
            None => return TickAction::None,
        };
        if state == PlaybackState::Ended && !self.end_notified {
            self.end_notified = true;
            if self.repeat_mode.is_one() {
                if let Some(player) = self.player.as_mut() {
                    let _ = player.seek(Duration::ZERO);
                    let _ = player.play();
                }
                self.end_notified = false;
                self.status = CoreStatus::PlayingRepeat;
                TickAction::None
            } else {
                self.status = CoreStatus::Ended;
                TickAction::TrackEnded
            }
        } else if let PlaybackState::Failed(reason) = state {
            self.status = CoreStatus::FailedWith(reason);
            TickAction::None
        } else {
            TickAction::None
        }
    }

    /// 退出收口：停引擎 → 松开媒体引用。
    ///
    /// 临时文件不在这里删：它们归装载器的会话缓存所有，由桥退出时
    /// `OnlineTrackLoader::cleanup()` 统一删除。顺序天然正确——
    /// `Drop for ReimuApp` 先调这里（`player.clear()` 关掉解码器句柄），
    /// 之后字段 drop 才轮到 `bridge`，那时删除不会撞上被占用的文件。
    pub fn shutdown(&mut self) {
        self.stop_media();
        self.session.release_media();
    }

    // ------------------------------------------------------------------
    // 视图
    // ------------------------------------------------------------------

    pub fn session(&self) -> &PlaySession {
        &self.session
    }

    /// UI 模式键当前档位（由 `shuffle` + `repeat_mode` 合成，不另存一份真值）。
    pub fn play_mode(&self) -> PlayMode {
        PlayMode::of(self.shuffle, self.repeat_mode)
    }

    /// 设为某个档位：两个字段一起写，避免「随机 + 单曲循环」这种自相矛盾的组合。
    pub fn set_play_mode(&mut self, mode: PlayMode) {
        let (shuffle, repeat) = mode.parts();
        self.shuffle = shuffle;
        self.repeat_mode = repeat;
    }

    pub fn now_playing(&self) -> Option<&NowPlaying> {
        self.now.as_ref()
    }

    /// 进度（来自音频输出时钟；无引擎/媒体时如实 0 / 未知）。
    pub fn progress(&self) -> Progress {
        match &self.player {
            Some(player) => player.progress(),
            None => Progress {
                position: Duration::ZERO,
                total: None,
            },
        }
    }

    /// 播放进度比例 0..=1（总长未知返回 0，不伪造）。
    pub fn progress_ratio(&self) -> f32 {
        let p = self.progress();
        match p.total {
            Some(total) if total.as_secs_f32() > 0.0 => {
                (p.position.as_secs_f32() / total.as_secs_f32()).clamp(0.0, 1.0)
            }
            _ => 0.0,
        }
    }

    /// 当前应高亮的歌词行（`Lyrics::line_at`，实际播放时钟驱动，规范 7.4）。
    pub fn active_lyric(&self) -> Option<usize> {
        let lyrics = self.session.lyrics()?;
        lyrics.line_at(self.progress().position)
    }

    /// 时间行文本（`mm:ss` / `mm:ss`）。
    pub fn time_texts(&self) -> (String, String) {
        let p = self.progress();
        (
            format_duration(p.position),
            p.total
                .map(format_duration)
                .unwrap_or_else(|| "--:--".to_string()),
        )
    }

    /// 人话状态行（缓冲/播放/失败等），按界面语言渲染。
    pub fn status_text(&self, lang: crate::i18n::Lang) -> String {
        self.status.text(lang)
    }

    /// 舱位状态（DECK 01 / …）。
    pub fn deck_status(&self) -> String {
        match self.session.phase() {
            FetchPhase::Empty => {
                if self
                    .player
                    .as_ref()
                    .map(|p| p.state() == &PlaybackState::Playing)
                    .unwrap_or(false)
                {
                    "DECK 01 / PLAYING".to_string()
                } else {
                    "DECK 01 / CASSETTE".to_string()
                }
            }
            FetchPhase::Fetching => "DECK 01 / BUFFERING".to_string(),
            FetchPhase::Loading => "DECK 01 / LOADING".to_string(),
            FetchPhase::Ready => "DECK 01 / CASSETTE".to_string(),
            FetchPhase::Failed(_) => "DECK 01 / ERROR".to_string(),
        }
    }

    /// 装载是否进行中（缓冲中）。
    ///
    /// 两个来源，含义不同但 UI 只有一句「缓冲中」：
    /// - 会话还在取址/下载（字节还没到闸门）；
    /// - 引擎的 PCM 环空了而源头还没收工（字节到了但跟不上）——这条只有
    ///   流式播放（[`crate::audio::stream_pump`]）才可能给出。
    pub fn is_buffering(&self) -> bool {
        self.session.phase().is_busy()
            || self
                .player
                .as_ref()
                .is_some_and(|p| p.engine().is_buffering())
    }

    /// 引擎是否存在。
    pub fn has_engine(&self) -> bool {
        self.player.is_some()
    }

    /// 是否正在出声（暂停 / 无引擎 / 缓冲中一律 `false`）。
    ///
    /// 供 SMTC 与托盘同步播放态：这两处都要"系统认为在播 = 真的在出声"，
    /// 否则系统浮层会显示 ▶ 却还在响，或反过来。
    pub fn is_playing(&self) -> bool {
        self.player
            .as_ref()
            .map(|p| p.state() == &PlaybackState::Playing)
            .unwrap_or(false)
    }

    /// 引擎状态名（System Monitor 展示）。
    pub fn engine_state(&self) -> &'static str {
        match self.player.as_ref().map(|p| p.state()) {
            Some(state) => state.name(),
            None => "NoDevice",
        }
    }

    /// 当前一帧实时频谱（26 段电平）。
    ///
    /// 没有引擎、未播放或样本不足时返回 `None`，调用方据此显示静止状态。
    pub fn spectrum(&self) -> Option<crate::audio::Bands> {
        self.player.as_ref().and_then(|p| p.engine().spectrum())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::MockEngine;
    use crate::i18n::Lang;
    use crate::music::provider::{
        LoginState, Playlist, ProviderId, QrLoginSession, TrackAvailability,
    };
    use std::path::PathBuf;
    use std::sync::Arc;

    /// 不触网的 Stub Provider（本地流不需要它，仅供构造）。
    struct StubProvider;

    impl crate::music::provider::MusicProvider for StubProvider {
        fn id(&self) -> ProviderId {
            ProviderId::Netease
        }
        async fn search_songs(&self, _: &str, _: u32) -> Result<Vec<Track>, ProviderError> {
            Err(ProviderError::EmptyResult)
        }
        async fn song_details(&self, _: &[String]) -> Result<Vec<Track>, ProviderError> {
            Err(ProviderError::EmptyResult)
        }
        async fn playlist(&self, _: &str, _: u32, _: u32) -> Result<Playlist, ProviderError> {
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
            _: crate::music::provider::FeaturedKind,
            _: u32,
        ) -> Result<Vec<crate::music::provider::PlaylistSummary>, ProviderError> {
            Err(ProviderError::EmptyResult)
        }
        async fn check_availability(&self, _: &str) -> Result<TrackAvailability, ProviderError> {
            Ok(TrackAvailability::Available)
        }
        async fn playback_source(
            &self,
            _: &str,
        ) -> Result<crate::music::provider::PlaybackSource, ProviderError> {
            Err(ProviderError::PlaybackUrlUnavailable)
        }
        async fn lyrics(&self, _: &str) -> Result<Lyrics, ProviderError> {
            Err(ProviderError::EmptyResult)
        }
        async fn login_status(&self) -> Result<LoginState, ProviderError> {
            Ok(LoginState::Anonymous)
        }
        async fn qr_login_start(&self) -> Result<QrLoginSession, ProviderError> {
            Ok(QrLoginSession {
                key: "stub-key".into(),
                qr_url: "https://example.invalid/qr".into(),
            })
        }
        async fn qr_login_poll(&self, _: &str) -> Result<LoginState, ProviderError> {
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

    fn test_bridge() -> ProviderBridge {
        ProviderBridge::spawn(
            Arc::new(StubProvider) as Arc<dyn crate::music::provider::DynMusicProvider>,
            super::super::bridge::noop_fetcher(),
        )
    }

    fn write_wav(dir: &Path, name: &str) -> PathBuf {
        // 44 字节静音 WAV 头 + 0 样本（MockEngine 不真正解码）
        let path = dir.join(name);
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF\x24\x00\x00\x00WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&44100u32.to_le_bytes());
        wav.extend_from_slice(&44100u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data\x00\x00\x00\x00");
        std::fs::write(&path, wav).unwrap();
        path
    }

    fn unique_dir() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("reimuplayer-core-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn mock_core() -> PlayerCore<MockEngine> {
        let engine = MockEngine::new().with_duration(Some(Duration::from_secs(60)));
        PlayerCore::new(Some(PlaybackController::new(engine)))
    }

    fn sample_track(id: &str) -> Track {
        Track {
            provider: ProviderId::Netease,
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
    fn play_online_dispatches_prepare_and_lyrics_requests() {
        let mut core = mock_core();
        let mut bridge = test_bridge();
        let track = sample_track("347230");
        let gen = core.play_online(&track, &mut bridge);
        assert_eq!(gen, 1);
        assert_eq!(core.prepare_seq, 1);
        assert_eq!(core.lyric_seq, 2);
        assert!(core.is_buffering(), "装载中应为缓冲态");
        assert!(core.status_text(Lang::En).contains("BUFFERING"));
        assert!(
            core.status_text(Lang::Zh).contains("缓冲"),
            "中文态要出中文"
        );
        assert_eq!(core.deck_status(), "DECK 01 / BUFFERING");
    }

    #[test]
    fn prepared_response_loads_and_plays() {
        let mut core = mock_core();
        let mut bridge = test_bridge();
        let dir = unique_dir();
        let path = write_wav(&dir, "ok.wav");
        core.play_online(&sample_track("347230"), &mut bridge);
        let seq = core.prepare_seq;
        let event = core.on_response(&Response::Prepared {
            seq,
            result: Ok(PreparedTrack {
                path: path.clone(),
                source: crate::music::provider::PlaybackSource {
                    url: "https://example.invalid/a.mp3".into(),
                    quality: Some(crate::music::provider::AudioQuality::Standard),
                    content_type: Some("mp3".into()),
                    bitrate_kbps: None,
                    expires_at: None,
                },
                spec: crate::music::models::AudioSpec::default(),
                stream: None,
            }),
        });
        assert_eq!(event, Some(CoreEvent::Started));
        assert_eq!(core.engine_state(), "Playing");
        assert!(core.status_text(Lang::En).contains("PLAYING"));
        // 退出只停引擎、松开引用；文件归装载器的会话缓存，由桥退出时统一删
        core.shutdown();
        assert!(path.exists(), "shutdown 不再删文件（删除权已移交装载器）");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_streaming_prepared_track_goes_to_the_pump_not_the_whole_file_entry() {
        // 交付形态决定入口：带着「还在长的缓冲」的曲目只能走 `load_stream_probed`。
        // 用整首入口打开它 = 解码器在水位之外撞尾部空洞，表现是「播到一半安静下来」
        // 而不是「缓冲中」，比报错更难懂——所以这条判据必须由测试钉住。
        //
        // 判据选 `file_size`：流式入口用**声明总长**，整首入口用 `metadata()`。
        // 盘上只有 44 字节头，声明是 90 MB 级，两个入口不可能混。
        let mut core = mock_core();
        let mut bridge = test_bridge();
        let dir = unique_dir();
        let path = write_wav(&dir, "track_0001_stream.flac");
        let on_disk = std::fs::metadata(&path).unwrap().len();
        let declared = on_disk + 90 * 1024 * 1024;
        let buffer = StreamBuffer::new(path.clone());
        buffer.declare_len(declared);
        buffer.commit(on_disk);

        core.play_online(&sample_track("347230"), &mut bridge);
        let seq = core.prepare_seq;
        let event = core.on_response(&Response::Prepared {
            seq,
            result: Ok(PreparedTrack {
                path: path.clone(),
                source: crate::music::provider::PlaybackSource {
                    url: "https://example.invalid/a.flac".into(),
                    quality: Some(crate::music::provider::AudioQuality::Lossless),
                    content_type: Some("flac".into()),
                    bitrate_kbps: Some(128),
                    expires_at: None,
                },
                spec: crate::music::models::AudioSpec {
                    codec: Some("FLAC".into()),
                    ..crate::music::models::AudioSpec::default()
                },
                stream: Some(buffer),
            }),
        });
        assert_eq!(event, Some(CoreEvent::Started), "起播不等整首下完");
        assert_eq!(core.engine_state(), "Playing");

        let media = core
            .player
            .as_ref()
            .expect("mock_core 带着引擎")
            .current_media()
            .expect("已装载");
        assert_eq!(
            media.file_size, declared,
            "边下边播展示的必须是声明总长，不是磁盘上的实时长度"
        );
        assert_ne!(
            media.file_size, on_disk,
            "落到整首入口上了（那条路会拿 metadata()，还会撞尾部空洞）"
        );

        core.shutdown();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stale_prepared_is_rejected_and_its_file_is_kept() {
        let mut core = mock_core();
        let dir = unique_dir();
        let path = write_wav(&dir, "stale.wav");
        let gen = core.session.begin_online("347230");
        core.fetch_gen = gen;
        // 模拟换台：generation 前进
        core.session.cancel_fetch();
        let event = core.on_response(&Response::Prepared {
            seq: 0, // 走 accept_prepared 直连路径
            result: Ok(PreparedTrack {
                path: path.clone(),
                source: crate::music::provider::PlaybackSource {
                    url: "https://example.invalid/b.mp3".into(),
                    quality: Some(crate::music::provider::AudioQuality::Standard),
                    content_type: Some("mp3".into()),
                    bitrate_kbps: None,
                    expires_at: None,
                },
                spec: crate::music::models::AudioSpec::default(),
                stream: None,
            }),
        });
        // seq=0 不匹配 prepare_seq → None；直接验证 session 门槛：
        assert_eq!(event, None);
        assert!(!core.session.on_prepared(
            gen,
            PreparedTrack {
                path: path.clone(),
                source: crate::music::provider::PlaybackSource {
                    url: "https://example.invalid/c.mp3".into(),
                    quality: Some(crate::music::provider::AudioQuality::Standard),
                    content_type: Some("mp3".into()),
                    bitrate_kbps: None,
                    expires_at: None,
                },
                spec: crate::music::models::AudioSpec::default(),
                stream: None,
            }
        ));
        core.shutdown();
        assert!(
            path.exists(),
            "过期装载的文件不得被会话删掉（它已进装载器的会话缓存）"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn switching_tracks_aborts_the_previous_download() {
        // 换台的接线：先抬音频水位、再分配新 seq。
        //
        // 水位必须**恰好等于**新请求的 seq（`wait_cancelled` 用严格 `>`，等于才
        // 安全），而旧 seq 严格小于它 → 在途下载必被 select! 打断。顺序写反时
        // 水位会大于新 seq，新请求当场取消自己，表现为"点歌永远失败"。
        let mut core = mock_core();
        let mut bridge = test_bridge();
        core.play_online(&sample_track("a"), &mut bridge);
        let first = core.prepare_seq;
        assert_eq!(
            bridge.audio_watermark(),
            first,
            "第一次装载：水位应恰好落在本次请求上"
        );

        core.play_online(&sample_track("b"), &mut bridge);
        let second = core.prepare_seq;
        assert!(second > first);
        assert_eq!(
            bridge.audio_watermark(),
            second,
            "新请求不得被自己的水位取消"
        );
        assert!(
            first < bridge.audio_watermark(),
            "旧装载必须落在水位之下，才会被取消"
        );
    }

    #[test]
    fn a_cancelled_prepare_leaves_buffering() {
        // 没有这条分支时 prepare_seq 永远留着、session 卡在 Fetching：
        // DECK 一直 BUFFERING，toggle() 被 is_buffering() 吞掉，界面永久卡死。
        let mut core = mock_core();
        let mut bridge = test_bridge();
        core.play_online(&sample_track("186016"), &mut bridge);
        assert!(core.is_buffering(), "装载中应在缓冲态");
        let seq = core.prepare_seq;

        let event = core.on_response(&Response::Cancelled { seq });
        assert_eq!(event, Some(CoreEvent::Cancelled));
        assert!(!core.is_buffering(), "取消后不得永久停在缓冲态");
        assert_eq!(core.prepare_seq, 0, "取消后必须清掉装载 seq");
        assert_eq!(core.deck_status(), "DECK 01 / CASSETTE");
    }

    #[test]
    fn now_playing_shows_the_real_codec_not_the_temp_suffix() {
        // 面板展示的 `describe()` 必须来自真实容器。原来在线临时文件恒叫
        // `.audio`，codec 被推成 `AUDIO`，"codec 为空才用接口 type" 的兜底
        // 又永不成立 —— 面板上永远看不到 FLAC/MP3。
        let mut core = mock_core();
        let mut bridge = test_bridge();
        core.play_online(&sample_track("1"), &mut bridge);
        let seq = core.prepare_seq;
        let dir = unique_dir();
        let path = write_wav(&dir, "track_0001_1.flac");

        let event = core.on_response(&Response::Prepared {
            seq,
            result: Ok(PreparedTrack {
                path: path.clone(),
                source: crate::music::provider::PlaybackSource {
                    url: "https://example.invalid/a.flac".into(),
                    quality: Some(crate::music::provider::AudioQuality::Lossless),
                    content_type: Some("flac".into()),
                    bitrate_kbps: None,
                    expires_at: None,
                },
                spec: crate::music::models::AudioSpec {
                    codec: Some("FLAC".into()),
                    ..crate::music::models::AudioSpec::default()
                },
                stream: None,
            }),
        });
        assert_eq!(event, Some(CoreEvent::Started));

        let shown = core.now_playing().expect("应写入正在播放").spec.describe();
        assert!(shown.contains("FLAC"), "面板必须标出真实容器: {shown}");
        assert!(
            !shown.contains("AUDIO"),
            "不得再出现那个假的 AUDIO: {shown}"
        );
        core.shutdown();
    }

    #[test]
    fn prepare_error_maps_to_human_message() {
        let mut core = mock_core();
        let mut bridge = test_bridge();
        core.play_online(&sample_track("186016"), &mut bridge);
        let seq = core.prepare_seq;
        let event = core.on_response(&Response::Prepared {
            seq,
            result: Err(ProviderError::ContentUnavailable),
        });
        match event {
            Some(CoreEvent::Failed(failure)) => {
                let msg = failure.message(crate::i18n::Lang::Zh);
                assert!(msg.contains("不可播放"), "人话提示: {}", msg);
                assert!(!msg.contains('{'), "不得暴露 JSON: {}", msg);
                // 英文侧同样不得漏原始 JSON
                let en = failure.message(crate::i18n::Lang::En);
                assert!(!en.contains('{'), "英文侧不得暴露 JSON: {}", en);
            }
            other => panic!("应为失败事件: {:?}", other),
        }
        assert!(core.status_text(Lang::En).contains("FAILED"));
    }

    #[test]
    fn cancel_is_silent_and_invalidates_inflight() {
        let mut core = mock_core();
        let mut bridge = test_bridge();
        core.play_online(&sample_track("1"), &mut bridge);
        core.cancel(&mut bridge);
        assert_eq!(core.prepare_seq, 0);
        assert_eq!(core.lyric_seq, 0);
        assert!(!core.is_buffering(), "取消后不再是缓冲态");
        assert_eq!(core.status_text(Lang::En), "CANCELLED");
    }

    #[test]
    fn lyrics_attaches_and_line_at_tracks_clock() {
        let mut core = mock_core();
        let mut bridge = test_bridge();
        core.play_online(&sample_track("347230"), &mut bridge);
        let seq = core.lyric_seq;
        let lyrics = parse_lrc("[00:00.00]first\n[00:05.00]second\n[00:10.00]third");
        let event = core.on_response(&Response::Lyrics {
            seq,
            result: Ok(lyrics),
        });
        assert_eq!(event, Some(CoreEvent::LyricsReady));
        assert_eq!(core.session().lyrics().map(|l| l.lines.len()), Some(3));
    }

    #[test]
    fn empty_lyrics_become_human_note() {
        let mut core = mock_core();
        let mut bridge = test_bridge();
        core.play_online(&sample_track("347230"), &mut bridge);
        let seq = core.lyric_seq;
        core.on_response(&Response::Lyrics {
            seq,
            result: Err(ProviderError::EmptyResult),
        });
        assert_eq!(
            core.session().lyric_note(),
            Some(&crate::music::play_session::LyricNote::Empty)
        );
    }

    #[test]
    fn local_playback_needs_no_network_and_reads_sidecar() {
        let mut core = mock_core();
        let dir = unique_dir();
        let path = write_wav(&dir, "local.wav");
        std::fs::write(dir.join("local.lrc"), "[00:01.00]侧车歌词").unwrap();
        let gen = core.play_local(&path);
        assert_eq!(gen, 1);
        assert_eq!(core.prepare_seq, 0, "本地播放不得发起网络请求");
        assert_eq!(core.lyric_seq, 0);
        assert!(core.now_playing().unwrap().is_local);
        assert_eq!(core.engine_state(), "Playing");
        let lines = core.session().lyrics().map(|l| l.lines.len());
        assert_eq!(lines, Some(1), "同名 .lrc 侧车歌词应被读取");
        core.shutdown();
        assert!(path.exists(), "本地文件永不删除");
    }

    #[test]
    fn local_missing_file_fails_with_human_reason() {
        let mut core = mock_core();
        let gen = core.play_local(Path::new("Q:\\definitely\\missing\\nope.mp3"));
        assert_eq!(gen, 1);
        assert!(matches!(core.session().phase(), FetchPhase::Failed(_)));
        assert!(core.status_text(Lang::En).contains("FAILED"));
    }

    #[test]
    fn toggle_and_seek_operate_on_engine() {
        let mut core = mock_core();
        let dir = unique_dir();
        let path = write_wav(&dir, "seek.wav");
        core.play_local(&path);
        core.toggle().unwrap(); // Playing → Paused
        assert_eq!(core.engine_state(), "Paused");
        core.seek_ratio(0.5).unwrap();
        let p = core.progress();
        assert_eq!(p.position, Duration::from_secs(30));
        core.seek_to(Duration::from_secs(3)).unwrap();
        assert_eq!(core.progress().position, Duration::from_secs(3));
        // 歌词行 seek 同一入口
        assert_eq!(core.progress_ratio(), 0.05);
    }

    #[test]
    fn tick_reports_end_once_and_repeat_restarts() {
        let mut core = mock_core();
        let dir = unique_dir();
        let path = write_wav(&dir, "end.wav");
        core.play_local(&path);
        // MockEngine 走完 60s → Ended
        core.player
            .as_mut()
            .unwrap()
            .engine_mut()
            .advance(Duration::from_secs(61));
        assert_eq!(core.tick(), TickAction::TrackEnded);
        assert_eq!(core.tick(), TickAction::None, "结束事件不得重复上报");

        // 单曲循环：结束即重播
        let mut core = mock_core();
        core.repeat_mode = RepeatMode::One;
        core.play_local(&path);
        core.player
            .as_mut()
            .unwrap()
            .engine_mut()
            .advance(Duration::from_secs(61));
        assert_eq!(core.tick(), TickAction::None);
        assert_eq!(core.engine_state(), "Playing", "repeat 应重新起播");
    }

    #[test]
    fn play_mode_cycles_four_steps_and_never_piles_shuffle_on_repeat_one() {
        // UI 模式键的四档：顺序 → 随机 → 单曲循环 → 列表循环 → 回到顺序
        assert_eq!(PlayMode::Sequential.next(), PlayMode::Shuffle);
        assert_eq!(PlayMode::Shuffle.next(), PlayMode::RepeatOne);
        assert_eq!(PlayMode::RepeatOne.next(), PlayMode::RepeatAll);
        assert_eq!(PlayMode::RepeatAll.next(), PlayMode::Sequential);

        // 每一档落回 (shuffle, repeat) 后再读回来必须是同一档——否则键的显示与行为会错位
        for m in [
            PlayMode::Sequential,
            PlayMode::Shuffle,
            PlayMode::RepeatOne,
            PlayMode::RepeatAll,
        ] {
            let (shuffle, repeat) = m.parts();
            assert_eq!(PlayMode::of(shuffle, repeat), m, "{m:?} 往返不一致");
        }
        // 随机档永远配列表循环：不存在「随机 + 单曲循环」这种自动续播永不换曲的组合
        assert_eq!(PlayMode::Shuffle.parts(), (true, RepeatMode::All));

        let mut core = mock_core();
        assert_eq!(core.play_mode(), PlayMode::Sequential, "默认档应是顺序播放");
        core.set_play_mode(PlayMode::Shuffle);
        assert!(core.shuffle);
        assert_eq!(core.repeat_mode, RepeatMode::All);
        core.set_play_mode(PlayMode::RepeatOne);
        assert!(!core.shuffle, "切到单曲循环必须退出随机");
        assert_eq!(core.play_mode(), PlayMode::RepeatOne);
    }

    #[test]
    fn repeat_mode_codes_match_legacy_prefs() {
        // 码值沿用历史配置：0=顺序不循环 / 1=列表 / 2=单曲；未知值回默认档
        assert_eq!(RepeatMode::default(), RepeatMode::Off);
        for (code, want) in [
            (0u8, RepeatMode::Off),
            (1, RepeatMode::All),
            (2, RepeatMode::One),
            (7, RepeatMode::Off),
        ] {
            assert_eq!(RepeatMode::from_code(code), want, "码值 {code}");
        }
        for mode in [RepeatMode::Off, RepeatMode::All, RepeatMode::One] {
            assert_eq!(RepeatMode::from_code(mode.code()), mode, "码值应自洽");
        }
    }

    #[test]
    fn no_engine_degrades_gracefully() {
        let mut core = PlayerCore::<MockEngine>::new(None);
        assert!(!core.has_engine());
        assert_eq!(core.engine_state(), "NoDevice");
        assert!(core.toggle().is_err());
        assert!(core.seek_ratio(0.5).is_err());
        assert_eq!(core.progress().total, None);
        assert_eq!(core.tick(), TickAction::None);
    }
}
