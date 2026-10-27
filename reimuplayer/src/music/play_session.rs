//! 播放会话状态机（`REIMU…工程实施规范` 8.1/8.2 的落点）。
//!
//! 职责：把「点播一首在线曲目」到「可交给 [`crate::music::PlaybackController`]」
//! 之间的**编排状态**收敛为单一可测模型：
//!
//! - **generation 防过期**：每次点播递增代号；晚到的旧响应不得覆盖新状态
//!   （规范 8.2「新请求发出后，旧请求即使晚到，也不得覆盖新页面状态」）。
//! - **取消**：换台/用户取消即提升代号，在途请求的结果被拒绝；
//!   桥层另有协作式取消（[`crate::gui::bridge`]），二者独立生效。
//! - **临时文件生命周期**：**不归本模块**。在线音频落在
//!   [`crate::music::OnlineTrackLoader`] 的暂存目录里，由装载器（会话缓存 +
//!   `cleanup()`）唯一持有与删除。会话只记"当前在播的是哪个路径"，用于展示与
//!   诊断——两个删除者并存时，被换台丢弃的那份两边都不认，只能等进程退出。
//! - **歌词槽**：歌词与曲目同代号绑定；歌词缺失/失败只做提示，
//!   **不影响播放**（规范 7.4）。
//!
//! 本模块**不做 I/O、不发网络、不碰音频引擎**——只管理编排状态，
//! 因此可以离线确定性测试；实际网络在桥工作线程、实际出声在音频引擎。

use std::path::PathBuf;

use crate::i18n::{pick, Lang};
use crate::music::lyrics::Lyrics;
use crate::music::online_playback::PreparedTrack;
use crate::music::provider::ProviderError;

/// 歌词侧提示（**类型化**：核心层不产出用户可见文案）。
///
/// 与 [`ProviderError::message`] 同一分层纪律：这里只说"发生了什么"，
/// 具体措辞由 UI 按界面语言渲染（`player_core` 的歌词区 fallback）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LyricNote {
    /// 本地文件没有在线歌词（不发起请求）。
    LocalFile,
    /// 服务端没有返回可用歌词。
    Empty,
    /// 歌词请求失败（保留错误类型，便于按语言渲染）。
    Failed(ProviderError),
}

impl LyricNote {
    /// 按界面语言渲染人话。
    pub fn message(&self, lang: Lang) -> String {
        match self {
            LyricNote::LocalFile => pick(
                lang,
                "本地文件无在线歌词",
                "Local files have no online lyrics",
            )
            .to_string(),
            LyricNote::Empty => pick(lang, "暂无歌词", "No lyrics available").to_string(),
            LyricNote::Failed(e) => {
                format!(
                    "{}{}",
                    pick(lang, "歌词加载失败：", "Lyrics failed: "),
                    e.message(lang)
                )
            }
        }
    }
}

/// 在线装载阶段（缓冲/失败可视化的数据源）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FetchPhase {
    /// 无在途装载，且尚无就绪曲目。
    #[default]
    Empty,
    /// 取播放地址 + 下载中（界面按「缓冲中」呈现）。
    Fetching,
    /// 音频字节已就绪，正在装载进音频引擎。
    Loading,
    /// 已就绪（装载器交出了本地媒体路径，等待/已经交给引擎）。
    Ready,
    /// 装载失败。携带人话原因（`ProviderError` 的 Display，不含 JSON/堆栈）。
    Failed(String),
}

impl FetchPhase {
    /// 状态名（诊断/测试用）。
    pub fn name(&self) -> &'static str {
        match self {
            FetchPhase::Empty => "Empty",
            FetchPhase::Fetching => "Fetching",
            FetchPhase::Loading => "Loading",
            FetchPhase::Ready => "Ready",
            FetchPhase::Failed(_) => "Failed",
        }
    }

    /// 是否仍在途（Fetching/Loading）。
    pub fn is_busy(&self) -> bool {
        matches!(self, FetchPhase::Fetching | FetchPhase::Loading)
    }
}

/// 播放会话：在线编排状态 + 代号 + 当前媒体引用 + 歌词槽。
#[derive(Debug, Default)]
pub struct PlaySession {
    /// 当前代号。每次点播/取消递增；响应代号不匹配一律拒绝。
    gen: u64,
    /// 当前点播的曲目 ID（本地装载为 `None`）。
    song_id: Option<String>,
    phase: FetchPhase,
    /// 当前曲目的媒体路径（在线=临时文件；本地=用户文件）。
    ///
    /// **只做引用，不做处置**：文件生命周期归 [`crate::music::OnlineTrackLoader`]。
    media: Option<SessionMedia>,
    /// 当前歌词（与 `gen` 同代）。
    lyrics: Option<Lyrics>,
    /// 歌词侧的提示（缺失/失败），仅展示用。
    lyric_note: Option<LyricNote>,
}

/// 会话当前持有的媒体。
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionMedia {
    /// 在线临时文件（由装载器的会话缓存持有）。
    Temp(PathBuf),
    /// 本地用户文件。
    Local(PathBuf),
}

impl PlaySession {
    /// 新建空会话。
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前代号。
    pub fn gen(&self) -> u64 {
        self.gen
    }

    /// 当前装载阶段。
    pub fn phase(&self) -> &FetchPhase {
        &self.phase
    }

    /// 当前点播曲目 ID（本地装载或空会话为 `None`）。
    pub fn song_id(&self) -> Option<&str> {
        self.song_id.as_deref()
    }

    /// 当前媒体路径（若已就绪/装载中）。
    pub fn media_path(&self) -> Option<&std::path::Path> {
        self.media.as_ref().map(|m| match m {
            SessionMedia::Temp(p) | SessionMedia::Local(p) => p.as_path(),
        })
    }

    /// 当前歌词（若有）。
    pub fn lyrics(&self) -> Option<&Lyrics> {
        self.lyrics.as_ref()
    }

    /// 歌词侧提示（缺失/失败时非 `None`；不影响播放）。
    pub fn lyric_note(&self) -> Option<&LyricNote> {
        self.lyric_note.as_ref()
    }

    // ------------------------------------------------------------------
    // 点播 / 取消
    // ------------------------------------------------------------------

    /// 开始点播一首**在线**曲目：提升代号并进入 [`FetchPhase::Fetching`]。
    ///
    /// 返回新代号（调用方附在请求上）。此前在途装载作废、当前媒体引用松开。
    pub fn begin_online(&mut self, song_id: impl Into<String>) -> u64 {
        self.gen = self.gen.wrapping_add(1).max(1);
        self.song_id = Some(song_id.into());
        self.phase = FetchPhase::Fetching;
        self.clear_media();
        self.lyrics = None;
        self.lyric_note = None;
        self.gen
    }

    /// 装载一首**本地**文件：提升代号并直接进入 [`FetchPhase::Loading`]。
    ///
    /// 本地路径**不受在线取消影响也不会被删除**——本地播放独立可用。
    pub fn begin_local(&mut self, path: impl Into<PathBuf>) -> u64 {
        self.gen = self.gen.wrapping_add(1).max(1);
        self.song_id = None;
        self.phase = FetchPhase::Loading;
        self.clear_media();
        self.lyrics = None;
        self.lyric_note = Some(LyricNote::LocalFile);
        self.media = Some(SessionMedia::Local(path.into()));
        self.gen
    }

    /// 取消在途装载（换台后的旧请求、用户主动取消）。
    ///
    /// 提升代号使一切在途响应失效；若当前无就绪曲目则回到 [`FetchPhase::Empty`]。
    /// **不影响**已在播放的引擎状态（引擎侧由调用方决定 stop 与否）。
    pub fn cancel_fetch(&mut self) {
        self.gen = self.gen.wrapping_add(1).max(1);
        if !matches!(self.phase, FetchPhase::Ready) {
            self.phase = FetchPhase::Empty;
            self.song_id = None;
        }
    }

    // ------------------------------------------------------------------
    // 装载结果（桥响应入口；代号不匹配一律拒绝）
    // ------------------------------------------------------------------

    /// 装载成功回调。代号匹配且处于 `Fetching` 才接受，返回是否接受。
    ///
    /// 过期结果**不碰文件**：它已经进了装载器的会话缓存，下次播同一首就是命中；
    /// 在这里删掉会破坏缓存，还可能删到别的 lane 正在用的那份。
    pub fn on_prepared(&mut self, gen: u64, prepared: PreparedTrack) -> bool {
        if !self.accepts(gen) || self.phase != FetchPhase::Fetching {
            return false;
        }
        self.phase = FetchPhase::Loading;
        self.media = Some(SessionMedia::Temp(prepared.path));
        true
    }

    /// 装载失败回调。[`ProviderError::Cancelled`] 不算失败（静默回 [`FetchPhase::Empty`]）。
    ///
    /// 返回是否接受（`false` = 过期响应已丢弃）。
    pub fn on_prepare_error(&mut self, gen: u64, error: ProviderError) -> bool {
        if !self.accepts(gen) || self.phase != FetchPhase::Fetching {
            return false;
        }
        match error {
            ProviderError::Cancelled => {
                self.phase = FetchPhase::Empty;
                self.song_id = None;
            }
            other => self.phase = FetchPhase::Failed(other.to_string()),
        }
        true
    }

    /// 媒体已装载进引擎（[`FetchPhase::Loading`] → [`FetchPhase::Ready`]）。
    ///
    /// 代号不匹配或不在 `Loading` 时返回 `false`。
    pub fn on_loaded(&mut self, gen: u64) -> bool {
        if !self.accepts(gen) || self.phase != FetchPhase::Loading {
            return false;
        }
        self.phase = FetchPhase::Ready;
        true
    }

    /// 引擎装载失败（解码失败/文件损坏等）。
    pub fn on_load_error(&mut self, gen: u64, reason: impl Into<String>) -> bool {
        if !self.accepts(gen) {
            return false;
        }
        self.phase = FetchPhase::Failed(reason.into());
        self.clear_media();
        true
    }

    // ------------------------------------------------------------------
    // 歌词槽
    // ------------------------------------------------------------------

    /// 歌词到达（与曲目同代才接受；缺失/失败只做提示，不影响播放）。
    pub fn on_lyrics(&mut self, gen: u64, result: Result<Lyrics, ProviderError>) -> bool {
        if !self.accepts(gen) {
            return false;
        }
        match result {
            Ok(lyrics) => {
                if lyrics.is_empty() {
                    self.lyric_note = Some(LyricNote::Empty);
                    self.lyrics = None;
                } else {
                    self.lyric_note = None;
                    self.lyrics = Some(lyrics);
                }
            }
            Err(ProviderError::EmptyResult) | Err(ProviderError::Cancelled) => {
                self.lyric_note = Some(LyricNote::Empty);
                self.lyrics = None;
            }
            Err(other) => {
                self.lyric_note = Some(LyricNote::Failed(other));
                self.lyrics = None;
            }
        }
        true
    }

    // ------------------------------------------------------------------
    // 媒体引用
    // ------------------------------------------------------------------

    /// 松开当前媒体引用（换台/停止/装载失败时调用）。
    ///
    /// **不删除任何文件**：在线临时文件归 [`crate::music::OnlineTrackLoader`]
    /// 的会话缓存管，本地文件本来就不该被删。这里只清掉"当前在播的是哪个路径"。
    pub fn release_media(&mut self) {
        self.clear_media();
    }

    /// 内部：松开媒体引用。
    fn clear_media(&mut self) {
        self.media = None;
    }

    /// 内部：代号是否可接受（等于当前代号）。
    fn accepts(&self, gen: u64) -> bool {
        gen == self.gen
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::music::provider::{PlaybackSource, ProviderId};

    fn prepared(path: &str) -> PreparedTrack {
        PreparedTrack {
            path: PathBuf::from(path),
            source: PlaybackSource {
                url: "http://cdn/x.mp3".into(),
                expires_at: None,
                quality: None,
                content_type: Some("mp3".into()),
                bitrate_kbps: None,
            },
            spec: crate::music::models::AudioSpec::default(),
            stream: None,
        }
    }

    fn lyric_line(ms: u64, text: &str) -> Lyrics {
        Lyrics {
            kind: crate::music::lyrics::LyricKind::Plain,
            lines: vec![crate::music::lyrics::LyricLine {
                start_ms: ms,
                text: text.into(),
            }],
            offset_ms: 0,
            dropped_lines: 0,
        }
    }

    #[test]
    fn begin_online_bumps_gen_and_starts_fetching() {
        let mut s = PlaySession::new();
        let g1 = s.begin_online("347230");
        assert!(g1 >= 1);
        assert_eq!(s.phase(), &FetchPhase::Fetching);
        assert_eq!(s.song_id(), Some("347230"));

        let g2 = s.begin_online("2");
        assert!(g2 > g1, "换台必须提升代号");
        assert_eq!(s.phase(), &FetchPhase::Fetching);
    }

    #[test]
    fn stale_prepare_result_is_rejected_and_keeps_its_file() {
        let mut s = PlaySession::new();
        let g_old = s.begin_online("1");
        let _g_new = s.begin_online("2");

        // 旧代号的装载结果必须被拒绝——但**不得**处置文件：它已经进了装载器的
        // 会话缓存，下次播同一首就是命中；删掉等于把刚下好的字节又扔了。
        assert!(!s.on_prepared(g_old, prepared("stale.audio")));
        assert_eq!(s.phase(), &FetchPhase::Fetching, "过期响应不得改变状态");
        assert!(s.media_path().is_none(), "过期结果不得写进当前媒体");
    }

    #[test]
    fn prepare_error_becomes_human_readable_failure() {
        let mut s = PlaySession::new();
        let g = s.begin_online("1");
        assert!(s.on_prepare_error(g, ProviderError::PlaybackUrlUnavailable));
        match s.phase() {
            FetchPhase::Failed(msg) => {
                assert!(msg.contains("未获得播放地址"), "必须是人话: {}", msg);
                assert!(!msg.contains('{'), "不得暴露 JSON: {}", msg);
            }
            other => panic!("期望 Failed，得到 {}", other.name()),
        }
    }

    #[test]
    fn cancelled_prepare_error_is_silent_not_failure() {
        let mut s = PlaySession::new();
        let g = s.begin_online("1");
        assert!(s.on_prepare_error(g, ProviderError::Cancelled));
        assert_eq!(s.phase(), &FetchPhase::Empty, "取消不是失败");
    }

    #[test]
    fn happy_path_prepared_loaded_ready() {
        let mut s = PlaySession::new();
        let g = s.begin_online("347230");
        assert!(s.on_prepared(g, prepared("t.audio")));
        assert_eq!(s.phase(), &FetchPhase::Loading);
        assert!(s.on_loaded(g));
        assert_eq!(s.phase(), &FetchPhase::Ready);
        assert_eq!(s.media_path(), Some(std::path::Path::new("t.audio")));
    }

    #[test]
    fn load_error_clears_the_media_reference() {
        let mut s = PlaySession::new();
        let g = s.begin_online("1");
        assert!(s.on_prepared(g, prepared("bad.audio")));
        assert!(s.on_load_error(g, "解码失败"));
        match s.phase() {
            FetchPhase::Failed(msg) => assert_eq!(msg, "解码失败"),
            other => panic!("期望 Failed，得到 {}", other.name()),
        }
        assert!(s.media_path().is_none(), "装载失败必须松开媒体引用");
    }

    #[test]
    fn lyrics_missing_is_note_not_error() {
        let mut s = PlaySession::new();
        let g = s.begin_online("1");
        assert!(s.on_lyrics(g, Err(ProviderError::EmptyResult)));
        assert_eq!(s.lyric_note(), Some(&LyricNote::Empty));
        assert!(s.lyrics().is_none());
        assert_eq!(s.phase(), &FetchPhase::Fetching, "歌词缺失不得影响播放状态");
    }

    #[test]
    fn lyrics_follow_current_gen_only() {
        let mut s = PlaySession::new();
        let g_old = s.begin_online("1");
        let g_new = s.begin_online("2");
        assert!(
            !s.on_lyrics(g_old, Ok(lyric_line(0, "旧歌词"))),
            "旧代歌词必须拒绝"
        );
        assert!(s.on_lyrics(g_new, Ok(lyric_line(0, "新歌词"))));
        assert_eq!(s.lyrics().unwrap().lines[0].text, "新歌词");
        assert_eq!(s.lyric_note(), None);
    }

    #[test]
    fn local_media_is_never_deleted_and_needs_no_network() {
        let dir =
            std::env::temp_dir().join(format!("reimuplayer_session_local_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("local.mp3");
        std::fs::write(&file, b"ID3local").unwrap();

        let mut s = PlaySession::new();
        let g = s.begin_local(&file);
        assert_eq!(s.phase(), &FetchPhase::Loading);
        assert!(s.song_id().is_none(), "本地装载不点播在线曲目");

        // 松开引用不得动文件——本地播放独立可用
        s.release_media();
        assert!(s.media_path().is_none());
        assert!(file.exists(), "本地文件必须原样保留（本地播放独立可用）");
        let _ = g;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancel_fetch_invalidates_inflight_and_clears_phase() {
        let mut s = PlaySession::new();
        let g = s.begin_online("1");
        s.cancel_fetch();
        assert!(
            !s.on_prepared(g, prepared("late.audio")),
            "取消后一切在途结果失效"
        );
        assert_eq!(s.phase(), &FetchPhase::Empty);
    }

    #[test]
    fn release_media_drops_the_path_without_deleting() {
        // 这条钉死"会话不再删文件"：松开引用后路径没了、文件还在。
        // 删除权已经整体移交给装载器（会话缓存 + cleanup）。
        let dir =
            std::env::temp_dir().join(format!("reimuplayer_session_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t.audio");
        std::fs::write(&file, b"ID3").unwrap();

        let mut s = PlaySession::new();
        let g = s.begin_online("1");
        assert!(s.on_prepared(g, prepared(&file.to_string_lossy())));
        assert_eq!(s.media_path(), Some(file.as_path()));

        s.release_media();
        assert!(s.media_path().is_none(), "引用必须松开");
        assert!(file.exists(), "文件必须还在（归装载器管）");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn release_media_keeps_ready_phase_for_display() {
        let mut s = PlaySession::new();
        let g = s.begin_online("1");
        s.on_prepared(g, prepared("a.audio"));
        s.on_loaded(g);
        s.release_media();
        assert_eq!(s.phase(), &FetchPhase::Ready);
        assert!(s.media_path().is_none());
    }

    #[test]
    fn busy_phase_reflects_fetching_and_loading() {
        let mut s = PlaySession::new();
        assert!(!s.phase().is_busy());
        let g = s.begin_online("1");
        assert!(s.phase().is_busy());
        s.on_prepared(g, prepared("x"));
        assert!(s.phase().is_busy());
        s.on_loaded(g);
        assert!(!s.phase().is_busy());
    }

    #[test]
    fn provider_id_type_used_for_compile_coverage() {
        // 确保测试模块对 provider 类型的引用与库导出一致
        let _ = ProviderId::Netease;
    }

    #[test]
    fn lyric_note_renders_in_both_languages() {
        let notes = [
            LyricNote::LocalFile,
            LyricNote::Empty,
            LyricNote::Failed(ProviderError::RequestTimeout),
        ];
        for note in notes {
            let zh = note.message(Lang::Zh);
            let en = note.message(Lang::En);
            assert!(!zh.is_empty() && !en.is_empty());
            assert_ne!(zh, en, "中英文案不得相同: {zh}");
            assert!(
                !en.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "英文文案不得含中文: {en}"
            );
        }
    }

    #[test]
    fn local_file_note_is_typed_not_a_string() {
        // 类型化而非拼字符串：UI 才能按语言渲染
        let mut s = PlaySession::new();
        s.begin_local("a.mp3");
        assert_eq!(s.lyric_note(), Some(&LyricNote::LocalFile));
    }
}
