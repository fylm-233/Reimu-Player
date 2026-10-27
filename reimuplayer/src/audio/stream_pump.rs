//! 流式解码泵：边下边播链路的最后一棒。
//!
//! 一句话：**解码绝不站在输出线程上**。
//!
//! 把 [`StreamReader`] 直接交给 rodio 的 `Decoder` 也能出声，但那个解码器是
//! **在输出线程上被拉动的**（rodio 0.23 没有内部解码线程），而它的每一次 `read`
//! 都可能越过水位——越线就等，一等最长按
//! [`READ_PATIENCE`](crate::music::stream_buffer::READ_PATIENCE) 那么久
//! （断流续传时这是真的会发生）。叠上 [`rodio::Player`] 的两条同步接口：
//!
//! - `clear()` 内部 `sleep_until_end()`，要等输出线程把队列排空；
//! - `try_seek()` 内部等一次反馈，要等输出线程的下一次 `periodic_access`；
//!
//! 三者凑在一起就是：**网络一抖，换台、停止、拖进度条全卡在 UI 线程上**。
//!
//! 所以这里把链路劈成两截：
//!
//! ```text
//!   写入线程 ─commit─▶ StreamBuffer ──read(可等)──▶ 泵线程 ──push──▶ PCM 环 ──take──▶ 输出线程
//!                                                        ▲                            │
//!                                                        └────────── 跳转请求 ────────┘
//! ```
//!
//! 输出线程那一侧的 [`PcmSource`] **从不阻塞、从不等待**：环里有样本就取，
//! 取不到就发静音，源头收工才返回 `None`。于是「缓冲中」有了明确的物理含义
//! ——环空了但还没收工，而不是某个要靠猜的状态。
//!
//! 位置取**消费时钟**（输出侧真取走了多少样本），不是调用侧累加的定时器
//! （`DESIGN_SPEC.md` 15.3）：缓冲期间进度条停住，字节一到就继续走。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rodio::source::SeekError;
use rodio::{ChannelCount, Decoder, SampleRate, Source};

use crate::error::{PlayerError, Result};
use crate::music::stream_buffer::{BufferPhase, StreamBuffer, StreamReader};

/// 泵线程名。
const PUMP_THREAD: &str = "reimu-decode";

/// 环里最多攒多少秒的 PCM。
///
/// 与起播闸门同一套理由：**按样本数定等于按采样率定**。2 秒的覆盖量对
/// 192 kHz 立体声是 3 MB 内存，对 44.1 kHz 是 700 KB，两个都不构成负担；
/// 真正要买的是「泵线程被水位卡住时，输出侧还能响多久」。
const RING_LEAD_SECS: u32 = 2;

/// 泵线程一次提交给环的样本数。
///
/// 这个数同时是「生产者要求腾出的最小连续空间」：环剩余不足一批时生产者会等，
/// 所以它必须远小于环容量，否则第一批就把自己锁死。44.1 kHz 立体声下约 12 ms。
const BATCH: usize = 1_024;

/// 生产者在环满时单次让渡的最长时间。
///
/// 只是兜底：真正的叫醒来自消费侧的 `notify`。取 50 ms 是因为停止标志也靠这次
/// 醒来复查，而「泵多久退掉」决定了换台时旧线程还占着文件句柄的时长上限。
const ROOM_WAIT: Duration = Duration::from_millis(50);

/// 解码器收到 EOF、缓冲却还挂在 `Growing` 时最多等写入者收口的时间。
///
/// 写入者从「最后一批字节落盘」到「写下终局」只隔一次锁，200 ms 是宽裕的上限。
/// 只在这窗口里轮询而不 `join`：泵线程再晚也要把「播完」告诉输出侧，
/// 而一个已经不会收工的写入者不该让解码器永远吊着。
const TERMINAL_GRACE: Duration = Duration::from_millis(200);

/// 环的容量（样本数），由解码器报出的采样率与声道数推出来。
fn ring_capacity(rate: SampleRate, channels: ChannelCount) -> usize {
    let frames = rate.get() as usize * RING_LEAD_SECS as usize;
    frames.saturating_mul(channels.get() as usize)
}

/// 环走到哪一步了。
#[derive(Debug, Clone, PartialEq)]
enum Phase {
    /// 泵线程还在干活（或还没干完）。
    Running,
    /// 字节见底且写入者收工：消费侧取空后即真结束。
    Ended,
    /// 解码或传输失败，带人话原因。已攒下的样本照放，放完再收。
    Failed(String),
}

impl Phase {
    fn is_terminal(&self) -> bool {
        !matches!(self, Phase::Running)
    }
}

/// 共享环本体。
struct Ring {
    samples: VecDeque<f32>,
    /// 容量上限（样本数）。
    cap: usize,
    /// 输出侧已取走**代表音频**的样本数（静音不计），即消费时钟的分子。
    taken: u64,
    /// 待处理的跳转请求；`Some` 时环已清空，泵线程应先作废手头的一批。
    seek: Option<Duration>,
    phase: Phase,
}

/// 解码线程与输出线程之间的 PCM 管道。
///
/// **两侧职责不同**：[`PcmPipe::push`] 可以等（它在自己的线程上），
/// [`PcmPipe::take`] 绝不等待（它在输出线程上）。
pub struct PcmPipe {
    ring: Mutex<Ring>,
    /// 环腾出空间时叫醒生产者。
    room: Condvar,
    /// 上层要求收工（换台/停止/析构）。独立于锁，停手不用抢锁。
    stopped: AtomicBool,
    channels: ChannelCount,
    sample_rate: SampleRate,
    duration: Option<Duration>,
}

/// [`PcmPipe::push`] 的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Push {
    /// 整批已进环。
    Accepted,
    /// 有跳转在等处理：这批属于旧位置，丢弃。
    SeekPending,
    /// 上层要求收工。
    Stopped,
}

/// [`PcmPipe::take`] 的结论。
#[derive(Debug, Clone, Copy, PartialEq)]
enum Frame {
    /// 真实音频样本。
    Sample(f32),
    /// 环暂时空了：发静音，位置不动，等字节回来。
    Silence,
    /// 源头收工：输出侧据此结束本曲。
    End,
}

impl PcmPipe {
    fn new(
        channels: ChannelCount,
        sample_rate: SampleRate,
        duration: Option<Duration>,
    ) -> Arc<Self> {
        let cap = ring_capacity(sample_rate, channels);
        Arc::new(Self {
            ring: Mutex::new(Ring {
                samples: VecDeque::with_capacity(BATCH * 4),
                cap,
                taken: 0,
                seek: None,
                phase: Phase::Running,
            }),
            room: Condvar::new(),
            stopped: AtomicBool::new(false),
            channels,
            sample_rate,
            duration,
        })
    }

    fn lock(&self) -> MutexGuard<'_, Ring> {
        // 锁中毒只在临界区 panic 时出现，而这里的临界区只有 VecDeque 读写与计数；
        // 与其让一次瞬时 panic 把播放链路钉死，不如取回内部值继续跑。
        self.ring.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 泵线程入口：把一批样本送进环，环满就等消费侧腾地方。
    fn push(&self, batch: &[f32]) -> Push {
        if batch.is_empty() {
            return Push::Accepted;
        }
        let mut g = self.lock();
        loop {
            if self.stopped.load(Ordering::SeqCst) {
                return Push::Stopped;
            }
            if g.seek.is_some() {
                return Push::SeekPending;
            }
            if g.samples.len() + batch.len() <= g.cap {
                g.samples.extend(batch.iter().copied());
                return Push::Accepted;
            }
            // 等消费侧腾空间。超时只是兜底：停止标志同样靠这次醒来复查。
            let (next, _waited) = match self.room.wait_timeout(g, ROOM_WAIT) {
                Ok(pair) => pair,
                Err(e) => e.into_inner(),
            };
            g = next;
        }
    }

    /// 输出线程入口：取一个样本。
    ///
    /// **不得等待**：临界区只有出队与一次计数（生产者持锁期间从不碰网络与磁盘，
    /// 且等待时会释放锁）；取不到样本时按状态发静音或收工。
    fn take(&self) -> Frame {
        let mut g = self.lock();
        if let Some(sample) = g.samples.pop_front() {
            g.taken += 1;
            // 只在「刚才满着」时叫醒生产者，避免每样本一次 notify。
            if g.samples.len() + 1 >= g.cap {
                drop(g);
                self.room.notify_one();
            }
            return Frame::Sample(sample);
        }
        match g.phase {
            Phase::Running => Frame::Silence,
            Phase::Ended | Phase::Failed(_) => Frame::End,
        }
    }

    /// 输出侧请求跳转：立刻清空环并把消费时钟摆到新位置，
    /// 真正的解码器跳转由泵线程去做。
    fn request_seek(&self, pos: Duration) {
        let mut g = self.lock();
        g.samples.clear();
        g.seek = Some(pos);
        // 时钟先就位：进度条要立刻响应拖动，不等解码器追上来
        g.taken = self.frames_to_samples(duration_clamped(self.duration, pos));
        drop(g);
        self.room.notify_all();
    }

    /// 泵线程取走待处理的跳转。
    fn take_seek(&self) -> Option<Duration> {
        self.lock().seek.take()
    }

    /// 泵线程收工：字节见底且写入者已完成。
    fn mark_ended(&self) {
        let mut g = self.lock();
        if !g.phase.is_terminal() {
            g.phase = Phase::Ended;
        }
    }

    /// 泵线程失败：已攒下的样本照放，放完再收。
    fn fail(&self, reason: impl Into<String>) {
        let mut g = self.lock();
        if !g.phase.is_terminal() {
            g.phase = Phase::Failed(reason.into());
        }
    }

    /// 要求两侧收工。
    ///
    /// 顺手把环清空并置终局：这首曲子已经不要了，环里攒的样本属于旧位置，
    /// 一发都不该再响；而输出侧立刻拿到 `End` 才能让 `Player::clear()`
    /// 那种「等队列真的空掉」的调用有界返回。失败原因已经说出口的，不改写。
    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        {
            let mut g = self.lock();
            g.samples.clear();
            if !matches!(g.phase, Phase::Failed(_)) {
                g.phase = Phase::Ended;
            }
        }
        self.room.notify_all();
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// 消费时钟：输出侧真取走了多少音频。
    ///
    /// 缓冲期间取到的是静音，不计入——进度条因此**停住**而不是继续空转，
    /// 这正是 15.3 要的「位置来自输出时钟」。
    fn position(&self) -> Duration {
        let taken = self.lock().taken;
        let frames = taken / self.channels.get() as u64;
        Duration::from_secs_f64(frames as f64 / self.sample_rate.get() as f64)
    }

    /// 环空了但还没收工 = 缓冲中。
    fn is_buffering(&self) -> bool {
        let g = self.lock();
        matches!(g.phase, Phase::Running) && g.samples.is_empty()
    }

    /// 终局失败原因（`None` = 正常播放或正常播完）。
    fn error(&self) -> Option<String> {
        match &self.lock().phase {
            Phase::Failed(reason) => Some(reason.clone()),
            _ => None,
        }
    }

    fn frames_to_samples(&self, pos: Duration) -> u64 {
        let frames = (pos.as_secs_f64().max(0.0) * self.sample_rate.get() as f64) as u64;
        frames.saturating_mul(self.channels.get() as u64)
    }
}

impl std::fmt::Debug for PcmPipe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let g = self.lock();
        f.debug_struct("PcmPipe")
            .field("queued", &g.samples.len())
            .field("cap", &g.cap)
            .field("taken", &g.taken)
            .field("seek", &g.seek)
            .field("phase", &g.phase)
            .field("stopped", &self.is_stopped())
            .finish()
    }
}

/// 跳转位置夹在总时长以内：越过末尾的请求不该把消费时钟推到曲目之外。
fn duration_clamped(total: Option<Duration>, pos: Duration) -> Duration {
    match total {
        Some(end) if pos > end => end,
        _ => pos,
    }
}

/// 交给 rodio 的输出侧音源：只碰环，不碰网络、不碰磁盘。
///
/// 生命周期：从装载起一直存在，直到字节真的放完（[`Frame::End`]）才返回
/// `None`。缓冲期间返回静音而**不是**结束——把「没声音」和「播完了」混成
/// 一件事，就会自动跳下一首，这是本方案最早要避开的那个坑。
#[derive(Debug)]
pub struct PcmSource {
    pipe: Arc<PcmPipe>,
}

impl Iterator for PcmSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        match self.pipe.take() {
            Frame::Sample(v) => Some(v),
            Frame::Silence => Some(0.0),
            Frame::End => None,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}

impl Source for PcmSource {
    /// 样本是随取随给的，没有「本段还剩多少」可预告——与解码器同义。
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        self.pipe.channels
    }

    fn sample_rate(&self) -> SampleRate {
        self.pipe.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        self.pipe.duration
    }

    /// 跳转必须**异步落地**：在输出线程上做解码器跳转等于把网络等待搬到
    /// UI 的调用栈上，所以这里只登记请求，由泵线程去执行。
    fn try_seek(&mut self, pos: Duration) -> std::result::Result<(), SeekError> {
        self.pipe.request_seek(pos);
        Ok(())
    }
}

/// 流式解码泵：一条 [`PcmSource`] 背后的解码线程。
///
/// [`StreamPump::shutdown`] 会让线程尽快退出（不等它收工——调用方正卡在
/// 换台/停止的按键路径上，一毫秒都不该为这个等待）。
pub struct StreamPump {
    pipe: Arc<PcmPipe>,
    buffer: Arc<StreamBuffer>,
    joiner: Option<std::thread::JoinHandle<()>>,
}

/// 手写而非 derive：`StreamBuffer` 里有 `Condvar`（它没有 `Debug`），
/// 而这里要的只是「哪首曲子的泵、线程还在不在、环里剩多少货」。
impl std::fmt::Debug for StreamPump {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamPump")
            .field("buffer", &self.buffer.path())
            .field("thread_done", &self.is_thread_done())
            .field("pipe", &self.pipe)
            .finish_non_exhaustive()
    }
}

impl StreamPump {
    /// 在字节缓冲上起一台泵。
    ///
    /// **前置条件**：总长已知且水位已过
    /// [`start_gate`](crate::music::stream_buffer::start_gate)，由装载侧保证。
    /// 构造时的第一口读若越过水位会等字节回来——那发生在调用线程上，
    /// 所以它只能出现在装载路径，绝不能出现在输出线程。
    pub fn start(buffer: Arc<StreamBuffer>) -> Result<(Self, PcmSource)> {
        let path = buffer.path().to_path_buf();
        let total = buffer.declared().ok_or_else(|| PlayerError::InvalidState {
            state: "LengthUnknown".into(),
            action: "load_stream".into(),
        })?;

        let reader = StreamReader::open(Arc::clone(&buffer))
            .map_err(|source| PlayerError::MediaRead { path, source })?;
        let decoder = Decoder::builder()
            .with_data(reader)
            .with_byte_len(total)
            .build()
            .map_err(|e| PlayerError::DecodeFailed {
                path: buffer.path().to_path_buf(),
                detail: e.to_string(),
            })?;

        let channels = decoder.channels();
        let sample_rate = decoder.sample_rate();
        let duration = decoder.total_duration();
        let pipe = PcmPipe::new(channels, sample_rate, duration);

        let thread_pipe = Arc::clone(&pipe);
        let thread_buffer = Arc::clone(&buffer);
        let joiner = std::thread::Builder::new()
            .name(PUMP_THREAD.into())
            .spawn(move || pump_loop(decoder, thread_pipe, thread_buffer))
            .map_err(|e| PlayerError::OutputError(format!("解码线程启动失败: {e}")))?;

        Ok((
            Self {
                pipe: Arc::clone(&pipe),
                buffer,
                joiner: Some(joiner),
            },
            PcmSource { pipe },
        ))
    }

    /// 当前播放位置（消费时钟）。
    pub fn position(&self) -> Duration {
        self.pipe.position()
    }

    /// 是否正在缓冲（环空了但还没收工）。
    pub fn is_buffering(&self) -> bool {
        self.pipe.is_buffering()
    }

    /// 终局失败原因。
    pub fn error(&self) -> Option<String> {
        self.pipe.error()
    }

    /// 泵线程是否已经退出。
    pub fn is_thread_done(&self) -> bool {
        self.joiner.as_ref().is_some_and(|j| j.is_finished())
    }

    /// 收工：请泵线程停手，并让写入者别再为一首已经不播的曲子下载字节。
    ///
    /// 不 `join`——线程靠 [`PcmPipe::stop`] + [`StreamBuffer::abort`] 自己醒，
    /// 最长 `ROOM_WAIT`（50 ms）后退出。
    pub fn shutdown(mut self) {
        self.teardown();
    }

    /// 停线程 + 取消下载，但不消费 `self`（供 `Drop` 复用）。
    ///
    /// 线程句柄直接丢弃：它的出口由停止标志与缓冲取消保证，而 `join` 会把
    /// 「等多久」重新变成 UI 路径上的未知数；线程退出后自己收尾，句柄只是噪音。
    fn teardown(&mut self) {
        self.joiner = None;
        self.pipe.stop();
        self.buffer.abort();
    }
}

impl Drop for StreamPump {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// 泵线程本体：解码 → 成批入环，直到源收工、被停或失败。
fn pump_loop(mut decoder: Decoder<StreamReader>, pipe: Arc<PcmPipe>, buffer: Arc<StreamBuffer>) {
    let mut batch: Vec<f32> = Vec::with_capacity(BATCH);
    loop {
        if pipe.is_stopped() {
            return;
        }
        if let Some(pos) = pipe.take_seek() {
            if let Err(e) = decoder.try_seek(pos) {
                pipe.fail(format!("流式跳转失败: {e}"));
                return;
            }
        }

        batch.clear();
        let mut exhausted = false;
        while batch.len() < BATCH {
            match decoder.next() {
                Some(sample) => batch.push(sample),
                None => {
                    exhausted = true;
                    break;
                }
            }
        }

        match pipe.push(&batch) {
            Push::Accepted => {}
            // 手这批属于旧位置：丢掉，回头处理跳转（跳转后同一轮循环继续解码）
            Push::SeekPending => continue,
            Push::Stopped => return,
        }

        if exhausted {
            // 只有「字节真的放完」或「被取消」才算播完；写入者还活着而解码器
            // 断了 = 读侧出错（超时/中断/坏容器），绝不能说成「播完」。
            let mut phase = buffer.phase();
            if phase == BufferPhase::Growing && all_bytes_present(&buffer) {
                // 最后一个字节已公布、终局却还没落进状态：解码器是**按总长**收到
                // EOF 的，这一刻它看到的和 `Done` 是同一件事。直接判超时，就会给
                // 一首正常播完的曲子安上假错误（写入者只在两次调用之间慢了几微秒）。
                // 等一个极短的收口窗口，让真正的 `Failed` 也有机会说话。
                phase = wait_for_terminal(&buffer);
            }
            match phase {
                BufferPhase::Done | BufferPhase::Aborted => pipe.mark_ended(),
                BufferPhase::Failed(detail) => pipe.fail(detail),
                BufferPhase::Growing => {
                    if all_bytes_present(&buffer) {
                        // 整首都在盘上、只是没人写终局（写手被杀 / 忘了收工）：
                        // 音频已经放完，按播完收场比对用户喊「缓冲超时」诚实。
                        pipe.mark_ended()
                    } else {
                        pipe.fail("音频字节停止到达（缓冲超时）")
                    }
                }
                other => pipe.fail(format!("流式解码提前结束（缓冲状态 {other:?}）")),
            }
            return;
        }
    }
}

/// 整首字节是否都已落盘。
///
/// 总长未知时一律算「没齐」：那条路径由 [`BufferPhase::LengthUnknown`] 交装载器
/// 回退整首，不该在这里被误判成播完。
fn all_bytes_present(buffer: &StreamBuffer) -> bool {
    buffer
        .declared()
        .is_some_and(|total| buffer.written() >= total)
}

/// 等缓冲离开 `Growing`；等不到就原样返回当前阶段。
fn wait_for_terminal(buffer: &StreamBuffer) -> BufferPhase {
    let until = Instant::now() + TERMINAL_GRACE;
    while buffer.phase() == BufferPhase::Growing && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(1));
    }
    buffer.phase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 造一个「整首 WAV 已写好、但水位只推进到前 `committed` 帧」的缓冲。
    ///
    /// WAV 本体是**合法完整**的（头部声明的数据长度与实际字节一致），模拟「没下完」
    /// 靠的是水位——解码器越过水位就会被 [`StreamReader`] 拦住，与真实断流同一条路径。
    /// 不写「声明比实下大」的文件：那样测的是尾部截断，不是边下边播。
    fn staged(name: &str, total_frames: usize, committed: usize) -> (Arc<StreamBuffer>, PathBuf) {
        let pcm = ramp_bytes(total_frames, 0, total_frames);
        let file = wav_bytes(&pcm);
        let path = scratch_path(name);
        std::fs::write(&path, &file).unwrap();
        let buf = StreamBuffer::new(path.clone());
        buf.declare_len(file.len() as u64);
        buf.commit((44 + committed * 4) as u64);
        (buf, path)
    }

    /// 斜坡：第 `i` 帧左声道 `0.05 + (start+i)/total × 0.4`、右声道取其相反数。
    ///
    /// 值随帧号**严格单调**，所以「取到的样本多大」直接说明「解码器读到第几帧」；
    /// 左右互为相反数则用来证明声道没接反。起点抬高到 0.05 是刻意的：
    /// 真实样本一个都不等于 0.0，测试里「非静音」判据 `!= 0.0` 才是干净的。
    fn ramp_bytes(total_frames: usize, start: usize, count: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(count * 4);
        for k in 0..count {
            let i = start + k;
            let left = 0.05 + (i as f32 / total_frames as f32) * 0.4;
            let right = -left;
            for v in [left, right] {
                out.extend_from_slice(&((v * i16::MAX as f32) as i16).to_le_bytes());
            }
        }
        out
    }

    /// 自己拼一个 RIFF/WAVE，避免依赖引擎侧的私有写盘函数。
    fn wav_header(pcm_len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(44);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36u32 + pcm_len as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&2u16.to_le_bytes()); // 立体声
        out.extend_from_slice(&44_100u32.to_le_bytes());
        out.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
        out.extend_from_slice(&4u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(pcm_len as u32).to_le_bytes());
        out
    }

    fn wav_bytes(pcm: &[u8]) -> Vec<u8> {
        let mut out = wav_header(pcm.len());
        out.extend_from_slice(pcm);
        out
    }

    fn scratch_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "reimuplayer_stream_pump_{}_{}_{}.wav",
            std::process::id(),
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ))
    }

    /// 取到 `want` 个**真实**样本（静音不计），源头收工则提前返回。
    ///
    /// 带死线：泵线程是异步的，万一它再也不会出货，测试该失败而不是挂住整个套件。
    fn drain(source: &mut PcmSource, want: usize) -> Vec<f32> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut out = Vec::with_capacity(want);
        while out.len() < want {
            match source.next() {
                Some(0.0) => assert!(Instant::now() < deadline, "取样超时：只拿到 {}", out.len()),
                Some(v) => out.push(v),
                None => break,
            }
        }
        out
    }

    /// 一直取到源头报「收工」；超时则返回 `false`，由调用方的断言报出具体环节。
    fn runs_to_end(source: &mut PcmSource) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while source.next().is_some() {
            if Instant::now() > deadline {
                return false;
            }
        }
        true
    }

    /// 轮询式等待：泵线程是异步的，测试靠条件本身收口，不靠固定 sleep。
    fn wait_until(label: &str, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cond() {
            assert!(Instant::now() < deadline, "等待超时：{label}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn stereo() -> ChannelCount {
        ChannelCount::new(2).unwrap()
    }

    fn rate(hz: u32) -> SampleRate {
        SampleRate::new(hz).unwrap()
    }

    fn pipe(ch: ChannelCount, hz: u32, total: Option<Duration>) -> Arc<PcmPipe> {
        PcmPipe::new(ch, rate(hz), total)
    }

    fn queued(pipe: &Arc<PcmPipe>) -> usize {
        pipe.lock().samples.len()
    }

    fn capacity(pipe: &Arc<PcmPipe>) -> usize {
        pipe.lock().cap
    }

    fn taken(pipe: &Arc<PcmPipe>) -> u64 {
        pipe.lock().taken
    }

    /// 大素材版写盘：头 + 逐块斜坡，全程只留一块的内存。
    ///
    /// `staged` 为了造水位必须先 `Vec` 出整份 PCM 再 `Vec` 出整份带头副本；
    /// 拿它做 50 MB 级探针，脚手架自己就先吃掉 100 MB，量到的就不是泵的占用了。
    fn write_wav_file(path: &std::path::Path, frames: usize) -> u64 {
        use std::io::Write as _;
        let mut out =
            std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path).unwrap());
        out.write_all(&wav_header(frames * 4)).unwrap();
        let block = 8_192;
        let mut start = 0usize;
        while start < frames {
            let n = (frames - start).min(block);
            out.write_all(&ramp_bytes(frames, start, n)).unwrap();
            start += n;
        }
        out.flush().unwrap();
        44 + (frames * 4) as u64
    }

    /// 限速写手：按 `kb_per_s` 把「已在盘上」的字节逐批公布成水位。
    ///
    /// 文件本体先写全，用**水位**演网速——解码器看到的正是水位，所以断供、
    /// 恢复、终局走的都是真实下载那一条代码路径，只是「字节何时可见」捏在测试手里。
    /// 被取消时不收工：`StreamBuffer::abort` 已经替它写了终局。
    fn trickle(buf: Arc<StreamBuffer>, kb_per_s: u32) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let total = buf.declared().expect("写手得知道整首多长");
            // 20 ms 一批：比解封装器一次读的块小，也把调度抖动摊平
            let step = (kb_per_s as u64 * 1024 / 50).max(1);
            let mut written = 0u64;
            while written < total && !buf.is_aborted() {
                written = (written + step).min(total);
                buf.commit(written);
                std::thread::sleep(Duration::from_millis(20));
            }
            if !buf.is_aborted() {
                buf.mark_done();
            }
        })
    }

    /// 一整遍取样的对账结果。
    struct Audit {
        /// 取到的真实样本数（静音不计）。
        samples: usize,
        /// 取到的静音样本数 = 输出侧为断供补了多少。
        silence: usize,
        /// 取到第一个真实样本时的水位与耗时。
        watermark_at_first: u64,
        ms_to_first: u128,
        /// 取样期间「缓冲中」命中次数。
        buffering: usize,
        /// 环内同时攒着的样本数峰值（泵的内存上限证据）。
        max_queued: usize,
    }

    /// 一路取到收工，逐样本核对数量、声道对齐与斜坡单调性。
    ///
    /// 计数而不是收集：母带档一趟就是两千六百万个样本，断言不该再吃一份内存。
    /// 斜坡左右声道互为相反数且都离开过 0，所以「非静音」判据 `!= 0.0` 是干净的。
    fn audit(pump: &StreamPump, source: &mut PcmSource, buf: &StreamBuffer) -> Audit {
        let started = Instant::now();
        let mut a = Audit {
            samples: 0,
            silence: 0,
            watermark_at_first: u64::MAX,
            ms_to_first: u128::MAX,
            buffering: 0,
            max_queued: 0,
        };
        let mut last_left = -1.0f32;
        loop {
            match source.next() {
                Some(0.0) => {
                    a.silence += 1;
                    if pump.is_buffering() {
                        a.buffering += 1;
                    }
                }
                Some(v) => {
                    if a.samples == 0 {
                        a.watermark_at_first = buf.written();
                        a.ms_to_first = started.elapsed().as_millis();
                    }
                    if a.samples.is_multiple_of(2) {
                        assert!(
                            v >= last_left,
                            "斜坡必须单调不减：第 {} 帧 {v} < 上一帧 {last_left}",
                            a.samples / 2
                        );
                        last_left = v;
                    } else {
                        assert!(
                            (v + last_left).abs() < 1e-6,
                            "右声道应是左声道的相反数，第 {} 帧得到 {v} / {last_left}（声道接反或帧错位）",
                            a.samples / 2
                        );
                    }
                    a.samples += 1;
                    a.max_queued = a.max_queued.max(queued(&pump.pipe));
                }
                None => break,
            }
            assert!(
                started.elapsed() < Duration::from_secs(120),
                "取样挂住：只拿到 {} 个真实样本 / {} 个静音",
                a.samples,
                a.silence
            );
        }
        a
    }

    // ---- 环容量：按秒，不按固定样本数 ----

    #[test]
    fn ring_capacity_is_measured_in_audio_seconds() {
        assert_eq!(
            ring_capacity(rate(44_100), stereo()),
            44_100 * 2 * RING_LEAD_SECS as usize
        );
        // 192 kHz 母带同样攒满 2 秒，而不是照抄 44.1 kHz 的样本数
        assert_eq!(
            ring_capacity(rate(192_000), stereo()),
            192_000 * 2 * RING_LEAD_SECS as usize
        );
        // 单声道容量相应减半
        assert_eq!(
            ring_capacity(rate(44_100), ChannelCount::new(1).unwrap()),
            44_100 * RING_LEAD_SECS as usize
        );
        // 极端组合不许溢出成 0：0 容量的环会让生产者永远等不到空间
        assert!(ring_capacity(rate(u32::MAX), ChannelCount::new(u16::MAX).unwrap()) > 0);
        // 一批必须远小于容量，否则第一批就把自己锁死
        assert!(capacity(&pipe(stereo(), 8_000, None)) > BATCH * 8);
    }

    // ---- 环的读写纪律 ----

    #[test]
    fn pushes_and_takes_in_order() {
        let p = pipe(stereo(), 44_100, None);
        assert_eq!(p.push(&[1.0, 2.0, 3.0, 4.0]), Push::Accepted);
        assert_eq!(queued(&p), 4);
        let got: Vec<f32> = (0..4)
            .map(|_| match p.take() {
                Frame::Sample(v) => v,
                other => panic!("应有真实样本，得到 {other:?}"),
            })
            .collect();
        assert_eq!(got, vec![1.0, 2.0, 3.0, 4.0], "环必须按写入顺序吐出");
        assert_eq!(taken(&p), 4, "取走 4 个样本");
    }

    #[test]
    fn consumer_gets_silence_not_end_while_running() {
        let p = pipe(stereo(), 44_100, None);
        assert!(matches!(p.take(), Frame::Silence), "环空且未收工应发静音");
        assert!(p.is_buffering());
        assert_eq!(p.position(), Duration::ZERO);
        assert_eq!(p.error(), None);
    }

    #[test]
    fn consumer_ends_only_after_the_buffered_audio_is_played_out() {
        let p = pipe(stereo(), 44_100, None);
        p.push(&[7.0, 8.0]);
        p.mark_ended();
        // 收工了但环里还有货：先放完，再结束
        assert!(matches!(p.take(), Frame::Sample(v) if v == 7.0));
        assert!(matches!(p.take(), Frame::Sample(v) if v == 8.0));
        assert!(matches!(p.take(), Frame::End));
        assert!(!p.is_buffering(), "已收工不该显示缓冲中");
    }

    #[test]
    fn failure_reports_a_reason_but_still_drains() {
        let p = pipe(stereo(), 44_100, None);
        p.push(&[1.0]);
        p.fail("缓冲超时");
        assert_eq!(p.error().as_deref(), Some("缓冲超时"));
        assert!(matches!(p.take(), Frame::Sample(v) if v == 1.0));
        assert!(matches!(p.take(), Frame::End));
        assert!(!p.is_buffering(), "失败态不该显示缓冲中");
    }

    #[test]
    fn terminal_phase_is_won_by_the_first_report() {
        let p = pipe(stereo(), 44_100, None);
        p.fail("先到的原因");
        p.mark_ended();
        assert_eq!(
            p.error().as_deref(),
            Some("先到的原因"),
            "终局只认第一次，后到的不许改写"
        );
    }

    #[test]
    fn position_freezes_while_starving() {
        let p = pipe(stereo(), 44_100, None);
        p.push(&[0.4, -0.2]);
        let mut source = PcmSource {
            pipe: Arc::clone(&p),
        };
        // 一整帧（两个样本）才算「走过一个时间刻度」，取半个帧位置还是 0
        assert!(source.next().is_some());
        assert!(source.next().is_some());
        let after_audio = p.position();
        assert!(after_audio > Duration::ZERO);
        for _ in 0..1_000 {
            assert!(matches!(p.take(), Frame::Silence));
        }
        assert_eq!(
            p.position(),
            after_audio,
            "缓冲期间位置必须停住，否则进度条在空转"
        );
    }

    #[test]
    fn a_full_ring_makes_the_producer_wait_for_room() {
        let p = pipe(stereo(), 44_100, None);
        let cap = capacity(&p);
        let wall = vec![0.5f32; cap];
        assert_eq!(p.push(&wall), Push::Accepted, "整环一次塞满");
        assert_eq!(queued(&p), cap);

        let blocked = {
            let p = Arc::clone(&p);
            std::thread::spawn(move || p.push(&[9.0, 9.0]))
        };
        std::thread::sleep(Duration::from_millis(30));
        assert!(!blocked.is_finished(), "环满时生产者应当等着");

        // 腾出够一批的空间，生产者就该进来
        for _ in 0..(BATCH + 1) {
            p.take();
        }
        assert_eq!(blocked.join().unwrap(), Push::Accepted);
        assert_eq!(p.lock().samples.back().copied(), Some(9.0), "新样本排队尾");
    }

    #[test]
    fn push_is_refused_once_stopped() {
        let p = pipe(stereo(), 44_100, None);
        p.stop();
        assert_eq!(p.push(&[1.0, 2.0]), Push::Stopped, "收工后不许再塞货");
        assert!(p.is_stopped());
    }

    #[test]
    fn stopping_ends_the_source_at_once() {
        let p = pipe(stereo(), 44_100, None);
        p.push(&[1.0, 2.0, 3.0]);
        p.stop();
        // 换台/停止路径靠这条：源立刻结束，旧曲子的尾巴不会继续响，
        // `Player::clear()` 那种「等队列真的空掉」的调用也就有界。
        assert!(matches!(p.take(), Frame::End), "收工后剩下的样本不该再放");
        assert!(!p.is_buffering(), "已收工不该报缓冲中");
    }

    #[test]
    fn stopping_keeps_the_failure_that_already_spoke() {
        let p = pipe(stereo(), 44_100, None);
        p.fail("下载中断");
        p.stop();
        assert_eq!(
            p.error().as_deref(),
            Some("下载中断"),
            "已经说出口的失败不该被「收工」抹成正常播完"
        );
    }

    #[test]
    fn waiting_producer_wakes_immediately_on_stop() {
        let p = pipe(stereo(), 44_100, None);
        let wall = vec![0.5f32; capacity(&p)];
        p.push(&wall);
        let started = Instant::now();
        let blocked = {
            let p = Arc::clone(&p);
            std::thread::spawn(move || p.push(&[1.0, 2.0]))
        };
        std::thread::sleep(Duration::from_millis(20));
        p.stop();
        assert_eq!(blocked.join().unwrap(), Push::Stopped);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "停止应立刻叫醒等待者，不该耗满超时"
        );
    }

    // ---- 跳转：输出侧登记，泵侧执行 ----

    #[test]
    fn seek_clears_the_ring_and_moves_the_clock_at_once() {
        let p = pipe(stereo(), 44_100, None);
        p.push(&[1.0; 8]);
        p.request_seek(Duration::from_secs(10));
        assert_eq!(queued(&p), 0, "旧位置的样本不属于新位置");
        assert!(
            p.position() >= Duration::from_millis(9_900),
            "进度条要立刻反映跳转，不等解码，实际 {:?}",
            p.position()
        );
        assert_eq!(p.take_seek(), Some(Duration::from_secs(10)));
        assert!(p.take_seek().is_none(), "跳转只交付一次");
    }

    #[test]
    fn seek_beyond_the_duration_clamps_the_clock() {
        let p = pipe(stereo(), 44_100, Some(Duration::from_secs(30)));
        p.request_seek(Duration::from_secs(999));
        assert!(
            p.position() <= Duration::from_secs(30),
            "越界的跳转不该把时钟推出曲目之外，实际 {:?}",
            p.position()
        );
    }

    #[test]
    fn pending_seek_drops_the_batch_being_built() {
        let p = pipe(stereo(), 44_100, None);
        p.request_seek(Duration::from_secs(1));
        assert_eq!(
            p.push(&[1.0, 2.0]),
            Push::SeekPending,
            "跳转待处理时攒的旧位置样本应作废"
        );
        assert_eq!(queued(&p), 0);
    }

    #[test]
    fn metadata_survives_the_ring_indirection() {
        let p = pipe(
            ChannelCount::new(1).unwrap(),
            48_000,
            Some(Duration::from_millis(1_234)),
        );
        let source = PcmSource { pipe: p };
        assert_eq!(source.channels().get(), 1);
        assert_eq!(source.sample_rate().get(), 48_000);
        assert_eq!(source.total_duration(), Some(Duration::from_millis(1_234)));
    }

    // ---- 端到端：缓冲 → 泵 → 环 → 输出 ----

    #[test]
    fn a_complete_buffer_decodes_through_the_pump() {
        let frames = 44_100; // 1 秒
        let (buf, path) = staged("complete", frames, frames);
        buf.mark_done();
        let (pump, source) = StreamPump::start(Arc::clone(&buf)).expect("完整缓冲应能起泵");
        let mut source = source;

        let got = drain(&mut source, frames * 2);
        assert_eq!(got.len(), frames * 2, "1 秒立体声应有 88200 个样本");
        // 斜坡起点抬到了 0.05：首样本明显非零，证明来自文件而不是补的静音
        assert!(
            got[0] > 0.045 && got[0] < 0.055,
            "首个样本应是斜坡起点，实际 {}",
            got[0]
        );
        assert!(
            got[2000] > 0.055 && got[2000] < 0.065,
            "第 1000 帧应约等于斜坡的 1/44，实际 {}",
            got[2000]
        );
        assert!(got[2001] < 0.0, "右声道应是左声道的相反数");

        assert!(runs_to_end(&mut source), "整首取完后输出侧应收工");
        assert_eq!(pump.error(), None, "播完不该带失败原因");
        assert!(!pump.is_buffering(), "已收工不该报缓冲中");
        assert!(
            pump.position() >= Duration::from_millis(900),
            "整段取走后位置应接近 1 秒，实际 {:?}",
            pump.position()
        );
        wait_until("解码线程退出", || pump.is_thread_done());
        pump.shutdown();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_starved_stream_yields_silence_and_resumes_when_bytes_arrive() {
        // 完整 2×0.1 秒的 WAV，但水位只给前一半，写入者不收工
        let part = 4_410;
        let (buf, path) = staged("starve", part * 2, part);
        let (pump, source) = StreamPump::start(Arc::clone(&buf)).expect("起泵");
        let mut source = source;

        // 取「一小部分」而不是「水位以内的全部」：解封装器按块读，
        // 最后一块越过水位就得等——能取到多少由块大小决定，不是本测试的判据。
        let heard = drain(&mut source, 2_000);
        assert!(heard[0] > 0.0, "取到的应是斜坡开头的真实样本");

        // 字节停止到达：先把手头能解码的都取干，状态就该固定在「静音 + 冻结」上
        let settle = Instant::now() + Duration::from_millis(500);
        while Instant::now() < settle {
            assert!(source.next().is_some(), "源头不该在缓冲中就结束");
        }
        let frozen = pump.position();
        assert!(frozen > Duration::ZERO);
        assert!(pump.is_buffering(), "应报缓冲中");
        assert_eq!(pump.error(), None, "缓冲中不是错误");
        assert!(!pump.is_thread_done(), "等字节不该把泵线程退掉");
        let mut silence = 0usize;
        for _ in 0..2_000 {
            if source.next() == Some(0.0) {
                silence += 1;
            }
        }
        assert_eq!(silence, 2_000, "字节停止到达后不该再有真实样本");
        assert_eq!(pump.position(), frozen, "静音不该推进进度条");

        // 后半段落盘 + 收工：解码器接着读，不必重新装载
        buf.commit(buf.declared().unwrap());
        buf.mark_done();
        let resumed = drain(&mut source, 4_000);
        assert!(
            resumed.iter().any(|v| *v > 0.2),
            "续上的应是后半段（斜坡值更大），不是静音"
        );
        assert!(
            pump.position() > frozen,
            "续播后位置应重新前进：{frozen:?} -> {:?}",
            pump.position()
        );
        assert!(runs_to_end(&mut source), "字节到齐后应能一路播完");
        assert_eq!(pump.error(), None, "续播播完不该带失败原因");
        assert!(
            pump.position() >= Duration::from_millis(190),
            "整首 0.2 秒应走完，实际 {:?}",
            pump.position()
        );
        pump.shutdown();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_failed_buffer_is_an_error_not_the_end_of_a_song() {
        let part = 4_410;
        let (buf, path) = staged("fail", part * 2, part);
        let (pump, source) = StreamPump::start(Arc::clone(&buf)).expect("起泵");
        let mut source = source;

        drain(&mut source, 2_000);
        buf.mark_failed("音频下载中断");

        assert!(runs_to_end(&mut source), "失败后已攒下的样本放完就该收工");
        assert_eq!(
            pump.error().as_deref(),
            Some("音频下载中断"),
            "写入失败必须报原因，不能伪装成播完"
        );
        assert!(!pump.is_buffering());
        wait_until("解码线程退出", || pump.is_thread_done());
        pump.shutdown();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_seek_on_the_output_thread_lands_on_the_new_audio() {
        let frames = 3 * 44_100; // 3 秒
        let (buf, path) = staged("seek", frames, frames);
        buf.mark_done();
        let (pump, source) = StreamPump::start(Arc::clone(&buf)).expect("起泵");
        let mut source = source;

        // 先证明起点在头部：取到的第一个样本是斜坡起点
        let head = drain(&mut source, 1);
        assert!(head[0] < 0.06, "起点样本应接近斜坡下端，实际 {}", head[0]);

        source.try_seek(Duration::from_secs(1)).expect("跳转");
        assert!(
            pump.position() >= Duration::from_millis(900),
            "跳转后时钟应立刻到新位置，实际 {:?}",
            pump.position()
        );

        // 环清空后先是一段静音，等泵线程解码到新位置的真实样本
        // 第 44100 帧 ≈ 0.05 + 0.4/3 = 0.183
        wait_until("跳转后取到新位置的样本", || {
            source.next().is_some_and(|v| v > 0.15 && v < 0.21)
        });
        pump.shutdown();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn shutdown_does_not_wait_for_a_blocked_producer() {
        // 水位只给前 0.1 秒、写入者不收工：泵必然等在下一次 read 上
        let part = 4_410;
        let (buf, path) = staged("shutdown", part * 2, part);
        let (pump, source) = StreamPump::start(Arc::clone(&buf)).expect("起泵");
        let mut source = source;
        drain(&mut source, 2_000);

        let started = Instant::now();
        pump.shutdown();
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "UI 路径上的收工不许等解码线程跑完，实际 {:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn aborting_the_buffer_frees_a_blocked_pump() {
        let part = 4_410;
        let (buf, path) = staged("abort", part * 2, part);
        let (pump, source) = StreamPump::start(Arc::clone(&buf)).expect("起泵");
        let mut source = source;

        // 取完一段后泵就卡在越不过去的那次 read 上
        let heard = drain(&mut source, 2_000);
        assert!(heard[0] > 0.0, "取消前已落盘的部分应照常给出");

        buf.abort();
        assert!(runs_to_end(&mut source), "取消后输出侧应尽快收工");
        assert_eq!(pump.error(), None, "被取消算安静结束，不该留下失败原因");
        wait_until("解码线程退出", || pump.is_thread_done());
        pump.shutdown();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unknown_length_refuses_to_stream() {
        let path = scratch_path("no_len");
        std::fs::write(&path, wav_bytes(&ramp_bytes(44_100, 0, 44_100))).unwrap();
        let buf = StreamBuffer::new(path.clone());
        buf.commit(1_024);
        let err = StreamPump::start(buf).unwrap_err();
        assert!(
            matches!(err, PlayerError::InvalidState { ref state, ref action }
                if state == "LengthUnknown" && action == "load_stream"),
            "总长未知要交调用方回退整首，实际 {err:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_non_audio_buffer_is_reported_as_a_decode_failure() {
        let path = scratch_path("not_audio");
        std::fs::write(&path, b"this is not an audio file at all, honest").unwrap();
        let buf = StreamBuffer::new(path.clone());
        buf.declare_len(41);
        buf.commit(41);
        buf.mark_done();
        let err = StreamPump::start(buf).unwrap_err();
        assert!(
            matches!(err, PlayerError::DecodeFailed { .. }),
            "认不出的容器要报解码失败，实际 {err:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_buffer_file_is_reported_as_a_read_failure() {
        // 声明过总长但文件不存在，且写入者已终局：开游标不该再等下去
        let buf = StreamBuffer::new(scratch_path("gone"));
        buf.declare_len(1_024);
        buf.commit(1_024);
        buf.mark_failed("文件已被清掉");
        let err = StreamPump::start(buf).unwrap_err();
        assert!(
            matches!(err, PlayerError::MediaRead { .. }),
            "文件不在要报读取失败，实际 {err:?}"
        );
    }

    #[test]
    fn a_complete_buffer_without_a_terminal_mark_still_ends_cleanly() {
        // 竞态的确定性版本：整首字节都公布了水位，写入者却没来得及写下终局
        // ——真实链路里这正是「最后一批 commit」与「mark_done」之间的那几微秒。
        // 泵不能把它读成「缓冲超时」：这首歌其实已经完整播完了。
        let frames = 44_100;
        let (buf, path) = staged("no_done", frames, frames);
        let (pump, source) = StreamPump::start(Arc::clone(&buf)).expect("起泵");
        let mut source = source;

        let got = drain(&mut source, frames * 2);
        assert_eq!(got.len(), frames * 2, "整首样本都应取到");
        assert!(runs_to_end(&mut source), "字节齐了输出侧就该收工");
        assert_eq!(pump.error(), None, "没人写终局不等于缓冲超时");
        assert!(!pump.is_buffering(), "收工后不该报缓冲中");
        wait_until("解码线程退出", || pump.is_thread_done());
        pump.shutdown();
        let _ = std::fs::remove_file(&path);
    }

    // ---- 限速桩验收：三档链路下的边下边播 ----

    /// 本模块用的起播闸门。
    ///
    /// 真实闸门由装载器按码率算（`stream_buffer::start_gate`，那边已有专测），无损档是 1.7 MB——
    /// 拿它套这里的百来 KB 素材，「过闸门」就等于「下完」，测的就不再是泵。
    /// 32 KB 只保证解封装器拿得到头 + 首批块，与真实链路同一条读取路径。
    const TEST_GATE: u64 = 32 * 1024;

    /// 跑完一档限速链路，把对账结果交给各档自己下判据。
    fn throttled(tag: &str, frames: usize, kb_per_s: u32) -> Audit {
        // 素材是无损档形态：44.1 kHz 立体声 16 bit = 176 KB/s 的**消耗**速率，
        // 变的只有下行速率。低于 176 KB/s 必然断供，高于则一路顺风。
        let (buf, path) = staged(tag, frames, 0);
        let writer = trickle(Arc::clone(&buf), kb_per_s);
        wait_until(&format!("{tag}: 水位过起播闸门"), || {
            buf.can_start_at(TEST_GATE)
        });

        let (pump, mut source) = StreamPump::start(Arc::clone(&buf)).expect("起泵");
        let a = audit(&pump, &mut source, &buf);
        let total = buf.declared().unwrap();
        let secs = frames as f64 / 44_100.0;

        assert_eq!(
            a.samples,
            frames * 2,
            "{tag}: 整首样本必须一个不少也不重复（静音补了 {} 个）",
            a.silence
        );
        assert!(
            a.ms_to_first < 800,
            "{tag}: 闸门已过却等了 {}ms 才出声",
            a.ms_to_first
        );
        assert!(
            a.watermark_at_first < total,
            "{tag}: 出声时水位已到 {} / 总长 {total}——那就不叫边下边播",
            a.watermark_at_first
        );
        assert_eq!(pump.error(), None, "{tag}: 限速不该报错");
        assert!(
            a.max_queued <= capacity(&pump.pipe),
            "{tag}: 环里峰值 {} 超过容量 {}",
            a.max_queued,
            capacity(&pump.pipe)
        );
        assert!(
            pump.position() >= Duration::from_secs_f64(secs * 0.95),
            "{tag}: 整首播完后位置应接近 {secs:.1}s，实际 {:?}",
            pump.position()
        );
        assert_eq!(buf.phase(), BufferPhase::Done, "{tag}: 写手应已收工");
        wait_until(&format!("{tag}: 解码线程退出"), || {
            pump.is_thread_done()
        });
        pump.shutdown();
        let _ = writer.join();
        let _ = std::fs::remove_file(&path);
        a
    }

    #[test]
    fn a_link_slower_than_the_lossless_bitrate_buffers_then_recovers() {
        // 0.5 秒无损（88 KB）配 45 KB/s：下行只有消耗的 1/4，断供是必然
        let a = throttled("slow_link", 22_050, 45);
        assert!(
            a.silence > 1_000,
            "断供期必须由输出侧补静音，实际静音 {} 个",
            a.silence
        );
        assert!(a.buffering > 0, "断供期间要能看到「缓冲中」");
    }

    #[test]
    fn a_link_at_the_lossless_bitrate_still_starts_before_the_file_is_downloaded() {
        // 2 秒无损（353 KB）配 200 KB/s：与消耗速率持平，最容易反复进出缓冲
        let a = throttled("even_link", 88_200, 200);
        assert!(
            a.silence > 0 && a.max_queued > 0,
            "贴着消耗速率的下行会在「攒一批」与「等一下」之间来回，实测静音 {} / 环峰值 {}",
            a.silence,
            a.max_queued
        );
    }

    #[test]
    fn a_link_faster_than_the_lossless_bitrate_finishes_without_a_reason_to_retry() {
        // 8 秒无损（1.4 MB）配 900 KB/s：下行是消耗的 5 倍
        let a = throttled("fast_link", 352_800, 900);
        assert!(a.max_queued > 0, "快链路应能看到环里攒着样本");
    }

    // ---- 端到端：真下载器 + 真 HTTP + 泵 ----

    #[test]
    fn a_real_downloader_feeds_the_pump_across_a_hold() {
        // 走完整的写侧（HttpAudioFetcher + StreamTask + 本地服务器），
        // 而不是手动推水位：服务器吐 150 KB 后按住 500 ms 再吐完，
        // 按住的那一段就是用户会看到的「缓冲中」——它必须既不断播也不报错。
        use crate::music::stream_transport::StreamTask;
        use crate::providers::test_support::{spawn_server, test_dir, Script};
        use crate::providers::HttpAudioFetcher;

        let frames = 3 * 44_100; // 3 秒无损 ≈ 1.27 MB
        let body = wav_bytes(&ramp_bytes(frames, 0, frames));
        let (port, _seen) = spawn_server(
            body.clone(),
            vec![Script::SlowSplit],
            150_000,
            Duration::from_millis(500),
        );
        let dir = test_dir("pump_http");
        let buf = StreamBuffer::new(dir.join("p.wav"));
        // 空闲超时给到 5 秒：本测试要验的是「按住」而不是「续传」，
        // 别让它被自身的重连逻辑提前叫醒。
        let fetcher = Arc::new(
            HttpAudioFetcher::new()
                .unwrap()
                .with_idle_timeout(Duration::from_secs(5))
                .with_write_buffer(16 * 1024),
        );
        let task = StreamTask::spawn(
            fetcher,
            format!("http://127.0.0.1:{port}/a.wav"),
            Arc::clone(&buf),
            None,
        );

        wait_until("真下载器过闸门", || buf.can_start_at(TEST_GATE));
        let (pump, mut source) = StreamPump::start(Arc::clone(&buf)).expect("起泵");
        let a = audit(&pump, &mut source, &buf);

        assert_eq!(a.samples, frames * 2, "整首样本一个不能少");
        assert!(
            a.watermark_at_first < body.len() as u64,
            "出声时整首还没下完（水位 {} / 总长 {}）",
            a.watermark_at_first,
            body.len()
        );
        assert!(a.buffering > 0, "按住的那 500ms 要能看到「缓冲中」");
        assert_eq!(pump.error(), None, "断流恢复不是错误");
        assert_eq!(buf.phase(), BufferPhase::Done, "后半段应续上并收工");
        assert!(
            pump.position() >= Duration::from_millis(2_850),
            "播完位置应接近 3 秒，实际 {:?}",
            pump.position()
        );
        pump.shutdown();
        // 线程已自然跑完：join 只是确认终局没有被取消改写
        assert_eq!(task.join(), BufferPhase::Done);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 真实 FLAC：边下边播时解码器怎么走 ----

    #[test]
    fn a_real_flac_stream_decodes_before_the_last_byte_lands() {
        // 用**真 FLAC 容器**（手写 verbatim 帧）走同一条链路，补上 WAV 素材验不到的那一半：
        // symphonia 的 FLAC 解析器是按帧头 + 帧尾 CRC-16 认帧边界的，帧尾没到就探不到
        // 下一帧，因此「帧跨过水位」正是边下边播的真实形态。这里服务器吐前半段后按住
        // 500 ms：出声必须发生在前半段之内，且整首样本一个不能少。
        use crate::music::stream_transport::StreamTask;
        use crate::providers::test_support::{
            flac_verbatim, spawn_server, test_dir, Script, FLAC_BLOCK, FLAC_HEADER_LEN,
        };
        use crate::providers::HttpAudioFetcher;

        let frames = 22 * FLAC_BLOCK; // ≈ 2.04 秒
        let (body, frame_len) = flac_verbatim(frames);
        // 按住点掐在**第 11 帧中段**：前 11 帧是完整的，第 12 帧只给到一半就停手 500 ms。
        // 掐在帧起点只等于「一整帧没到」，解码器拿齐前面的帧照样走，逼不出真实等待。
        let hold_at = FLAC_HEADER_LEN + 11 * frame_len + frame_len / 2;
        let (port, _seen) = spawn_server(
            body.clone(),
            vec![Script::SlowSplit],
            hold_at,
            Duration::from_millis(500),
        );

        let dir = test_dir("pump_flac");
        let buf = StreamBuffer::new(dir.join("a.flac"));
        let fetcher = Arc::new(
            HttpAudioFetcher::new()
                .unwrap()
                .with_idle_timeout(Duration::from_secs(5))
                .with_write_buffer(16 * 1024),
        );
        let task = StreamTask::spawn(
            fetcher,
            format!("http://127.0.0.1:{port}/a.flac"),
            Arc::clone(&buf),
            None,
        );

        wait_until("真下载器过闸门", || buf.can_start_at(TEST_GATE));
        let (pump, mut source) = StreamPump::start(Arc::clone(&buf)).expect("起泵");
        let a = audit(&pump, &mut source, &buf);

        assert_eq!(a.samples, frames * 2, "整首样本一个不能少");
        // 出声必须发生在整首落盘**之前**。判据只能是总长，不能是按住点：本机回环下行极快，
        // 水位会在解码器出第一声之前就冲到按住点，用按住点当上限是把竞态当成缺陷。
        assert!(
            a.watermark_at_first < body.len() as u64,
            "出声时水位 {} 已是整首 {}——那就不叫边下边播",
            a.watermark_at_first,
            body.len()
        );
        assert!(
            a.buffering > 0,
            "按住的那 500ms 要能看到「缓冲中」，实际静音 {} 个",
            a.silence
        );
        assert_eq!(pump.error(), None, "断供恢复不是错误");
        assert_eq!(buf.phase(), BufferPhase::Done, "后半段应续上并收工");
        assert!(
            pump.position() >= Duration::from_secs_f64(frames as f64 / 44_100.0 * 0.95),
            "播完位置应接近 {:.2}s，实际 {:?}",
            frames as f64 / 44_100.0,
            pump.position()
        );
        pump.shutdown();
        assert_eq!(task.join(), BufferPhase::Done);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 大素材：50 MB 级无损档一整遍 ----

    /// 手动验收：50 MB 级无损档一整遍，逐样本核对 + 环占用上限 + 吞吐。
    ///
    /// `cargo test --lib audio::stream_pump -- --ignored --nocapture`
    ///
    /// 默认不跑：要写 50 MiB 临时文件、并在测试里过两千六百万个样本，
    /// 量级和套件其余部分差两个数量级；验收时手动执行并读数。
    #[test]
    #[ignore = "写 50 MiB 临时文件、跑一整遍母带量级；手动验收时执行"]
    fn fifty_megabytes_of_lossless_play_through_without_losing_a_sample() {
        let frames = 13_200_000; // 299 秒 × 44.1 kHz × 立体声 × 4 字节 ≈ 50.4 MiB
        let path = scratch_path("fifty");
        let total = write_wav_file(&path, frames);
        assert!(
            total > 50 * 1024 * 1024,
            "验收要求是 50 MiB 级，实际 {total} 字节"
        );

        let buf = StreamBuffer::new(path.clone());
        buf.declare_len(total);
        buf.commit(total);
        buf.mark_done();

        let (pump, mut source) = StreamPump::start(Arc::clone(&buf)).expect("起泵");
        let started = Instant::now();
        let a = audit(&pump, &mut source, &buf);
        let elapsed = started.elapsed();

        let cap = capacity(&pump.pipe);
        let mib_per_s = total as f64 / 1_048_576.0 / elapsed.as_secs_f64();
        // 无损档形态本身的**消耗**速率：44.1 kHz × 立体声 × 2 字节
        let realtime_mib_per_s = 44_100.0 * 4.0 / 1_048_576.0;
        println!(
            "50 MiB 无损：样本 {}（期望 {}）| 静音 {} | 环峰值 {}/{} | 用时 {elapsed:?} | \
             解码 {:.1} MiB/s = 实时消耗的 {:.0} 倍",
            a.samples,
            frames * 2,
            a.silence,
            a.max_queued,
            cap,
            mib_per_s,
            mib_per_s / realtime_mib_per_s,
        );
        println!(
            "起播：{}ms 时水位 {} 字节 | 结束位置 {:?} | 失败 {:?}",
            a.ms_to_first,
            a.watermark_at_first,
            pump.position(),
            pump.error()
        );

        assert_eq!(a.samples, frames * 2, "两千六百多万个样本必须一一对上");
        // 静音数只说明「测试这个消费者比泵快」，不是用户会看到的东西：
        // 真实混音器按 176 KB/s 取样本，比这里量到的解码速率慢两个数量级。
        // 于是判据换成泵自己跟不跟得上播放：debug 构建也要留下足够余量。
        assert!(
            mib_per_s > realtime_mib_per_s * 20.0,
            "解码只有实时消耗的 {:.1} 倍余量，撑不住无损档播放",
            mib_per_s / realtime_mib_per_s
        );
        assert!(a.max_queued <= cap, "环占用不许超过 2 秒覆盖量");
        assert_eq!(pump.error(), None);
        assert!(
            pump.position() >= Duration::from_secs(297),
            "播完位置应接近 299 秒，实际 {:?}",
            pump.position()
        );
        pump.shutdown();
        let _ = std::fs::remove_file(&path);
    }
}
