//! 边下边播的共享字节缓冲（方案 2A 的地基）。
//!
//! 一句话：**文件在长，解码器在读，两边不能互相骗**。
//!
//! - 写入者（[`crate::music::stream_transport`] 的写线程，字节由
//!   [`crate::providers::http_fetch`] 的流式下载落盘）每落盘一块就
//!   [`StreamBuffer::commit`] 推进水位；
//! - 读取者（[`StreamReader`]，交给 rodio 的解码器）读到水位线以内就取数据，
//!   **越线就等**，绝不返回假的 EOF。
//!
//! 为什么不能返回 EOF：解码器把 `Ok(0)` 当「文件放完了」，播放会提前结束并
//! 自动跳下一首（实测稀疏预分配方案就是这样翻车的，见
//! `docs/NETEASE_PHASE3_PLAN.md` 的方案选型记录）。等待则表现为「缓冲中」，
//! 字节一到就继续。
//!
//! 同样**不做磁盘预分配**（不 `set_len` 到全长）：预分配会让「越界读到空洞」
//! 变成静默的零字节，比 EOF 更糟——听出来是一片静音却没有任何错误。
//!
//! 总字节数以响应头的 `Content-Length` 为准（[`StreamBuffer::declare_len`]）：
//! 解码器要拿它算 `byte_len` 与 seek 边界，而**磁盘上的实时长度是不可信的**
//! （它总是落后于水位）。没有 `Content-Length` 的响应直接判
//! [`BufferPhase::LengthUnknown`]，由调用方回退整首下载。
//!
//! 起播不等整首下完：[`start_gate`] 把「攒够多少字节」按**音频秒数**换算
//! （按字节定等于按音质定），[`StreamBuffer::can_start_at`] 据此判定可否交解码器。

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// 水位连续静止这么久仍未推进，读侧判定写入者已死。
///
/// 必须大于 HTTP 侧的空闲超时（15 s）+ 一次重连，否则正常的断流续传会被
/// 解码器误判成失败。
pub const READ_PATIENCE: Duration = Duration::from_secs(45);

/// 起播前想提前攒下的音频时长。
///
/// 为什么是 10 秒而不是「攒够 8 MB」：**按字节定闸门等于按音质定闸门**——
/// 320 kbps 的 MP3 攒 8 MB 是 3 分钟的量（白等），母带 FLAC 攒 8 MB 却只有
/// 27 秒（照样饿）。按秒定才有物理意义：10 秒 ≈ 传输层一次空闲超时重连
/// （15 s 内自愈，见 `providers::http_fetch`）+ RTT + 调度抖动的余量，
/// 断流续传期间解码器手里仍有可放的字节。
pub const START_LEAD: Duration = Duration::from_secs(10);

/// 闸门下限：容器头 + 首批帧，再低的码率也得攒这么多。
pub const MIN_START_GATE: u64 = 512 * 1024;

/// 闸门上限：慢网上起播别等太久，高码率档靠「边播边攒」而不是多等。
///
/// 本机实测落盘带宽 84.7 MiB/s：8 MB 约 155 ms，磁盘从来不是瓶颈；上限锁的是
/// **下行**要等多久（0.29 MB/s 的母带档下 8 MB = 27 s，那已经是不能接受的值）。
pub const MAX_START_GATE: u64 = 8 * 1024 * 1024;

/// 码率未知时的闸门。
///
/// 接口没给 `br` 就退回到「一刀切字节数」：取任务实测的无损甜点 2 MB——
/// 对 320 kbps 是 50 s（偏保守但起播仍只需百来毫秒），对无损是 ~17 s（够用）。
const UNKNOWN_BITRATE_GATE: u64 = 2 * 1024 * 1024;

/// 起播闸门：攒够多少字节才值得把缓冲交给解码器。
///
/// 输入是接口声明的码率（kbps，见 [`crate::music::provider::PlaybackSource::bitrate_kbps`]）；
/// `None` 表示接口没给，走 [`UNKNOWN_BITRATE_GATE`]。
///
/// 结果还要被整首长度夹一次——两三分钟的短曲子可能整个文件都不到闸门，那时
/// 「攒够」就等于「下完」，[`StreamBuffer::can_start_at`] 已经按 `min(gate, declared)` 处理。
pub fn start_gate(bitrate_kbps: Option<u32>) -> u64 {
    let Some(kbps) = bitrate_kbps.filter(|k| *k > 0) else {
        return UNKNOWN_BITRATE_GATE;
    };
    let bytes_per_second = kbps as u64 * 1_000 / 8;
    (bytes_per_second * START_LEAD.as_secs()).clamp(MIN_START_GATE, MAX_START_GATE)
}

/// 缓冲的进展阶段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BufferPhase {
    /// 仍在写入。
    Growing,
    /// 全部字节已落盘（读侧越过水位即真 EOF）。
    Done,
    /// 写入失败或续传次数用尽，携带人话原因。
    Failed(String),
    /// 响应没有 `Content-Length`：总长未知，流式解码不安全。
    LengthUnknown,
    /// 被上层取消（换台/停止）：读侧应立即失败，不再等。
    Aborted,
}

impl BufferPhase {
    /// 是否已是终局（不再会有新字节）。
    pub fn is_terminal(&self) -> bool {
        !matches!(self, BufferPhase::Growing)
    }
}

/// 一条在线曲目的共享字节缓冲。
///
/// 句柄以 `Arc` 形式同时交给写入线程与解码线程；内部一把锁 + 一个条件变量，
/// 热路径（读已落盘的数据）不碰网络。
pub struct StreamBuffer {
    path: PathBuf,
    state: Mutex<State>,
    ready: Condvar,
    /// 取消标志独立于锁：写入者在每个分片之间自查，不必抢读者的锁。
    aborted: AtomicBool,
}

struct State {
    /// 响应头声明的整首字节数；未知为 `None`。
    declared: Option<u64>,
    /// 已落盘水位（字节）。
    written: u64,
    phase: BufferPhase,
    /// 最近一次水位推进的时刻。
    progressed_at: Instant,
}

impl StreamBuffer {
    /// 建缓冲（文件还不存在，由写入者创建）。
    pub fn new(path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            path,
            state: Mutex::new(State {
                declared: None,
                written: 0,
                phase: BufferPhase::Growing,
                progressed_at: Instant::now(),
            }),
            ready: Condvar::new(),
            aborted: AtomicBool::new(false),
        })
    }

    /// 缓冲对应的临时文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 声明整首字节数（拿到响应头时调用一次）。
    pub fn declare_len(&self, total: u64) {
        let mut st = self.lock();
        if st.declared.is_none() {
            st.declared = Some(total);
        }
    }

    /// 响应没有 `Content-Length`：终局，交调用方回退整首。
    pub fn mark_length_unknown(&self) {
        let mut st = self.lock();
        if matches!(st.phase, BufferPhase::Growing) {
            st.phase = BufferPhase::LengthUnknown;
        }
        drop(st);
        self.ready.notify_all();
    }

    /// 水位推进到 `written`（写盘成功之后调用；只增不减）。
    pub fn commit(&self, written: u64) {
        let mut st = self.lock();
        if written > st.written {
            st.written = written;
            st.progressed_at = Instant::now();
        }
        drop(st);
        // 唤醒所有等待者：多个读者（播放 + 探测）可能都在等同一段字节
        self.ready.notify_all();
    }

    /// 全部写完。
    pub fn mark_done(&self) {
        self.finish(BufferPhase::Done);
    }

    /// 写入失败（携带人话原因，不含 URL/堆栈）。
    pub fn mark_failed(&self, reason: impl Into<String>) {
        self.finish(BufferPhase::Failed(reason.into()));
    }

    fn finish(&self, phase: BufferPhase) {
        let mut st = self.lock();
        if !st.phase.is_terminal() {
            st.phase = phase;
        }
        drop(st);
        self.ready.notify_all();
    }

    /// 取消：请写入者停手，并叫醒正在等字节的读者。
    ///
    /// 幂等；已终局的缓冲不再改变状态（失败原因比「被取消」更值得保留）。
    pub fn abort(&self) {
        self.aborted.store(true, Ordering::SeqCst);
        let mut st = self.lock();
        if matches!(st.phase, BufferPhase::Growing) {
            st.phase = BufferPhase::Aborted;
        }
        drop(st);
        self.ready.notify_all();
    }

    /// 写入者是否已被要求停止（分片之间自查，开销一次原子读）。
    pub fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::SeqCst)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // 锁中毒只在临界区 panic 时出现，而这里的临界区只有字段读写；
        // 取回内部值继续跑，比让一次瞬时 panic 卡死整条播放链路更可取。
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 当前水位（已落盘字节数）。
    pub fn written(&self) -> u64 {
        self.lock().written
    }

    /// 声明的整首字节数。
    pub fn declared(&self) -> Option<u64> {
        self.lock().declared
    }

    /// 当前阶段。
    pub fn phase(&self) -> BufferPhase {
        self.lock().phase.clone()
    }

    /// 是否已可交给解码器起播：总长已知 + 水位到闸门。
    ///
    /// 闸门按 `min(gate, declared)` 取，短曲子不该被「10 秒音频量」卡住。
    pub fn can_start_at(&self, gate: u64) -> bool {
        let st = self.lock();
        st.declared
            .is_some_and(|total| st.written >= gate.min(total))
    }

    /// 水位静止这么久（**没有推进**）——装载侧用它判断「写手已经不在了」。
    ///
    /// 与读侧的 [`StreamReader`] 耐心同一判据：慢网只要一直在推进就不算停滞，
    /// 空闲重连期间不动水位才是真信号。
    pub fn is_stalled(&self, patience: Duration) -> bool {
        self.lock().progressed_at.elapsed() >= patience
    }

    /// 阻塞等待水位推进到至少 `min`，或进入终局。
    ///
    /// `patience` 是「水位静止」的上限：写入者空闲重连期间水位不动是正常的，
    /// 所以计时基准是**最近一次推进**，不是调用时刻。
    fn wait_for(&self, min: u64, patience: Duration) -> Wait {
        let mut st = self.lock();
        loop {
            if st.written >= min {
                return Wait::Enough;
            }
            match &st.phase {
                BufferPhase::Growing => {}
                BufferPhase::Done => return Wait::Ended,
                other => return Wait::Gone(other.clone()),
            }
            // 每次醒来都按最新水位时刻重算：慢网只要一直在推进，就不该耗尽耐心
            let deadline = st.progressed_at + patience;
            let now = Instant::now();
            if now >= deadline {
                return Wait::Stalled;
            }
            let (next, _waited) = match self.ready.wait_timeout(st, deadline - now) {
                Ok(pair) => pair,
                Err(e) => e.into_inner(),
            };
            st = next;
        }
    }
}

/// 手写而非 derive：`Condvar` 没有 `Debug`，而诊断只关心「哪首曲子、下到哪、
/// 收工没有」——这三项也正是 `PreparedTrack` 的 `Debug` 需要转发的内容。
impl std::fmt::Debug for StreamBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let st = self.lock();
        f.debug_struct("StreamBuffer")
            .field("path", &self.path)
            .field("declared", &st.declared)
            .field("written", &st.written)
            .field("phase", &st.phase)
            .field("aborted", &self.is_aborted())
            .finish()
    }
}

/// [`StreamBuffer::wait_for`] 的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Wait {
    /// 要的字节已落盘。
    Enough,
    /// 写入者收工且字节不够 —— 这就是真 EOF。
    Ended,
    /// 写入者失败/被取消，携带终局原因。
    Gone(BufferPhase),
    /// 水位静止太久：写入者大概已经不在了。
    Stalled,
}

/// 只读游标：把「还在长的文件」呈现成一个规规矩矩的 `Read + Seek` 源。
///
/// 交给 `rodio::Decoder` 用（`Read + Seek + Send + Sync + 'static`）。
/// **每个游标自带位置**，可以开多个（播放一个、探测一个）。
pub struct StreamReader {
    buf: Arc<StreamBuffer>,
    pos: u64,
    file: File,
    patience: Duration,
}

impl StreamReader {
    /// 在缓冲上开一个从头开始的游标。
    pub fn open(buf: Arc<StreamBuffer>) -> io::Result<Self> {
        Self::at(buf, 0)
    }

    /// 在缓冲上开一个定位到 `pos` 的游标。
    pub fn at(buf: Arc<StreamBuffer>, pos: u64) -> io::Result<Self> {
        // 文件由写入者在第一个字节之前创建；闸门等到过字节才允许开游标，
        // 所以这里正常情况下已存在。仍容忍一次「还没有」的竞态。
        let file = loop {
            match File::open(&buf.path) {
                Ok(f) => break f,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    if buf.phase().is_terminal() {
                        return Err(e);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => return Err(e),
            }
        };
        let mut cursor = Self {
            buf,
            pos: 0,
            file,
            patience: READ_PATIENCE,
        };
        cursor.seek(SeekFrom::Start(pos))?;
        Ok(cursor)
    }

    /// 覆盖等待耐心（测试用）。
    pub fn with_patience(mut self, patience: Duration) -> Self {
        self.patience = patience;
        self
    }

    /// 缓冲的共享句柄（引擎侧需要它做取消与状态查询）。
    pub fn buffer(&self) -> &Arc<StreamBuffer> {
        &self.buf
    }

    fn err(phase: &BufferPhase) -> io::Error {
        let (kind, msg) = match phase {
            BufferPhase::Failed(reason) => (io::ErrorKind::Other, reason.as_str()),
            BufferPhase::Aborted => (io::ErrorKind::Interrupted, "装载已被取消"),
            BufferPhase::LengthUnknown => (io::ErrorKind::Unsupported, "响应未声明总长"),
            BufferPhase::Growing => (io::ErrorKind::Other, "缓冲仍在写入"),
            BufferPhase::Done => (io::ErrorKind::UnexpectedEof, "音频字节不完整"),
        };
        io::Error::new(kind, msg)
    }
}

impl Read for StreamReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        // 能取多少：已落盘水位以内。越过水位的部分**等**，不返回 EOF。
        let available = loop {
            let st = self.buf.lock();
            let avail = st.written.saturating_sub(self.pos);
            if avail > 0 {
                break avail;
            }
            drop(st);
            match self.buf.wait_for(self.pos + 1, self.patience) {
                Wait::Enough => continue,
                Wait::Ended => return Ok(0),
                Wait::Gone(phase) => return Err(Self::err(&phase)),
                Wait::Stalled => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "音频字节停止到达（缓冲超时）",
                    ))
                }
            }
        };
        let want = available.min(out.len() as u64) as usize;
        use std::io::Seek as _;
        self.file.seek(SeekFrom::Start(self.pos))?;
        let n = self.file.read(&mut out[..want])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for StreamReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let declared = self.buf.declared();
        let base: i64 = match from {
            // 超出 i64 的偏移不可能是合法音频位置，夹到上限后面按总长截断
            SeekFrom::Start(n) => i64::try_from(n).unwrap_or(i64::MAX),
            SeekFrom::End(off) => {
                let end = declared.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::Unsupported, "总长未知，无法按尾部定位")
                })?;
                i64::try_from(end).unwrap_or(i64::MAX).saturating_add(off)
            }
            SeekFrom::Current(off) => (self.pos as i64).saturating_add(off),
        };
        if base < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "定位到文件开始之前",
            ));
        }
        let mut target = base as u64;
        // 已知总长时夹到文件末尾：解码器会试 `seek(End(0))` 探长度，
        // 越过末尾的等待是等不到的（水位永远不会超过总长）。
        if let Some(total) = declared {
            target = target.min(total);
        }
        self.pos = target;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一段已经落盘的文件 + 缓冲，水位手动推进。
    fn buffer_with(dir: &str, name: &str, bytes: &[u8], total: u64) -> Arc<StreamBuffer> {
        let path = std::env::temp_dir().join(format!(
            "reimuplayer_stream_{}_{}_{}",
            dir,
            std::process::id(),
            name
        ));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        let buf = StreamBuffer::new(path);
        buf.declare_len(total);
        buf.commit(bytes.len() as u64);
        buf
    }

    fn cleanup(buf: &StreamBuffer) {
        let _ = std::fs::remove_file(buf.path());
    }

    #[test]
    fn reads_only_what_has_been_committed() {
        let buf = buffer_with("read", "a.bin", b"hello", 100);
        let mut cur = StreamReader::open(Arc::clone(&buf))
            .unwrap()
            .with_patience(Duration::from_millis(50));
        let mut got = [0u8; 16];
        // 水位只有 5 字节：一次 read 最多给 5，绝不越过
        let n = cur.read(&mut got).unwrap();
        assert_eq!(&got[..n], b"hello");
        cleanup(&buf);
    }

    #[test]
    fn waiting_reader_wakes_when_the_writer_commits() {
        let buf = buffer_with("wait", "b.bin", b"ab", 10);
        let mut cur = StreamReader::open(Arc::clone(&buf)).unwrap();
        let mut sink = [0u8; 2];
        cur.read_exact(&mut sink).unwrap();
        assert_eq!(&sink, b"ab");

        // 第三个字节还没到：开一个后台线程延迟提交，主线程应当等到它而不是 EOF
        let writer = {
            let buf = Arc::clone(&buf);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(120));
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(buf.path())
                    .and_then(|mut f| {
                        use std::io::Write;
                        f.seek(SeekFrom::Start(2))?;
                        f.write_all(b"cd")
                    })
                    .unwrap();
                buf.commit(4);
                buf.mark_done();
            })
        };
        let started = Instant::now();
        let mut rest = vec![0u8; 2];
        cur.read_exact(&mut rest).unwrap();
        assert_eq!(&rest, b"cd", "等待期间到达的字节应原样读到");
        assert!(
            started.elapsed() >= Duration::from_millis(80),
            "确实等过，不是碰巧读到"
        );
        writer.join().unwrap();
        // Done 之后越过的读取是真 EOF
        let mut tail = [0u8; 4];
        assert_eq!(cur.read(&mut tail).unwrap(), 0, "写完就是 EOF");
        cleanup(&buf);
    }

    #[test]
    fn done_before_the_watermark_reports_clean_eof() {
        // 写入者宣告完成但字节比声明的少（断流后放弃）：读侧到水位即 EOF，
        // 解码器会把它当「播完」，由上层判断是否提前结束。
        let buf = buffer_with("short", "c.bin", b"xyz", 100);
        buf.mark_done();
        let mut cur = StreamReader::open(Arc::clone(&buf)).unwrap();
        let mut got = [0u8; 8];
        assert_eq!(cur.read(&mut got).unwrap(), 3);
        assert_eq!(cur.read(&mut got).unwrap(), 0);
        cleanup(&buf);
    }

    #[test]
    fn stalled_wait_becomes_timeout_not_eof() {
        let buf = buffer_with("stall", "d.bin", b"ab", 100);
        let mut cur = StreamReader::open(Arc::clone(&buf))
            .unwrap()
            .with_patience(Duration::from_millis(60));
        let mut sink = [0u8; 2];
        cur.read_exact(&mut sink).unwrap();
        let err = cur.read(&mut sink).unwrap_err();
        assert_eq!(
            err.kind(),
            io::ErrorKind::TimedOut,
            "停止到达要报超时，绝不能报 EOF"
        );
        cleanup(&buf);
    }

    #[test]
    fn abort_wakes_a_blocked_reader_immediately() {
        let buf = buffer_with("abort", "e.bin", b"ab", 100);
        let blocked = {
            let buf = Arc::clone(&buf);
            std::thread::spawn(move || {
                let mut cur = StreamReader::open(buf)
                    .unwrap()
                    .with_patience(Duration::from_secs(30));
                let mut sink = [0u8; 2];
                cur.read_exact(&mut sink).unwrap();
                let mut more = [0u8; 2];
                cur.read(&mut more).map(|n| n as i32).unwrap_or(-1)
            })
        };
        std::thread::sleep(Duration::from_millis(60));
        let t = Instant::now();
        buf.abort();
        let code = blocked.join().unwrap();
        assert_eq!(code, -1, "取消后读必须报错返回");
        assert!(
            t.elapsed() < Duration::from_secs(1),
            "取消要立刻叫醒等待者，不用等耐心耗尽"
        );
        cleanup(&buf);
    }

    #[test]
    fn failed_phase_surfaces_the_reason() {
        let buf = buffer_with("fail", "f.bin", b"ab", 100);
        buf.mark_failed("音频下载失败");
        let mut cur = StreamReader::open(Arc::clone(&buf))
            .unwrap()
            .with_patience(Duration::from_millis(30));
        let mut sink = [0u8; 2];
        cur.read_exact(&mut sink).unwrap();
        let err = cur.read(&mut sink).unwrap_err();
        assert!(err.to_string().contains("音频下载失败"), "{err}");
        cleanup(&buf);
    }

    #[test]
    fn seek_uses_declared_length_not_disk_length() {
        // 磁盘只有 2 字节，声明 100：seek(End(0)) 必须是 100，
        // 否则解码器会以为文件就这么长（预分配方案的反面教材）。
        let buf = buffer_with("seek", "g.bin", b"ab", 100);
        let mut cur = StreamReader::open(Arc::clone(&buf)).unwrap();
        use std::io::Seek;
        assert_eq!(cur.seek(SeekFrom::End(0)).unwrap(), 100);
        assert_eq!(cur.seek(SeekFrom::End(-10)).unwrap(), 90);
        assert_eq!(cur.seek(SeekFrom::Start(7)).unwrap(), 7);
        assert_eq!(cur.seek(SeekFrom::Current(3)).unwrap(), 10);
        assert_eq!(cur.stream_position().unwrap(), 10);
        // 越过总长的定位被夹住，不会留下等不到的等待
        assert_eq!(cur.seek(SeekFrom::Start(9999)).unwrap(), 100);
        assert!(
            cur.seek(SeekFrom::Current(-99_999)).is_err(),
            "不允许定位到 0 之前"
        );
        cleanup(&buf);
    }

    #[test]
    fn unknown_length_refuses_end_seek_and_gates_start() {
        let path = std::env::temp_dir().join(format!(
            "reimuplayer_stream_nolen_{}.bin",
            std::process::id()
        ));
        std::fs::write(&path, b"ab").unwrap();
        let buf = StreamBuffer::new(path.clone());
        buf.commit(2);
        assert!(!buf.can_start_at(1), "总长未知时不许起播");
        buf.mark_length_unknown();
        assert_eq!(buf.phase(), BufferPhase::LengthUnknown);
        let mut cur = StreamReader::open(Arc::clone(&buf)).unwrap();
        use std::io::Seek;
        assert!(
            cur.seek(SeekFrom::End(0)).is_err(),
            "没有 Content-Length 就不该按尾部定位"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_gate_is_capped_by_the_declared_length() {
        // 短曲子（总长 10 字节）不该被「4 MB 闸门」卡住：闸门按 min(gate, declared) 取
        let short = buffer_with("gate", "h.bin", b"0123456789", 10);
        assert!(
            short.can_start_at(4 * 1024 * 1024),
            "整首字节都落盘了就该起播，不必等 Done"
        );

        // 反过来：长曲子水位没到闸门就不该起播，否则解码器第一口就撞等待
        let long = buffer_with("gate", "j.bin", &vec![0u8; 1024 * 1024], 100 * 1024 * 1024);
        assert!(!long.can_start_at(4 * 1024 * 1024), "水位未过闸门不起播");
        long.commit(4 * 1024 * 1024);
        assert!(
            long.can_start_at(4 * 1024 * 1024),
            "到闸门即可起播，不必等整首下完"
        );
        cleanup(&short);
        cleanup(&long);
    }

    #[test]
    fn the_gate_is_measured_in_audio_seconds_not_fixed_bytes() {
        // 换算式：kbps × 1000 ÷ 8 × 10 s，再夹进 [512 KiB, 8 MiB]。
        // 逐个数值都是实测档位的真实消耗速率：
        //   128 / 320 kbps：160 KB / 400 KB —— 都在下限之下，抬到 512 KiB
        //   CD 无损 1411 kbps：1.76 MB
        //   母带（90 MB / 302 s ≈ 2432 kbps）：3.04 MB
        //   7000 kbps：12.5 MB —— 压回上限 8 MiB，慢网上起播不该等半分钟
        assert_eq!(start_gate(Some(128)), MIN_START_GATE);
        assert_eq!(start_gate(Some(320)), MIN_START_GATE);
        assert_eq!(start_gate(Some(419)), MIN_START_GATE);
        assert_eq!(start_gate(Some(420)), 525_000);
        assert_eq!(start_gate(Some(962)), 1_202_500);
        assert_eq!(start_gate(Some(1411)), 1_763_750);
        assert_eq!(start_gate(Some(2432)), 3_040_000);
        assert_eq!(start_gate(Some(6711)), MAX_START_GATE);
        assert_eq!(start_gate(Some(u32::MAX)), MAX_START_GATE, "不溢出");

        // 接口没给码率 = 按字节的老办法，取无损甜点 2 MB；0 同样视为没给
        assert_eq!(start_gate(None), 2 * 1024 * 1024);
        assert_eq!(start_gate(Some(0)), 2 * 1024 * 1024);

        // 码率更高 ⇒ 闸门不更小（否则高码率反而更早饿死）
        let mut prev = 0u64;
        for kbps in [128, 320, 962, 1411, 2432, 4608, 7000, 9999] {
            let gate = start_gate(Some(kbps));
            assert!(
                gate >= prev,
                "{kbps} kbps 的闸门 {gate} 小于低码率的 {prev}"
            );
            prev = gate;
        }
    }

    #[test]
    fn a_lossless_track_starts_at_three_megabytes_not_ninety() {
        // 本任务要解决的那个数字：90 MB 母带档，过去要等整首下完才起播。
        let declared = 90 * 1024 * 1024;
        let gate = start_gate(Some(2432));
        assert!(
            gate * 30 < declared,
            "闸门应远小于整首，否则等于没做边下边播"
        );

        let buf = buffer_with(
            "gate_mb",
            "master.flac",
            &vec![0u8; gate as usize],
            declared,
        );
        assert!(
            buf.can_start_at(gate),
            "攒够 10 秒音频量（{} B）就该起播",
            gate
        );
        cleanup(&buf);
    }

    #[test]
    fn commit_is_monotonic() {
        let path = std::env::temp_dir().join(format!(
            "reimuplayer_stream_mono_{}.bin",
            std::process::id()
        ));
        std::fs::write(&path, b"").unwrap();
        let buf = StreamBuffer::new(path.clone());
        buf.declare_len(500);
        buf.commit(100);
        buf.commit(40);
        assert_eq!(buf.written(), 100, "水位只增不减：重连后的旧数值不能倒退它");
        // 总长只认第一次声明：206 续传的 Content-Length 是「剩余字节」，不是整首
        buf.declare_len(400);
        assert_eq!(buf.declared(), Some(500));
        assert!(!buf.can_start_at(500), "水位 100 未过闸门 500，不该起播");
        buf.commit(500);
        assert!(buf.can_start_at(500), "水位到闸门即就绪");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_read_after_seek_starts_at_the_new_offset() {
        // 真实游标语义：seek 到已落盘区域内，下一次 read 从新位置开始
        let buf = buffer_with("cursor", "i.bin", b"0123456789", 10);
        let mut cur = StreamReader::open(Arc::clone(&buf)).unwrap();
        use std::io::Seek;
        cur.seek(SeekFrom::Start(4)).unwrap();
        let mut b = [0u8; 3];
        cur.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"456");
        cleanup(&buf);
    }
}
