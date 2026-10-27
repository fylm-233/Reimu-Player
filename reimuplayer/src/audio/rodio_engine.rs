//! 基于 `rodio` 的真实音频引擎。
//!
//! 职责边界（`DESIGN_SPEC.md` 11.2）：
//! - 本模块只做解码与输出，不碰 UI、不碰元数据标签、不写日志文件。
//! - 进度取自 `Player::get_pos()`，即**输出时钟**，满足 15.3 的防漂移要求。
//!
//! 设备异常处理（15.1「设备异常时的恢复或清晰提示」）：
//! 打开失败会返回 [`PlayerError::AudioDeviceUnavailable`]，调用方可降级到
//! 无声模式继续使用媒体加载/状态管理能力，而不是整个进程崩溃。
//!
//! 频谱采样（[`TapSource`]）：解码器外面套一层**透传** `Source`，在把样本
//! 原样交给 rodio 的同时，把混成单声道的样本批量推进
//! [`SpectrumShared`]。对播放链路零侵入——不改样本、不改时长、不改 seek。
//!
//! 流式播放（方案 2A）：[`AudioEngine::load_stream`] 收的是「还在长的字节缓冲」，
//! 解码由 [`crate::audio::stream_pump`] 的独立线程完成，本模块只把它的
//! [`PcmSource`] 当成又一条普通音源塞进队列。于是 15.2 的约束第一次有了
//! **可验证的**形态：输出线程取样本时既不碰网络也不碰磁盘，
//! 拿不到就发静音，「缓冲中」由环的空/满直接定义（[`AudioEngine::is_buffering`]）。

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rodio::source::SeekError;
use rodio::{
    ChannelCount, Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, SampleRate, Source,
};

use super::engine::{AudioEngine, EngineMedia};
use super::spectrum::{Bands, SpectrumShared, TAP_BATCH};
use super::stream_pump::StreamPump;
use crate::error::{PlayerError, Result};
use crate::music::stream_buffer::StreamBuffer;

/// 透传采样器：原样转发样本，顺带把单声道混音推进频谱缓冲。
///
/// 两条铁律（`DESIGN_SPEC.md` 15.2）：
/// 1. **不改样本**——`next()` 返回的永远是 `inner` 的原值。
/// 2. **不阻塞**——攒够 [`TAP_BATCH`] 个才 `try_lock` 一次，抢不到就丢弃这一批。
///    丢一批只影响 ~11ms 的频谱分辨率，绝不能让音频回调等锁。
struct TapSource<S> {
    inner: S,
    shared: Arc<Mutex<SpectrumShared>>,
    channels: u16,
    /// 当前帧已累加的样本和（多声道混单声道）。
    frame_sum: f32,
    /// 当前帧已累加的声道数。
    frame_seen: u16,
    batch: [f32; TAP_BATCH],
    batch_len: usize,
}

impl<S: Source> TapSource<S> {
    fn new(inner: S, shared: Arc<Mutex<SpectrumShared>>) -> Self {
        Self {
            channels: inner.channels().get(),
            inner,
            shared,
            frame_sum: 0.0,
            frame_seen: 0,
            batch: [0.0; TAP_BATCH],
            batch_len: 0,
        }
    }

    /// 把攒下的一批单声道样本交给共享缓冲；抢不到锁就整批丢弃。
    fn flush(&mut self) {
        if self.batch_len == 0 {
            return;
        }
        if let Ok(mut shared) = self.shared.try_lock() {
            for v in &self.batch[..self.batch_len] {
                shared.push_mono(*v);
            }
        }
        self.batch_len = 0;
    }
}

impl<S: Source> Iterator for TapSource<S> {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        let Some(sample) = self.inner.next() else {
            // 源结束：把尾巴交出去，避免最后 ~11ms 白丢
            self.flush();
            return None;
        };

        self.frame_sum += sample;
        self.frame_seen += 1;
        if self.frame_seen >= self.channels {
            let mono = self.frame_sum / self.channels as f32;
            self.frame_sum = 0.0;
            self.frame_seen = 0;
            self.batch[self.batch_len] = mono;
            self.batch_len += 1;
            if self.batch_len == TAP_BATCH {
                self.flush();
            }
        }

        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<S: Source> Source for TapSource<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    fn channels(&self) -> ChannelCount {
        self.inner.channels()
    }

    fn sample_rate(&self) -> SampleRate {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }

    /// **必须**转发：`Source::try_seek` 默认实现返回 `NotSupported`，
    /// 不转发的话套上这层之后整条链路的 seek 会静默失效。
    fn try_seek(&mut self, pos: Duration) -> std::result::Result<(), SeekError> {
        let result = self.inner.try_seek(pos);
        if result.is_ok() {
            // 跳转后旧样本不属于新位置，丢弃未提交的一批
            self.batch_len = 0;
            self.frame_sum = 0.0;
            self.frame_seen = 0;
        }
        result
    }
}

/// rodio 音频引擎。
pub struct RodioEngine {
    /// 设备句柄必须存活，否则输出流会被关闭。
    _device: MixerDeviceSink,
    player: Player,
    /// 当前媒体路径；保留以便 stop/播放结束后重新装载。
    source: Option<PathBuf>,
    duration: Option<Duration>,
    /// 是否由调用方主动 stop（用于区分「自然播完」与「主动停止」）。
    user_stopped: bool,
    volume: f32,
    /// 频谱共享缓冲：音频线程写，UI 线程读。
    spectrum: Arc<Mutex<SpectrumShared>>,
    /// 流式播放的解码泵；`None` = 当前内容是整首装载的文件。
    stream: Option<StreamPump>,
}

impl RodioEngine {
    /// 打开默认输出设备。
    ///
    /// 无声卡或设备被占用时返回 [`PlayerError::AudioDeviceUnavailable`]。
    pub fn open() -> Result<Self> {
        tracing::info!("正在打开默认音频输出设备…");
        let device = DeviceSinkBuilder::open_default_sink().map_err(|e| {
            tracing::warn!("音频输出设备打开失败: {}", e);
            PlayerError::AudioDeviceUnavailable(e.to_string())
        })?;
        let player = Player::connect_new(device.mixer());

        tracing::info!(
            "音频输出设备已就绪: 采样率={}Hz 声道={}",
            device.config().sample_rate(),
            device.config().channel_count()
        );

        Ok(Self {
            _device: device,
            player,
            source: None,
            duration: None,
            user_stopped: false,
            volume: 1.0,
            spectrum: Arc::new(Mutex::new(SpectrumShared::new())),
            stream: None,
        })
    }

    /// 打开文件并解码为可播放源，同时读取解码器给出的总时长。
    fn open_decoder(path: &Path) -> Result<(Decoder<std::io::BufReader<File>>, Option<Duration>)> {
        let file = File::open(path).map_err(|source| PlayerError::MediaRead {
            path: path.to_path_buf(),
            source,
        })?;

        let decoder = Decoder::try_from(file).map_err(|e| {
            let detail = e.to_string();
            match e {
                rodio::decoder::DecoderError::UnrecognizedFormat => {
                    PlayerError::UnsupportedFormat(detail)
                }
                _ => PlayerError::DecodeFailed {
                    path: path.to_path_buf(),
                    detail,
                },
            }
        })?;

        let total = decoder.total_duration();
        Ok((decoder, total))
    }

    /// 把当前媒体重新装载到播放队列（从头开始）。
    fn reload(&mut self) -> Result<()> {
        let Some(path) = self.source.clone() else {
            return Err(PlayerError::InvalidState {
                state: "Idle".into(),
                action: "reload".into(),
            });
        };
        let (decoder, total) = Self::open_decoder(&path)?;
        if total.is_some() {
            self.duration = total;
        }
        self.append_synced(decoder);
        Ok(())
    }

    /// 队列已空之后重新起播。
    ///
    /// 流式内容到这里**一律退回整首装载**：泵只因终局（播完 / 失败 / 被取消）才退出，
    /// 那时写入线程早就不在了，临时文件里有什么就是什么——再开一台泵去读一个
    /// 不会再长的文件，除了多一条线程没有换来任何东西。非流式内容与
    /// [`reload`](Self::reload) 同义。
    fn restart(&mut self) -> Result<()> {
        self.stream = None;
        self.reload()
    }

    /// 把音源装载进播放队列，并把位置跟踪**同步**归零。
    ///
    /// rodio 的播放位置由音频线程经 `periodic_access` 每 ~5ms 异步刷新，
    /// `Player::append` 本身不会重置它——不校正的话，重播/换曲后的极短
    /// 窗口内 `get_pos()` 会返回上一条的旧位置（例如已播完的 00:03），
    /// 直到第一次刷新到来。`try_seek(ZERO)` 会同步写回归零值：
    /// 即使解码器不支持 seek，位置同样被校正（只是返回 `SeekError`），
    /// 而全新解码器本来就在起点，故此处忽略该错误。
    fn append_synced<S: Source + Send + 'static>(&mut self, source: S) {
        // 换曲先把频谱缓冲清干净：旧曲的尾巴会被误当成新曲的开头
        if let Ok(mut shared) = self.spectrum.lock() {
            shared.set_rate(source.sample_rate().get());
            shared.reset();
        }
        self.player
            .append(TapSource::new(source, Arc::clone(&self.spectrum)));
        if let Err(e) = self.player.try_seek(Duration::ZERO) {
            tracing::debug!("装载后位置归零未被解码器执行（位置已在起点，无碍）: {}", e);
        }
    }
}

impl AudioEngine for RodioEngine {
    fn load(&mut self, path: &Path) -> Result<EngineMedia> {
        let (decoder, total) = Self::open_decoder(path)?;

        // 换台：先收掉可能还在跑的泵（它服务的曲子没了，下载也该停），
        // 再清空队列——源被停后立刻返回 `None`，`clear()` 里的等待才有界。
        self.stream = None;
        self.player.clear();
        self.append_synced(decoder);

        self.source = Some(path.to_path_buf());
        self.duration = total;
        self.user_stopped = false;

        tracing::info!(
            "媒体已加载: {} | 时长={}",
            path.display(),
            total
                .map(crate::music::models::format_duration)
                .unwrap_or_else(|| "未知".into())
        );

        Ok(EngineMedia { duration: total })
    }

    fn load_stream(&mut self, buffer: Arc<StreamBuffer>) -> Result<EngineMedia> {
        // 顺序同上：先停旧泵，队列里的旧源才不会再「没声音但没结束」。
        self.stream = None;
        self.player.clear();

        let path = buffer.path().to_path_buf();
        // 构造解码器要在**装载线程**上读容器头：越过水位时会等字节回来。
        // 这一步绝不能挪到输出线程——那正是本模块要避开的形状。
        let (pump, source) = StreamPump::start(buffer)?;
        let total = source.total_duration();
        self.stream = Some(pump);
        self.source = Some(path.clone());
        self.duration = total;
        self.user_stopped = false;
        self.append_synced(source);

        tracing::info!(
            "流式媒体已装载: {} | 时长={}",
            path.display(),
            total
                .map(crate::music::models::format_duration)
                .unwrap_or_else(|| "未知".into())
        );

        Ok(EngineMedia { duration: total })
    }

    fn is_buffering(&self) -> bool {
        self.stream.as_ref().is_some_and(|pump| pump.is_buffering())
    }

    fn play(&mut self) -> Result<()> {
        if self.source.is_none() {
            return Err(PlayerError::InvalidState {
                state: "Idle".into(),
                action: "play".into(),
            });
        }
        // 队列已空说明「播完」或「已 stop」，需要重新装载才能再次播放。
        if self.player.empty() {
            self.restart()?;
        }
        self.player.play();
        self.user_stopped = false;
        tracing::debug!("开始播放");
        Ok(())
    }

    fn pause(&mut self) -> Result<()> {
        if self.source.is_none() || self.player.empty() {
            return Err(PlayerError::InvalidState {
                state: if self.source.is_some() {
                    "Ended"
                } else {
                    "Idle"
                }
                .into(),
                action: "pause".into(),
            });
        }
        self.player.pause();
        tracing::debug!("暂停");
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        // 先收泵：它一停，队列里那条源就结束了，`clear()` 不必等一条还在缓冲的曲子
        self.stream = None;
        self.player.clear();
        self.user_stopped = true;
        if let Ok(mut shared) = self.spectrum.lock() {
            shared.reset();
        }
        tracing::debug!("停止");
        Ok(())
    }

    fn seek(&mut self, position: Duration) -> Result<()> {
        if self.source.is_none() {
            return Err(PlayerError::InvalidState {
                state: "Idle".into(),
                action: "seek".into(),
            });
        }
        if self.player.empty() {
            self.restart()?;
        }
        self.player.try_seek(position).map_err(|e| {
            PlayerError::InvalidPosition(format!(
                "{}（{}）",
                crate::music::models::format_duration(position),
                e
            ))
        })?;
        tracing::debug!("跳转到 {}", crate::music::models::format_duration(position));
        Ok(())
    }

    fn set_volume(&mut self, volume: f32) -> Result<()> {
        if !(0.0..=1.0).contains(&volume) {
            return Err(PlayerError::InvalidVolume(volume));
        }
        self.player.set_volume(volume);
        self.volume = volume;
        tracing::debug!("音量 -> {:.0}%", volume * 100.0);
        Ok(())
    }

    fn volume(&self) -> f32 {
        self.volume
    }

    fn position(&self) -> Duration {
        if let Some(pump) = &self.stream {
            // 流式：位置直接取泵的消费时钟。缓冲期间它自己停住，
            // 不需要这里再猜，也不该被 `player.get_pos()` 的定时器带着空转。
            return pump.position();
        }
        if self.player.empty() {
            // 队列为空：要么从未加载，要么已主动停止，要么已自然播完。
            if self.user_stopped {
                Duration::ZERO
            } else {
                self.duration.unwrap_or(Duration::ZERO)
            }
        } else {
            self.player.get_pos()
        }
    }

    fn duration(&self) -> Option<Duration> {
        self.duration
    }

    fn is_finished(&self) -> bool {
        if self.source.is_none() || self.user_stopped || !self.player.empty() {
            return false;
        }
        // 队列空 = 源返回过 `None`。流式内容还要泵确实走到终局，
        // 否则「环暂时空了」这种中间态会被读成播完而自动跳下一首。
        self.stream
            .as_ref()
            .is_none_or(|pump| pump.is_thread_done())
    }

    fn has_media(&self) -> bool {
        self.source.is_some()
    }

    fn spectrum(&self) -> Option<Bands> {
        // 缓冲期间灌进环的是补出来的静音，柱子会塌成一条直线；
        // 报「没有频谱数据」让 UI 走衰减，比画一段死掉的假波形诚实。
        if self.is_buffering() {
            return None;
        }
        // 暂停时音频线程不再拉样本，缓冲会定格在最后一帧——
        // 直接返回 None 让 UI 走衰减，避免暂停后柱子僵在某个形状。
        if self.player.empty() || self.player.is_paused() {
            return None;
        }
        self.spectrum.lock().ok()?.bands()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::buffer::SamplesBuffer;
    use std::num::NonZero;

    fn stereo(data: Vec<f32>) -> SamplesBuffer {
        SamplesBuffer::new(
            NonZero::new(2u16).unwrap(),
            NonZero::new(44_100u32).unwrap(),
            data,
        )
    }

    #[test]
    fn tap_source_passes_samples_through_unchanged() {
        let input: Vec<f32> = (0..64).map(|i| (i as f32) / 64.0).collect();
        let shared = Arc::new(Mutex::new(SpectrumShared::new()));
        let out: Vec<f32> = TapSource::new(stereo(input.clone()), Arc::clone(&shared)).collect();
        assert_eq!(out, input, "采样器必须原样透传，一个样本都不能改");
    }

    #[test]
    fn tap_source_preserves_stream_metadata() {
        let shared = Arc::new(Mutex::new(SpectrumShared::new()));
        let tap = TapSource::new(stereo(vec![0.0; 32]), shared);
        assert_eq!(tap.channels().get(), 2);
        assert_eq!(tap.sample_rate().get(), 44_100);
        assert!(
            tap.total_duration().is_some(),
            "总时长必须透传，否则进度条失效"
        );
    }

    #[test]
    fn tap_source_feeds_mono_mix_into_shared() {
        let shared = Arc::new(Mutex::new(SpectrumShared::new()));
        // 左右声道各 1.0 / -1.0 → 单声道混音 0.0
        let data: Vec<f32> = std::iter::repeat_n([1.0f32, -1.0], TAP_BATCH)
            .flatten()
            .collect();
        let mut tap = TapSource::new(stereo(data), Arc::clone(&shared));
        while tap.next().is_some() {}

        let mut g = shared.lock().unwrap();
        assert_eq!(g.filled(), TAP_BATCH, "一批样本应在源结束时全部提交");
        // 混音结果接近 0，频谱不应被点亮
        g.set_rate(44_100);
        let bands = g.bands();
        assert!(bands.is_none(), "样本不足半窗时不出频谱");
    }

    #[test]
    fn tap_source_keeps_multichannel_frame_alignment() {
        let shared = Arc::new(Mutex::new(SpectrumShared::new()));
        // 立体声：每帧两个样本，单声道混音应等于两声道均值
        let data: Vec<f32> = std::iter::repeat_n([0.5f32, 0.25], TAP_BATCH)
            .flatten()
            .collect();
        let mut tap = TapSource::new(stereo(data), Arc::clone(&shared));
        while tap.next().is_some() {}
        let g = shared.lock().unwrap();
        assert_eq!(g.filled(), TAP_BATCH, "帧对齐错位会多算/少算样本");
    }
}

#[cfg(test)]
mod live_probe {
    use super::*;
    use std::io::Write;

    /// 写一个 16bit 单声道 WAV（44.1kHz）。
    fn write_wav(path: &Path, tone_hz: f32, secs: f32) {
        let rate = 44_100u32;
        let n = (rate as f32 * secs) as usize;
        let mut pcm = Vec::with_capacity(n * 2);
        for i in 0..n {
            let v = (std::f32::consts::TAU * tone_hz * i as f32 / rate as f32).sin() * 0.8;
            pcm.extend_from_slice(&((v * i16::MAX as f32) as i16).to_le_bytes());
        }
        let mut f = std::fs::File::create(path).unwrap();
        let data_len = pcm.len() as u32;
        f.write_all(b"RIFF").unwrap();
        f.write_all(&(36 + data_len).to_le_bytes()).unwrap();
        f.write_all(b"WAVEfmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap(); // fmt chunk size
        f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        f.write_all(&1u16.to_le_bytes()).unwrap(); // mono
        f.write_all(&rate.to_le_bytes()).unwrap();
        f.write_all(&(rate * 2).to_le_bytes()).unwrap(); // byte rate
        f.write_all(&2u16.to_le_bytes()).unwrap(); // block align
        f.write_all(&16u16.to_le_bytes()).unwrap(); // bits
        f.write_all(b"data").unwrap();
        f.write_all(&data_len.to_le_bytes()).unwrap();
        f.write_all(&pcm).unwrap();
    }

    /// 手动探针：真实设备上验证「解码 → 采样 → FFT → 频带」全链路。
    ///
    /// `cargo test -p reimuplayer --lib spectrum_live_probe -- --ignored --nocapture`
    #[test]
    #[ignore = "需要音频输出设备；手动执行"]
    fn spectrum_live_probe() {
        let dir = std::env::temp_dir();
        let path = dir.join("reimu-player_spectrum_probe.wav");
        write_wav(&path, 1000.0, 1.0);

        let mut engine = RodioEngine::open().expect("需要可用的音频输出设备");
        engine.load(&path).expect("装载 WAV");
        engine.play().expect("开始播放");

        // 攒够一个分析窗（2048 样本 ≈ 46ms）+ 余量
        std::thread::sleep(Duration::from_millis(300));

        let bands = engine.spectrum().expect("播放中应有频谱数据");
        let peak = (0..super::super::spectrum::BANDS)
            .max_by(|a, b| bands[*a].partial_cmp(&bands[*b]).unwrap())
            .unwrap();
        println!("1kHz 正弦的频带分布: {:?}", bands);
        println!(
            "峰值频带 = {} / {}，电平 = {:.3}",
            peak,
            super::super::spectrum::BANDS,
            bands[peak]
        );

        let _ = engine.stop();
        let _ = std::fs::remove_file(&path);

        assert!(
            bands[peak] > 0.3,
            "1kHz 应显著点亮某一段，实际峰值 {:.3}",
            bands[peak]
        );
        // 1kHz 在对数轴上约在 26 段中的第 13 段附近
        assert!(
            (8..20).contains(&peak),
            "1kHz 的峰值应落在中频段，实际第 {} 段",
            peak
        );
    }
}
