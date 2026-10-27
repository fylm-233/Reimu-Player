//! 播放控制器：状态机、播放控制与进度管理。
//!
//! 这是播放器的**唯一控制入口**。UI / CLI / 未来的在线模块都通过它操作播放，
//! 不直接触碰 [`crate::audio::AudioEngine`]，从而满足
//! `DESIGN_SPEC.md` 11.2「UI 不直接解码音频」。
//!
//! 进度来源：[`PlaybackController::progress`] 直接读取引擎的输出时钟，
//! 不做 `elapsed += timer` 累加（`DESIGN_SPEC.md` 15.3）。

use std::path::Path;
use std::time::Duration;

use super::media::load_media;
use super::models::{AudioFormat, AudioSpec, MediaInfo, PlaybackState, Progress, Tags};
use crate::audio::{AudioEngine, EngineMedia};
use crate::error::{PlayerError, Result};

/// 播放控制器，泛型绑定音频引擎。
///
/// 泛型而非 `dyn` 是因为引擎在编译期即可确定，
/// `NETEASE_INTEGRATION_SPEC.md` 6.1 允许「不需要运行时动态切换时使用泛型」。
pub struct PlaybackController<E: AudioEngine> {
    engine: E,
    state: PlaybackState,
    current: Option<MediaInfo>,
    /// 引擎权威时长（解码器给出），优先于元数据估计值。
    engine_duration: Option<Duration>,
}

impl<E: AudioEngine> PlaybackController<E> {
    /// 创建控制器。初始状态 [`PlaybackState::Idle`]。
    pub fn new(engine: E) -> Self {
        Self {
            engine,
            state: PlaybackState::Idle,
            current: None,
            engine_duration: None,
        }
    }

    // ---- 状态查询 ----

    /// 当前播放状态。
    pub fn state(&self) -> &PlaybackState {
        &self.state
    }

    /// 当前媒体；未加载时为 `None`。
    pub fn current_media(&self) -> Option<&MediaInfo> {
        self.current.as_ref()
    }

    /// 当前进度快照。
    ///
    /// `position` 取自引擎输出时钟；`total` 优先取解码器权威值，
    /// 其次取元数据估计值，两者皆无时为 `None`（不伪造）。
    pub fn progress(&self) -> Progress {
        Progress {
            position: self.engine.position(),
            total: self
                .engine_duration
                .or_else(|| self.current.as_ref().and_then(|m| m.duration)),
        }
    }

    /// 当前音量。
    pub fn volume(&self) -> f32 {
        self.engine.volume()
    }

    // ---- 控制 ----

    /// 加载媒体文件。
    ///
    /// 会读取元数据并装载到引擎；成功后状态变为 [`PlaybackState::Ready`]。
    /// 加载新媒体会停止当前播放。
    pub fn load(&mut self, path: impl AsRef<Path>) -> Result<&MediaInfo> {
        let path = path.as_ref();
        tracing::info!("加载媒体: {}", path.display());

        // 元数据层：失败会返回明确错误（如文件不存在）
        let info = load_media(path)?;

        // 引擎层：解码失败在这里暴露，而不是等到播放时
        let EngineMedia { duration } = self.engine.load(path)?;

        self.engine_duration = duration;
        self.current = Some(info);
        self.state = PlaybackState::Ready;

        tracing::info!(
            "加载完成: {} | 状态={} | 进度={}",
            self.current
                .as_ref()
                .map(|m| m.display_title())
                .unwrap_or_default(),
            self.state,
            self.progress()
        );
        Ok(self.current.as_ref().expect("刚写入，必然存在"))
    }

    /// 用调用方**已经探测好**的流参数装载（在线装载线程已读过元数据）。
    ///
    /// 与 [`Self::load`] 的唯一区别：**不再调用 `load_media`**。
    ///
    /// 为什么需要它：在线曲目的元数据在装载线程里已经用 lofty 探过一遍
    /// （`OnlineTrackLoader::prepare_once`），而 `load` 会在 **UI 线程**上
    /// 再探一遍——每首歌多一次头部解析 + 一次文件打开，全压在 egui 帧里。
    /// 在线临时文件本来也没有标签，唯一有价值的字段是时长，而时长以解码器
    /// （`EngineMedia::duration`）为准。
    ///
    /// 本地播放仍走 `load`：它需要标签来显示标题/艺人。
    pub fn load_probed(&mut self, path: impl AsRef<Path>, spec: AudioSpec) -> Result<&MediaInfo> {
        let path = path.as_ref();
        tracing::info!("加载媒体（已探测）: {}", path.display());

        // 引擎层：解码失败在这里暴露，而不是等到播放时
        let EngineMedia { duration } = self.engine.load(path)?;

        self.engine_duration = duration;
        self.current = Some(MediaInfo {
            path: path.to_path_buf(),
            format: AudioFormat::from_extension(
                path.extension()
                    .map(|e| e.to_string_lossy().to_string())
                    .unwrap_or_default()
                    .as_str(),
            ),
            duration,
            tags: Tags::default(),
            file_size: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            audio: spec,
        });
        self.state = PlaybackState::Ready;
        Ok(self.current.as_ref().expect("刚写入，必然存在"))
    }

    /// 边下边播：把**还在长的**字节缓冲交给解码泵装载，流参数用调用方探好的那份。
    ///
    /// 与 [`Self::load_probed`] 的差别只有两处：
    /// - 数据交给 [`AudioEngine::load_stream`]，解码住在独立线程上跟着水位走，
    ///   装载与「出声」都不等整首下完（`OnlineTrackLoader::prepare` 已确认过闸门）；
    /// - `file_size` 用**声明总长**而不是 `metadata()`：磁盘上的实时长度永远落后
    ///   于水位，拿它显示就是让人看一个一直在涨的假数字。
    pub fn load_stream_probed(
        &mut self,
        buffer: std::sync::Arc<super::stream_buffer::StreamBuffer>,
        spec: AudioSpec,
    ) -> Result<&MediaInfo> {
        let path = buffer.path().to_path_buf();
        tracing::info!("加载媒体（边下边播）: {}", path.display());

        // 解码器起不了泵（容器不对/头部读不出）在这里暴露，而不是等到播放时
        let EngineMedia { duration } = self.engine.load_stream(buffer.clone())?;
        let file_size = buffer.declared().unwrap_or_else(|| buffer.written());

        self.engine_duration = duration;
        self.current = Some(MediaInfo {
            path: path.clone(),
            format: AudioFormat::from_extension(
                path.extension()
                    .map(|e| e.to_string_lossy().to_string())
                    .unwrap_or_default()
                    .as_str(),
            ),
            duration,
            tags: Tags::default(),
            file_size,
            audio: spec,
        });
        self.state = PlaybackState::Ready;
        Ok(self.current.as_ref().expect("刚写入，必然存在"))
    }

    /// 切歌：装载新媒体并立即开始播放（停止当前内容）。
    ///
    /// 与「`load` + `play`」等价的原子入口（M3「切歌」），返回新媒体描述。
    /// **装载失败时当前播放不受影响**（如文件缺失，旧曲目继续播），
    /// 不因为切歌失败把状态机打落到 `Failed`。
    pub fn switch_to(&mut self, path: impl AsRef<Path>) -> Result<&MediaInfo> {
        self.load(path)?;
        self.play()?;
        Ok(self.current.as_ref().expect("刚写入，必然存在"))
    }

    /// 开始或继续播放。
    pub fn play(&mut self) -> Result<()> {
        self.engine.play().map_err(|e| self.mark_failed(e))?;
        self.state = PlaybackState::Playing;
        tracing::info!("▶ 播放 | {}", self.progress());
        Ok(())
    }

    /// 暂停。
    ///
    /// 仅在 [`PlaybackState::Playing`] 下合法。其他状态（含自然播完后的
    /// [`PlaybackState::Ended`]）返回 [`PlayerError::InvalidState`]，
    /// 且**不**改动状态机——与 [`PlaybackController::toggle`] 的拒绝行为一致：
    /// 非法操作是调用方错误，不是播放失败，不应把状态打落到 `Failed`。
    pub fn pause(&mut self) -> Result<()> {
        if self.state != PlaybackState::Playing {
            return Err(PlayerError::InvalidState {
                state: self.state.name().to_string(),
                action: "pause".into(),
            });
        }
        self.engine.pause().map_err(|e| self.mark_failed(e))?;
        self.state = PlaybackState::Paused;
        tracing::info!("⏸ 暂停 | {}", self.progress());
        Ok(())
    }

    /// 播放/暂停切换。返回切换后的状态。
    pub fn toggle(&mut self) -> Result<&PlaybackState> {
        match self.state {
            PlaybackState::Playing => self.pause()?,
            PlaybackState::Ready | PlaybackState::Paused | PlaybackState::Ended => self.play()?,
            PlaybackState::Idle | PlaybackState::Failed(_) => {
                return Err(PlayerError::InvalidState {
                    state: self.state.name().to_string(),
                    action: "toggle".into(),
                })
            }
        }
        Ok(&self.state)
    }

    /// 停止播放，进度归零，媒体保持已加载（状态回到 [`PlaybackState::Ready`]）。
    pub fn stop(&mut self) -> Result<()> {
        self.engine.stop().map_err(|e| self.mark_failed(e))?;
        if self.current.is_some() {
            self.state = PlaybackState::Ready;
        } else {
            self.state = PlaybackState::Idle;
        }
        tracing::info!("⏹ 停止");
        Ok(())
    }

    /// 跳转到指定位置。
    pub fn seek(&mut self, position: Duration) -> Result<()> {
        self.engine
            .seek(position)
            .map_err(|e| self.mark_failed(e))?;
        // 跳转到末尾之后仍可再次播放，回到 Ready 语义更自然
        if matches!(self.state, PlaybackState::Ended) {
            self.state = PlaybackState::Ready;
        }
        tracing::info!(
            "⏩ 跳转到 {} | {}",
            super::models::format_duration(position),
            self.progress()
        );
        Ok(())
    }

    /// 跳转到总时长的指定百分比（0–100）。
    ///
    /// 总时长未知时返回 [`PlayerError::InvalidState`]，不猜测位置。
    pub fn seek_percent(&mut self, percent: f32) -> Result<()> {
        if !(0.0..=100.0).contains(&percent) {
            return Err(PlayerError::InvalidPosition(format!(
                "百分比超出范围 [0, 100]: {}",
                percent
            )));
        }
        let total = self
            .progress()
            .total
            .ok_or_else(|| PlayerError::InvalidState {
                state: self.state.name().to_string(),
                action: "seek_percent(总时长未知)".into(),
            })?;
        let target = total.mul_f32(percent / 100.0);
        self.seek(target)
    }

    /// 设置音量（0.0–1.0）。
    pub fn set_volume(&mut self, volume: f32) -> Result<()> {
        self.engine
            .set_volume(volume)
            .map_err(|e| self.mark_failed(e))?;
        tracing::debug!("音量 -> {:.0}%", volume * 100.0);
        Ok(())
    }

    /// 轮询播放状态。
    ///
    /// 引擎自然播完时把状态推进到 [`PlaybackState::Ended`]。
    /// 调用方（UI 事件循环 / CLI 定时器）应周期性调用；
    /// 该方法不做任何阻塞等待，开销可忽略。
    pub fn poll(&mut self) -> &PlaybackState {
        if matches!(self.state, PlaybackState::Playing) && self.engine.is_finished() {
            tracing::info!("■ 播放结束 | 时长={}", self.progress());
            self.state = PlaybackState::Ended;
        }
        &self.state
    }

    /// 清空当前媒体，回到 [`PlaybackState::Idle`]。
    pub fn clear(&mut self) -> Result<()> {
        self.engine.stop().map_err(|e| self.mark_failed(e))?;
        self.current = None;
        self.engine_duration = None;
        self.state = PlaybackState::Idle;
        tracing::info!("已清空播放内容");
        Ok(())
    }

    /// 暴露引擎供诊断使用（例如打印引擎调用日志）。
    pub fn engine(&self) -> &E {
        &self.engine
    }

    /// 暴露可变引擎（模拟测试推进时钟 / 注入错误）。
    pub fn engine_mut(&mut self) -> &mut E {
        &mut self.engine
    }

    /// 把错误落到 `Failed` 状态并返回原错误。
    ///
    /// **不吞错误**：调用方仍会拿到 `Err`，可以决定是否重试；
    /// 这里只是让状态机如实反映「当前不可用」。
    fn mark_failed(&mut self, error: PlayerError) -> PlayerError {
        tracing::warn!("播放操作失败: {}", error);
        self.state = PlaybackState::Failed(error.to_string());
        error
    }
}

impl<E: AudioEngine> std::fmt::Debug for PlaybackController<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaybackController")
            .field("state", &self.state)
            .field("media", &self.current.as_ref().map(|m| m.display_title()))
            .field("progress", &self.progress())
            .finish()
    }
}

/// 便捷类型：使用真实音频引擎的控制器。
pub type RealController = PlaybackController<crate::audio::RodioEngine>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::mock_engine::{EngineCall, MockEngine};
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;

    fn write_wav(dir: &Path, name: &str) -> PathBuf {
        let sample_rate: u32 = 8000;
        let data_len = sample_rate * 2;
        let path = dir.join(name);
        let mut f = fs::File::create(&path).unwrap();
        let mut h = Vec::new();
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&(36 + data_len).to_le_bytes());
        h.extend_from_slice(b"WAVEfmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes());
        h.extend_from_slice(&sample_rate.to_le_bytes());
        h.extend_from_slice(&sample_rate.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes());
        h.extend_from_slice(&8u16.to_le_bytes());
        h.extend_from_slice(b"data");
        h.extend_from_slice(&data_len.to_le_bytes());
        f.write_all(&h).unwrap();
        f.write_all(&vec![128u8; data_len as usize]).unwrap();
        path
    }

    /// 每次调用生成唯一临时目录。
    ///
    /// 测试默认并行执行，共用同一路径会互相抢文件锁（Windows 上表现为
    /// `PermissionDenied`），因此用进程内自增序号隔离。
    fn unique_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("reimuplayer_ctrl_{}_{}", std::process::id(), n));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn setup() -> (PlaybackController<MockEngine>, PathBuf) {
        let dir = unique_dir();
        let path = write_wav(&dir, "demo.wav");
        (
            PlaybackController::new(MockEngine::new().with_duration(Some(Duration::from_secs(60)))),
            path,
        )
    }

    #[test]
    fn initial_state_is_idle() {
        let (c, _) = setup();
        assert_eq!(*c.state(), PlaybackState::Idle);
        assert!(c.current_media().is_none());
    }

    #[test]
    fn load_moves_to_ready_and_reports_progress() {
        let (mut c, path) = setup();
        c.load(&path).unwrap();
        assert_eq!(*c.state(), PlaybackState::Ready);
        assert!(c.current_media().is_some());

        let p = c.progress();
        assert_eq!(p.position, Duration::ZERO);
        // 解码器权威时长 60s 优先于 WAV 元数据估计值
        assert_eq!(p.total, Some(Duration::from_secs(60)));
    }

    #[test]
    fn load_probed_uses_the_given_spec_and_skips_the_probe() {
        // 在线路径：装载线程已用 lofty 探过一遍，这里不得再探。
        // 传进来的 codec 必须原样落地——`load` 会从扩展名重算成 "WAV"。
        let (mut c, path) = setup();
        let spec = AudioSpec {
            codec: Some("FLAC".into()),
            sample_rate: Some(44_100),
            ..AudioSpec::default()
        };
        c.load_probed(&path, spec).unwrap();

        assert_eq!(*c.state(), PlaybackState::Ready);
        let media = c.current_media().expect("必须写入 MediaInfo");
        assert_eq!(media.audio.codec.as_deref(), Some("FLAC"));
        assert_eq!(media.audio.sample_rate, Some(44_100));
        assert!(media.tags.title.is_none(), "在线临时文件没有标签，不得伪造");
        assert!(media.file_size > 0, "文件大小取自真实文件");
        // 时长仍以解码器为准，与 `load` 一致
        assert_eq!(c.progress().total, Some(Duration::from_secs(60)));
    }

    #[test]
    fn load_probed_propagates_engine_errors() {
        let (mut c, path) = setup();
        c.engine_mut().fail_next("load");
        let err = c.load_probed(&path, AudioSpec::default()).unwrap_err();
        assert!(matches!(err, PlayerError::OutputError(_)));
        // 与 `load` 一致：引擎层失败直接返回，不动状态机、不写 current
        assert_eq!(*c.state(), PlaybackState::Idle);
        assert!(c.current_media().is_none());
    }

    #[test]
    fn play_pause_toggle_transitions() {
        let (mut c, path) = setup();
        c.load(&path).unwrap();

        c.play().unwrap();
        assert_eq!(*c.state(), PlaybackState::Playing);

        c.pause().unwrap();
        assert_eq!(*c.state(), PlaybackState::Paused);

        c.toggle().unwrap();
        assert_eq!(*c.state(), PlaybackState::Playing);
    }

    #[test]
    fn stop_returns_to_ready_and_resets_position() {
        let (mut c, path) = setup();
        c.load(&path).unwrap();
        c.play().unwrap();
        c.seek(Duration::from_secs(30)).unwrap();

        c.stop().unwrap();
        assert_eq!(*c.state(), PlaybackState::Ready);
        assert_eq!(c.progress().position, Duration::ZERO);
    }

    #[test]
    fn switch_to_replaces_media_and_plays_from_start() {
        let (mut c, path) = setup();
        c.load(&path).unwrap();
        c.play().unwrap();
        c.seek(Duration::from_secs(10)).unwrap();

        let dir = unique_dir();
        let next = write_wav(&dir, "next.wav");
        let info_path = c.switch_to(&next).unwrap().path.clone();
        assert_eq!(*c.state(), PlaybackState::Playing, "切歌后应处于播放态");
        assert_eq!(info_path, next);
        assert_eq!(c.progress().position, Duration::ZERO, "切歌后从头播放");
    }

    #[test]
    fn switch_to_failure_keeps_current_playback_intact() {
        let (mut c, path) = setup();
        c.load(&path).unwrap();
        c.play().unwrap();

        let err = c.switch_to("no/such/next.mp3").unwrap_err();
        assert!(matches!(err, PlayerError::MediaNotFound(_)));
        assert_eq!(
            *c.state(),
            PlaybackState::Playing,
            "切歌失败不得影响当前曲目，更不得打落到 Failed"
        );
        assert_eq!(
            c.current_media().map(|m| m.path.clone()),
            Some(path.clone()),
            "当前曲目保持不变"
        );
    }

    #[test]
    fn progress_follows_engine_clock_not_a_timer() {
        let (mut c, path) = setup();
        c.load(&path).unwrap();
        c.play().unwrap();

        // 直接推进引擎时钟，控制器必须如实反映，而不是自己累加
        c.engine_mut_for_test().advance(Duration::from_secs(12));
        assert_eq!(c.progress().position, Duration::from_secs(12));

        c.engine_mut_for_test().advance(Duration::from_secs(3));
        assert_eq!(c.progress().position, Duration::from_secs(15));
    }

    #[test]
    fn poll_promotes_to_ended_when_engine_finishes() {
        let (mut c, path) = setup();
        c.load(&path).unwrap();
        c.play().unwrap();
        assert_eq!(c.poll(), &PlaybackState::Playing);

        c.engine_mut_for_test().advance(Duration::from_secs(60));
        assert_eq!(c.poll(), &PlaybackState::Ended);
    }

    #[test]
    fn playing_after_end_restarts() {
        let (mut c, path) = setup();
        c.load(&path).unwrap();
        c.play().unwrap();
        c.engine_mut_for_test().advance(Duration::from_secs(60));
        c.poll();
        assert_eq!(*c.state(), PlaybackState::Ended);

        c.play().unwrap();
        assert_eq!(*c.state(), PlaybackState::Playing);
        assert_eq!(c.progress().position, Duration::ZERO);
    }

    #[test]
    fn seek_percent_uses_total_duration() {
        let (mut c, path) = setup();
        c.load(&path).unwrap();
        c.seek_percent(50.0).unwrap();
        assert_eq!(c.progress().position, Duration::from_secs(30));

        assert!(c.seek_percent(120.0).is_err());
    }

    #[test]
    fn seek_percent_fails_when_total_unknown() {
        // 用无法解析元数据的扩展名 + 不提供时长的引擎，
        // 构造「总时长完全未知」的真实场景。
        let dir = unique_dir();
        let path = dir.join("noundur.zzz");
        fs::write(&path, b"not audio").unwrap();

        let mut c = PlaybackController::new(MockEngine::new()); // 引擎不提供时长
        c.load(&path).unwrap();

        assert_eq!(c.progress().total, None, "总时长应为未知");
        let err = c.seek_percent(50.0).unwrap_err();
        assert!(matches!(err, PlayerError::InvalidState { .. }));

        // 百分比越界与 NaN 同样被拒绝
        assert!(c.seek_percent(120.0).is_err());
        assert!(c.seek_percent(f32::NAN).is_err());
    }

    #[test]
    fn engine_error_marks_failed_and_propagates() {
        let (mut c, path) = setup();
        c.load(&path).unwrap();
        c.engine_mut_for_test().fail_next("play");

        let err = c.play().unwrap_err();
        assert!(matches!(err, PlayerError::OutputError(_)));
        assert!(matches!(c.state(), PlaybackState::Failed(_)));
    }

    #[test]
    fn load_missing_file_is_media_not_found() {
        let (mut c, _) = setup();
        let err = c.load("no/such/file.mp3").unwrap_err();
        assert!(matches!(err, PlayerError::MediaNotFound(_)));
    }

    #[test]
    fn clear_returns_to_idle() {
        let (mut c, path) = setup();
        c.load(&path).unwrap();
        c.play().unwrap();
        c.clear().unwrap();
        assert_eq!(*c.state(), PlaybackState::Idle);
        assert!(c.current_media().is_none());
        assert_eq!(c.progress().total, None);
    }

    #[test]
    fn toggle_in_idle_is_rejected() {
        let (mut c, _) = setup();
        assert!(c.toggle().is_err());
    }

    #[test]
    fn pause_after_ended_is_rejected_without_breaking_state() {
        // 真实 CLI 验证曾暴露的契约：媒体自然播完（Ended）后执行 pause 是非法操作。
        // 必须返回 InvalidState，且不得把状态机打落到 Failed（非法操作 ≠ 播放失败）。
        let (mut c, path) = setup();
        c.load(&path).unwrap();
        c.play().unwrap();
        c.engine_mut_for_test().advance(Duration::from_secs(60));
        c.poll();
        assert_eq!(*c.state(), PlaybackState::Ended);

        let err = c.pause().unwrap_err();
        match err {
            PlayerError::InvalidState { state, action } => {
                assert_eq!(state, "Ended");
                assert_eq!(action, "pause");
            }
            other => panic!("期望 InvalidState，实际得到: {:?}", other),
        }
        assert_eq!(
            *c.state(),
            PlaybackState::Ended,
            "非法 pause 不得改变状态机"
        );

        // 状态未被破坏后，仍可正常重新起播
        c.play().unwrap();
        assert_eq!(*c.state(), PlaybackState::Playing);
    }

    #[test]
    fn volume_control_delegates_and_validates() {
        let (mut c, path) = setup();
        c.load(&path).unwrap();
        c.set_volume(0.3).unwrap();
        assert_eq!(c.volume(), 0.3);
        assert!(c.set_volume(2.0).is_err());
    }

    #[test]
    fn engine_calls_are_recorded() {
        let (mut c, path) = setup();
        c.load(&path).unwrap();
        c.play().unwrap();
        c.pause().unwrap();
        let calls = c.engine().calls();
        assert!(calls.contains(&EngineCall::Play));
        assert!(calls.contains(&EngineCall::Pause));
    }
}

impl<E: AudioEngine> PlaybackController<E> {
    /// 仅测试可见：拿到可变引擎以便推进模拟时钟 / 注入错误。
    #[cfg(test)]
    fn engine_mut_for_test(&mut self) -> &mut E {
        &mut self.engine
    }
}
