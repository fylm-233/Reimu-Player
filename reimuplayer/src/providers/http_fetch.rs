//! HTTP 音频字节获取（阶段 C / M3）。
//!
//! [`crate::music::online_playback::AudioFetcher`] 的 HTTP 实现：
//! - 基于 `reqwest` 0.12（依赖树既有，rustls，免 OpenSSL），**不引入新编译单元**。
//! - `Response::chunk()` **流式写盘**，不整段进内存（规范 9.2）；攒够 1 MB
//!   才一次 `write_all`，写盘次数从「每个 TCP 分片一次」降到「每 MB 一次」。
//! - 超时按**字节还在不在动**判定（空闲超时），不再给整首设墙钟。旧值 60 s
//!   等价于「带宽低于 1.5 MB/s 必失败」，而 90 MB 母带档在慢网络上本来就要
//!   几分钟；失败后上层还是从 0 重下，用户看到的就是永远缓冲不完。
//! - 断流后带 `Range: bytes=<已落盘>-` **原地续传**，已到手的前缀不重下
//!   （实测 `m701.music.126.net` 返回 206 + 精确偏移，续传可用）。
//! - 体积上限 + 整体墙钟兜底（只掐「永远涓流」的病态连接）；失败清理半成品。
//! - CDN 签名 URL 失效（401/403/404）映射为
//!   [`ProviderError::PlaybackUrlUnavailable`]，交由上层冷恢复。
//!
//! 日志只记主机名，**不记完整签名 URL**（含鉴权查询串，属敏感信息，
//! 规范 3.2）。
//!
//! 续传偏移按**响应体原始字节**计算：`reqwest` 未编译 `gzip`/`brotli`/`zstd`
//! 特性，响应体不做透明解压，「文件里的第 N 字节」与「流里的第 N 字节」一一
//! 对应。哪天给 `reqwest` 开了透明解压，这里必须跟着改成按解码后偏移记账，
//! 否则续传会在文件中间留下错位的数据。

use std::io::SeekFrom;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use crate::music::online_playback::{AudioFetcher, MAX_AUDIO_BYTES};
use crate::music::provider::ProviderError;
use crate::music::stream_buffer::StreamBuffer;

/// 连接超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 空闲超时：连续这么久拿不到新字节（**含等响应头**）即判定断流，重连续传。
///
/// 15 s 比 TCP 常见的 ~13 s 重传窗口略宽，不至于把一次抖动误判成断流。
const IDLE_TIMEOUT: Duration = Duration::from_secs(15);

/// 整体墙钟兜底：只用于掐死「每次只漏几个字节、永远不空闲超时」的连接。
///
/// 8 分钟够 90 MB 母带档在 ~0.2 MB/s 下走完；真正的失败判定由
/// [`IDLE_TIMEOUT`] 负责，正常路径不该碰到这个值。
const TOTAL_TIMEOUT: Duration = Duration::from_secs(8 * 60);

/// 一次下载允许的重连次数（每次都保住已落盘的前缀）。
const RESUME_TRIES: u32 = 3;

/// 写盘缓冲：攒够这么多字节才一次 `write_all`。
///
/// 同时也是断流时**最多重下一小段**的上限——缓冲里的字节不算已落盘。
const WRITE_BUFFER_BYTES: usize = 1024 * 1024;

/// 无新字节时多久醒一次，问一句「还要不要继续」。
///
/// 边下边播的取消必须赶得上换台的手速，而 `Response::chunk()` 在慢网上可以
/// 好几秒不返回；拿一个比它快得多的定时器赛跑，代价只是每 200 ms 一次空转。
const CANCEL_POLL: Duration = Duration::from_millis(200);

/// HTTP 音频下载器。
pub struct HttpAudioFetcher {
    client: reqwest::Client,
    idle: Duration,
    total: Duration,
    resume_tries: u32,
    max_bytes: u64,
    buffer_bytes: usize,
}

impl HttpAudioFetcher {
    /// 创建下载器（构建 HTTP 客户端失败时报错，不 panic）。
    pub fn new() -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| ProviderError::Unknown(format!("HTTP 客户端构造失败: {}", e)))?;
        Ok(Self {
            client,
            idle: IDLE_TIMEOUT,
            total: TOTAL_TIMEOUT,
            resume_tries: RESUME_TRIES,
            max_bytes: MAX_AUDIO_BYTES,
            buffer_bytes: WRITE_BUFFER_BYTES,
        })
    }

    /// 覆盖空闲超时（测试或调优用）。
    pub fn with_idle_timeout(mut self, idle: Duration) -> Self {
        self.idle = idle;
        self
    }

    /// 覆盖整体墙钟兜底（测试或调优用）。
    pub fn with_total_timeout(mut self, total: Duration) -> Self {
        self.total = total;
        self
    }

    /// 覆盖断点重连次数（测试或调优用）。
    pub fn with_resume_tries(mut self, tries: u32) -> Self {
        self.resume_tries = tries;
        self
    }

    /// 覆盖写盘缓冲大小（测试或调优用）。
    pub fn with_write_buffer(mut self, buffer_bytes: usize) -> Self {
        self.buffer_bytes = buffer_bytes.max(1);
        self
    }

    /// 覆盖体积上限（测试或配置调优用）。
    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// HTTP 状态码 → 项目错误分类（CDN 语义）。
    fn map_status(status: u16) -> ProviderError {
        match status {
            // 签名 URL 失效 / 资源不在 → 「未获得播放地址」，可冷恢复
            401 | 403 | 404 => ProviderError::PlaybackUrlUnavailable,
            429 => ProviderError::RateLimited,
            500..=599 => ProviderError::DataProviderUnavailable,
            other => ProviderError::Unknown(format!("HTTP {}", other)),
        }
    }

    /// reqwest 错误 → 项目错误分类。
    fn map_reqwest_err(e: reqwest::Error) -> ProviderError {
        if e.is_timeout() {
            ProviderError::RequestTimeout
        } else if e.is_connect() || e.is_request() {
            ProviderError::NetworkUnavailable
        } else if e.is_decode() {
            ProviderError::InvalidData
        } else if let Some(status) = e.status() {
            Self::map_status(status.as_u16())
        } else {
            ProviderError::Unknown(e.to_string())
        }
    }

    /// 单轮流式拉取：从 `ledger.flushed` 处接下去（或从头）。
    ///
    /// 每轮都是**一次全新连接**；已落盘字节通过 `Range` 头接上，不重下。
    /// 落盘事实通过 `sink` 发布（边下边播要在每批字节落盘后推进水位）。
    async fn stream_once(
        &self,
        url: &str,
        host: &str,
        dest: &Path,
        ledger: &mut WriteLedger,
        sink: &dyn ByteSink,
    ) -> StreamOutcome {
        // 取消之后绝不再开一轮：上一轮结束后装载器可能已经把这个临时文件删了，
        // 再 `File::create` 就会凭空造出一个没人登记的孤儿文件。
        if sink.is_stopped() {
            return StreamOutcome::Stopped;
        }
        // 进到这里的缓冲必为空：每轮退出前都把缓冲刷进文件（或随失败丢弃）
        let offset = ledger.flushed;
        let mut req = self.client.get(url);
        if let Some(range) = range_header(offset) {
            req = req.header(reqwest::header::RANGE, range);
        }

        // 响应头也算「有没有在动」：只连上不吐头同样是断流，不该干等整体墙钟
        let mut resp = match tokio::time::timeout(self.idle, req.send()).await {
            Err(_elapsed) => {
                tracing::warn!("音频下载等响应头超时（>{:?}）host={}", self.idle, host);
                return break_outcome(offset, ProviderError::RequestTimeout);
            }
            Ok(Err(e)) => return break_outcome(offset, Self::map_reqwest_err(e)),
            Ok(Ok(r)) => r,
        };

        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            return StreamOutcome::Fatal(Self::map_status(status));
        }

        // 206 = 服务器按 Range 回了部分体，接在前缀之后；
        // 200（含「无视 Range 整首重发」）= 文件归零从头写。
        let append = writes_from_prefix(status, offset);
        let base = if append { offset } else { 0 };
        if let Err(e) = sink.on_head(&ResponseHead {
            requested_at: offset,
            write_base: base,
            body_len: resp.content_length(),
        }) {
            return StreamOutcome::Fatal(e);
        }
        let opened = if append {
            tokio::fs::OpenOptions::new().write(true).open(dest).await
        } else {
            ledger.flushed = 0;
            tokio::fs::File::create(dest).await
        };
        let mut file = match opened {
            Ok(f) => f,
            Err(e) => {
                return StreamOutcome::Fatal(ProviderError::Unknown(format!(
                    "临时文件创建失败: {}",
                    e
                )))
            }
        };
        // 写头对齐到已落盘偏移：上一轮中断可能在尾部留下没记账的多余字节
        if let Err(e) = file.seek(SeekFrom::Start(ledger.flushed)).await {
            return StreamOutcome::Fatal(ProviderError::Unknown(format!(
                "临时文件写入失败: {}",
                e
            )));
        }

        loop {
            // 取下一批字节。**取消与空闲超时在同一处裁决**：慢网里 `chunk()` 可能
            // 几秒不返回，而那正是用户已经换台的时刻——取消不能等下一个分片到来。
            let chunk = loop {
                tokio::select! {
                    biased;
                    got = tokio::time::timeout(self.idle, resp.chunk()) => match got {
                        Err(_elapsed) => {
                            // 断流：先把缓冲里的字节落盘，续传点就能多保住一整块
                            if let Err(e) = drain(&mut file, ledger).await {
                                return StreamOutcome::Fatal(e);
                            }
                            sink.on_flushed(ledger.flushed);
                            tracing::warn!(
                                "音频下载空闲无新字节（>{:?}）host={} 已落盘 {} B",
                                self.idle,
                                host,
                                ledger.flushed
                            );
                            return break_outcome(ledger.flushed, ProviderError::RequestTimeout);
                        }
                        Ok(Err(e)) => {
                            if let Err(err) = drain(&mut file, ledger).await {
                                return StreamOutcome::Fatal(err);
                            }
                            sink.on_flushed(ledger.flushed);
                            let mapped = Self::map_reqwest_err(e);
                            tracing::warn!(
                                "音频下载中断: host={} 已落盘 {} B — {}",
                                host,
                                ledger.flushed,
                                mapped
                            );
                            return break_outcome(ledger.flushed, mapped);
                        }
                        Ok(Ok(None)) => break None,
                        Ok(Ok(Some(bytes))) => break Some(bytes),
                    },
                    // 没有新字节也要按期醒一次，问记账者要不要停手
                    _ = tokio::time::sleep(CANCEL_POLL) => {
                        if sink.is_stopped() {
                            // 取消不比失败金贵：缓冲里的字节不写了，续传点就停在这里
                            return StreamOutcome::Stopped;
                        }
                    }
                }
            };
            let Some(chunk) = chunk else {
                break;
            };

            if ledger.received() + chunk.len() as u64 > self.max_bytes {
                return StreamOutcome::Fatal(ProviderError::Unknown(format!(
                    "音频超过大小上限（{} MB）",
                    self.max_bytes / (1024 * 1024)
                )));
            }
            ledger.push(&chunk);
            if ledger.needs_flush() {
                if let Err(e) = drain(&mut file, ledger).await {
                    return StreamOutcome::Fatal(e);
                }
                sink.on_flushed(ledger.flushed);
            }
        }

        if let Err(e) = drain(&mut file, ledger).await {
            return StreamOutcome::Fatal(e);
        }
        sink.on_flushed(ledger.flushed);
        // 文件大小严格等于返回的字节数：上一次中断留下的长尾巴不能留
        if let Err(e) = file.set_len(ledger.flushed).await {
            return StreamOutcome::Fatal(ProviderError::Unknown(format!(
                "临时文件收尾失败: {}",
                e
            )));
        }
        StreamOutcome::Complete
    }

    /// 下载主体：整体墙钟兜底 + 断流原地续传。
    ///
    /// 失败时**不**动文件——半成品归谁处置由调用方决定（整首下载直接删；
    /// 边下边播的文件正被读方打开，删除属于装载器的职责）。
    async fn download(
        &self,
        url: &str,
        dest: &Path,
        ledger: &mut WriteLedger,
        sink: &dyn ByteSink,
    ) -> Result<u64, ProviderError> {
        // 只记主机名；完整签名 URL 不入日志（敏感信息约束）
        let host = host_of(url);
        let download = async {
            let mut tries = 0u32;
            loop {
                match self.stream_once(url, host, dest, ledger, sink).await {
                    StreamOutcome::Complete => return Ok(ledger.flushed),
                    StreamOutcome::Stopped => return Err(ProviderError::Cancelled),
                    StreamOutcome::Fatal(e) => return Err(e),
                    StreamOutcome::Resumable(e) => {
                        tries += 1;
                        if tries > self.resume_tries {
                            tracing::warn!(
                                "音频续传次数用尽（host={}，最后停在 {} B）: {}",
                                host,
                                ledger.flushed,
                                e
                            );
                            return Err(e);
                        }
                        tracing::warn!(
                            "从 {} B 处 Range 续传（第 {}/{} 次）host={}",
                            ledger.flushed,
                            tries,
                            self.resume_tries,
                            host
                        );
                    }
                }
            }
        };

        match tokio::time::timeout(self.total, download).await {
            Ok(result) => result,
            Err(_elapsed) => {
                tracing::warn!(
                    "音频下载超过整体上限（>{:?}）host={}，放弃续传",
                    self.total,
                    host
                );
                Err(ProviderError::RequestTimeout)
            }
        }
    }
}

impl AudioFetcher for HttpAudioFetcher {
    async fn fetch_to_file(&self, url: &str, dest: &Path) -> Result<u64, ProviderError> {
        tracing::debug!(target: "provider", "下载音频: host={} -> {}", host_of(url), dest.display());
        let mut ledger = WriteLedger::new(self.buffer_bytes);
        let result = self.download(url, dest, &mut ledger, &WholeFile).await;
        if result.is_err() {
            // 彻底失败：半成品不属于任何曲目，删掉（上层 prepare_once 也兜底）
            let _ = std::fs::remove_file(dest);
        }
        result
    }

    async fn stream_to_buffer(
        &self,
        url: &str,
        buf: &Arc<StreamBuffer>,
    ) -> Result<u64, ProviderError> {
        tracing::debug!(
            target: "provider",
            "流式下载音频: host={} -> {}（从 {} B 接下去）",
            host_of(url),
            buf.path().display(),
            buf.written()
        );
        let mut ledger = WriteLedger::new(self.buffer_bytes);
        // 起点取缓冲当前水位：签名 URL 失效后换新地址重进本方法，就是原地续传
        // 而不是从 0 重下（`declare_len` 只认第一次声明，总长不会被 206 的剩余长度带偏）。
        ledger.flushed = buf.written();
        let result = self
            .download(url, buf.path(), &mut ledger, &Streaming { buf })
            .await;
        if result.is_ok() {
            // 成功即终局：读方越过水位看到的从此是真 EOF，不再需要任何等待
            buf.mark_done();
        }
        result
    }
}

/// 落盘过程的记账去向。
///
/// 同一套「请求 → 响应头 → 逐批落盘 → 断流续传」的机器要服务两种需求：
/// - 整首下载：只要最终字节数，中间状态没人关心；
/// - 边下边播：解码器正在另一条线程读同一个文件，**每一步事实都必须立刻发布**
///   （总长、水位），否则读方会等到天荒地老或读到假 EOF。
///
/// 把差异收进这一个接口，`stream_once` 就能保持一条线性流程——两个分支各写一遍
/// 续传逻辑是这个模块最容易埋 bug 的地方。
///
/// `Sync` 是硬要求：`&dyn ByteSink` 要能穿过 `impl Future + Send` 的边界。
trait ByteSink: Sync {
    /// 响应头到手（在打开文件之前）。返回 `Err` = 这轮响应不可接受，立刻断线。
    ///
    /// 默认「什么都接受」——整首下载路径正是如此。
    fn on_head(&self, head: &ResponseHead) -> Result<(), ProviderError> {
        let _ = head;
        Ok(())
    }

    /// 已有这么多字节**真的**在文件里（续传点）。
    fn on_flushed(&self, _written: u64) {}

    /// 每个分片之前问一次：记账者要求停手吗？
    ///
    /// 边下边播用它承接换台取消——不能等整首下完才响应，那正是用户等不起的。
    fn is_stopped(&self) -> bool {
        false
    }
}

/// [`ByteSink::on_head`] 携带的事实。
struct ResponseHead {
    /// 本轮请求带出去的 Range 偏移；0 = 整首请求。
    requested_at: u64,
    /// 本轮响应体将写到文件的哪个偏移：206 续写 = `requested_at`，200 = 0。
    write_base: u64,
    /// 响应体字节数（`Content-Length`）；`None` = chunked / 未知。
    body_len: Option<u64>,
}

/// 整首下载：无中间记账。
struct WholeFile;

impl ByteSink for WholeFile {}

/// 边下边播：把 HTTP 侧的事实翻译成 [`StreamBuffer`] 的水位与阶段。
struct Streaming<'a> {
    buf: &'a StreamBuffer,
}

impl ByteSink for Streaming<'_> {
    fn on_head(&self, head: &ResponseHead) -> Result<(), ProviderError> {
        // 半路重连却拿到整首响应 = 服务器无视 Range，要清空文件从头再来。
        // 而读方的游标还在后面，清空会让它立刻撞上假 EOF——判不可续传，
        // 交上层重开一轮装载（实测网易云 CDN 按 Range 回 206，这里只是防线）。
        if head.requested_at > 0 && head.write_base == 0 {
            return Err(ProviderError::UnsupportedResponse);
        }
        match head.body_len {
            // 只有整首响应（write_base 起算的那份长度）才是总长；
            // 206 的 Content-Length 是剩余字节，`write_base + len` 恰好等于总长，
            // 而 [`StreamBuffer::declare_len`] 只认第一次声明，重复值会被忽略。
            Some(len) => self.buf.declare_len(head.write_base + len),
            // 总长未知 → 解码器无法定位尾部、进度条也没有分母：
            // 立刻断线，别把带宽浪费在播不了的内容上，由上层回退整首下载。
            None => {
                self.buf.mark_length_unknown();
                return Err(ProviderError::UnsupportedResponse);
            }
        }
        Ok(())
    }

    fn on_flushed(&self, written: u64) {
        self.buf.commit(written);
    }

    fn is_stopped(&self) -> bool {
        self.buf.is_aborted()
    }
}

/// 单轮流式拉取的结局。
enum StreamOutcome {
    /// 全部字节已落盘。
    Complete,
    /// 断流，但已落盘的字节是有效前缀，值得带 `Range` 重连接着下。
    /// 携带的错误只在续传次数用尽时交出去。
    Resumable(ProviderError),
    /// 记账者要求停手（边下边播被换台取消）。
    Stopped,
    /// 不可续传的失败（签名失效、超限、写盘失败……），错误交上层分类处理。
    Fatal(ProviderError),
}

/// 落盘字节账本。
///
/// `flushed` **只统计已经写进文件的字节**：缓冲里那几 KB 一旦断流就重新取回，
/// 宁可重下一小段，也不能让续传偏移越过还没落盘的数据、在文件里留下空洞。
struct WriteLedger {
    buf: Vec<u8>,
    buffer_bytes: usize,
    flushed: u64,
}

impl WriteLedger {
    fn new(buffer_bytes: usize) -> Self {
        Self {
            buf: Vec::with_capacity(buffer_bytes),
            buffer_bytes,
            flushed: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    fn needs_flush(&self) -> bool {
        self.buf.len() >= self.buffer_bytes
    }

    /// 已到手的全部字节（含还在缓冲里的）。体积上限按它算。
    fn received(&self) -> u64 {
        self.flushed + self.buf.len() as u64
    }

    fn pending(&self) -> &[u8] {
        &self.buf
    }

    /// 刷盘成功：这些字节从此就是续传点的一部分。
    fn commit(&mut self, written: usize) {
        self.flushed += written as u64;
        self.buf.clear();
    }

    /// 刷盘失败：字节作废（偏移不推进，重连时 seek 回来覆盖脏尾巴）。
    fn discard(&mut self) {
        self.buf.clear();
    }
}

/// 把缓冲里的字节写进文件；成功才推进续传点。
async fn drain(file: &mut tokio::fs::File, ledger: &mut WriteLedger) -> Result<(), ProviderError> {
    if ledger.pending().is_empty() {
        return Ok(());
    }
    let written = ledger.pending().len();
    match file.write_all(ledger.pending()).await {
        Ok(()) => {
            ledger.commit(written);
            Ok(())
        }
        Err(e) => {
            ledger.discard();
            Err(ProviderError::Unknown(format!("临时文件写入失败: {}", e)))
        }
    }
}

/// 续传请求头；偏移 0 不发（让 CDN 走最常见的整首路径）。
fn range_header(offset: u64) -> Option<String> {
    (offset > 0).then(|| format!("bytes={offset}-"))
}

/// 这次响应体是接在前缀之后，还是从文件头重写。
///
/// 只有**我们真发过 Range**（`offset > 0`）且服务器按 206 回了部分体时才是续写。
fn writes_from_prefix(status: u16, offset: u64) -> bool {
    status == 206 && offset > 0
}

/// 断流后果判定：有已落盘前缀才值得自己重连续传；一个字节都没存下时直接把
/// 原始错误交上层——那里已有指数退避的整首重试，不该在这里再叠一层。
fn break_outcome(flushed: u64, mapped: ProviderError) -> StreamOutcome {
    if flushed == 0 {
        StreamOutcome::Fatal(mapped)
    } else {
        StreamOutcome::Resumable(mapped)
    }
}

/// 从 URL 取主机名（日志唯一允许出现的地址片段）。
fn host_of(url: &str) -> &str {
    url.split('/').nth(2).unwrap_or("(未知主机)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::music::stream_buffer::{BufferPhase, StreamBuffer, StreamReader};
    use crate::providers::test_support::{
        pattern, requested_offset, spawn_server, test_dir, Script,
    };
    use std::io::Read;

    /// 服务器端口拼成 URL。
    fn url(port: u16) -> String {
        format!("http://127.0.0.1:{port}/a.flac")
    }

    #[test]
    fn map_status_classifies_cdn_semantics() {
        // 签名 URL 失效类 → 可冷恢复的「未获得播放地址」
        for code in [401, 403, 404] {
            assert_eq!(
                HttpAudioFetcher::map_status(code),
                ProviderError::PlaybackUrlUnavailable
            );
        }
        assert_eq!(
            HttpAudioFetcher::map_status(429),
            ProviderError::RateLimited
        );
        for code in [500, 502, 503] {
            assert_eq!(
                HttpAudioFetcher::map_status(code),
                ProviderError::DataProviderUnavailable
            );
        }
        assert!(matches!(
            HttpAudioFetcher::map_status(418),
            ProviderError::Unknown(_)
        ));
    }

    #[test]
    fn new_builds_client_without_panic() {
        // 构造失败应报错而非 panic（区别于 SDK 的 expect 行为）
        assert!(HttpAudioFetcher::new().is_ok());
    }

    #[test]
    fn range_header_only_for_nonzero_offset() {
        assert_eq!(range_header(0), None);
        assert_eq!(range_header(1), Some("bytes=1-".to_string()));
        assert_eq!(
            range_header(120_000),
            Some("bytes=120000-".to_string()),
            "续传点按已落盘字节算，开区间到末尾"
        );
    }

    #[test]
    fn only_206_with_a_prefix_appends() {
        assert!(writes_from_prefix(206, 1024));
        // 200 是整首重发：必须从 0 写，否则前缀会被重复一遍
        assert!(!writes_from_prefix(200, 1024));
        // 没发过 Range 就谈不上续写（此时文件还没建）
        assert!(!writes_from_prefix(206, 0));
        assert!(!writes_from_prefix(200, 0));
    }

    #[test]
    fn nothing_durable_means_no_self_retry() {
        // 一个字节都没存下 → 交上层整首重试（它已有退避）
        assert!(matches!(
            break_outcome(0, ProviderError::NetworkUnavailable),
            StreamOutcome::Fatal(ProviderError::NetworkUnavailable)
        ));
        // 存下了前缀 → 自己重连续传，别把已到手的部分丢掉
        assert!(matches!(
            break_outcome(4096, ProviderError::RequestTimeout),
            StreamOutcome::Resumable(ProviderError::RequestTimeout)
        ));
    }

    #[test]
    fn ledger_counts_only_committed_bytes_as_resumable() {
        let mut ledger = WriteLedger::new(8);
        ledger.push(&[0u8; 5]);
        // 缓冲里 5 字节：收到了，但**还不能当作续传点**
        assert_eq!(ledger.received(), 5);
        assert_eq!(ledger.flushed, 0);
        assert!(!ledger.needs_flush(), "未攒满不该刷盘");

        ledger.push(&[0u8; 3]);
        assert!(ledger.needs_flush(), "攒满即刷");
        assert_eq!(ledger.pending().len(), 8);
        ledger.commit(ledger.pending().len());
        assert_eq!(ledger.flushed, 8, "只有写成功才推进续传点");
        assert!(ledger.pending().is_empty());
        assert_eq!(ledger.received(), 8);

        // 刷盘失败时 discard：偏移停在原地，重连会 seek 回来覆盖那截脏尾巴
        let mut l2 = WriteLedger::new(4);
        l2.push(b"abcd");
        l2.discard();
        assert_eq!(l2.flushed, 0);
        assert_eq!(l2.received(), 0, "没写进文件的字节等于没收到");
    }

    #[tokio::test]
    async fn fetch_rejects_empty_url_without_network() {
        let f = HttpAudioFetcher::new().unwrap();
        let dir = test_dir("empty_url");
        let dest = dir.join("x.audio");
        // 空 URL：reqwest 直接报构造/请求错误 → 网络类分类，不 panic
        let err = f.fetch_to_file("", &dest).await.unwrap_err();
        assert!(matches!(
            err,
            ProviderError::NetworkUnavailable | ProviderError::Unknown(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn full_download_writes_exact_bytes() {
        let body = pattern(200_000);
        let (port, seen) = spawn_server(
            body.clone(),
            vec![Script::Full],
            0,
            Duration::from_millis(0),
        );
        let dir = test_dir("full");
        let dest = dir.join("full.flac");
        let f = HttpAudioFetcher::new()
            .unwrap()
            .with_write_buffer(16 * 1024);
        let n = f
            .fetch_to_file(&url(port), &dest)
            .await
            .expect("整首下载应成功");
        assert_eq!(n as usize, body.len(), "返回值=落盘字节数");
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        let reqs = seen.lock().unwrap();
        assert_eq!(reqs.len(), 1, "一次到位不该重连");
        assert!(requested_offset(&reqs[0]).is_none(), "首请求不该带 Range");
        drop(reqs);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resumes_a_broken_download_with_range() {
        let body = pattern(300_000);
        let cut = 120_000;
        let (port, seen) = spawn_server(
            body.clone(),
            vec![Script::Truncate, Script::Partial],
            cut,
            Duration::from_millis(0),
        );
        let dir = test_dir("resume");
        let dest = dir.join("resume.flac");
        let f = HttpAudioFetcher::new()
            .unwrap()
            // 小缓冲：让「断流前先把缓冲落盘」这一步可观察（续传点 = 已到手字节）
            .with_write_buffer(16 * 1024);
        let n = f
            .fetch_to_file(&url(port), &dest)
            .await
            .expect("断流应被 Range 续传救回来");
        assert_eq!(n as usize, body.len());
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            body,
            "拼接结果必须逐字节等于原文：续传偏移错一位都会在这里暴露"
        );
        let reqs = seen.lock().unwrap();
        assert_eq!(reqs.len(), 2, "断一次 → 重连一次");
        assert_eq!(
            requested_offset(&reqs[1]),
            Some(cut as u64),
            "续传点应是已落盘字节数（含断流时刷出的缓冲）"
        );
        drop(reqs);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn restarts_from_zero_when_server_ignores_range() {
        let body = pattern(250_000);
        // 第二条连接无视 Range，按 200 重发整首
        let (port, seen) = spawn_server(
            body.clone(),
            vec![Script::Truncate, Script::Full],
            100_000,
            Duration::from_millis(0),
        );
        let dir = test_dir("ignore_range");
        let dest = dir.join("ir.flac");
        let f = HttpAudioFetcher::new()
            .unwrap()
            .with_write_buffer(16 * 1024);
        let n = f
            .fetch_to_file(&url(port), &dest)
            .await
            .expect("服务器忽略 Range 时整首重发，仍应得到完好的文件");
        assert_eq!(n as usize, body.len());
        // 若把整首重发误接在前缀之后，文件会多出 100 KB 的重复段
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            body,
            "归零重写，不保留旧前缀"
        );
        let reqs = seen.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(requested_offset(&reqs[1]), Some(100_000));
        drop(reqs);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn idle_timeout_resumes_instead_of_waiting_for_the_wall_clock() {
        // 服务器吐 1 KiB 后不再发字节：空闲超时判为断流并重连，而不是干等墙钟
        let body = pattern(100_000);
        let (port, _seen) = spawn_server(
            body,
            vec![Script::Stall, Script::Mute],
            1024,
            Duration::from_millis(900),
        );
        let dir = test_dir("idle");
        let dest = dir.join("idle.flac");
        let f = HttpAudioFetcher::new()
            .unwrap()
            .with_idle_timeout(Duration::from_millis(150))
            .with_total_timeout(Duration::from_secs(30))
            .with_resume_tries(1)
            .with_write_buffer(1024);
        let started = std::time::Instant::now();
        let err = f.fetch_to_file(&url(port), &dest).await.unwrap_err();
        assert_eq!(err, ProviderError::RequestTimeout, "空闲超时的分类");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "靠空闲超时收尾（{:?}），不该被整体墙钟拖住",
            started.elapsed()
        );
        assert!(!dest.exists(), "彻底失败要清理半成品");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn silent_headers_fail_at_offset_zero_without_retrying() {
        // 一个字节都没存下 → 不在这里叠第二层重试，直接把错误交上层
        let body = pattern(4096);
        let (port, seen) = spawn_server(body, vec![Script::Mute], 0, Duration::from_millis(600));
        let dir = test_dir("mute");
        let dest = dir.join("mute.flac");
        let f = HttpAudioFetcher::new()
            .unwrap()
            .with_idle_timeout(Duration::from_millis(120));
        let err = f.fetch_to_file(&url(port), &dest).await.unwrap_err();
        assert_eq!(err, ProviderError::RequestTimeout);
        assert!(!dest.exists());
        assert_eq!(seen.lock().unwrap().len(), 1, "没存下字节就不该重连");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn over_limit_aborts_and_removes_the_partial() {
        let body = pattern(100_000);
        let (port, _seen) = spawn_server(body, vec![Script::Full], 0, Duration::from_millis(0));
        let dir = test_dir("limit");
        let dest = dir.join("big.flac");
        let f = HttpAudioFetcher::new()
            .unwrap()
            .with_max_bytes(32 * 1024)
            .with_write_buffer(4 * 1024);
        let err = f.fetch_to_file(&url(port), &dest).await.unwrap_err();
        assert!(
            matches!(&err, ProviderError::Unknown(s) if s.contains("超过大小上限")),
            "超限应是人话错误: {err}"
        );
        assert!(!dest.exists(), "超限的半成品不留");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn expired_signed_url_maps_to_cold_recovery_class() {
        // 403 只连一次：分类成「未获得播放地址」，且不得误当断流重连
        let body = pattern(4096);
        let (port, seen) = spawn_server(
            body,
            vec![Script::Denied, Script::Full],
            0,
            Duration::from_millis(0),
        );
        let dir = test_dir("403");
        let dest = dir.join("x.flac");
        let f = HttpAudioFetcher::new().unwrap();
        let err = f.fetch_to_file(&url(port), &dest).await.unwrap_err();
        assert_eq!(
            err,
            ProviderError::PlaybackUrlUnavailable,
            "签名失效类必须可被上层冷恢复识别"
        );
        assert!(!dest.exists());
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "签名失效不是断流，不该自己重连消耗续传次数"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 边下边播：水位、取消、拒绝不可流式的响应 ----

    #[tokio::test]
    async fn streaming_publishes_the_watermark_before_the_file_is_complete() {
        // 服务器吐 60 KB、按住 500 ms、再吐完剩下的：按住的那段时间正是
        // 「解码器已经能开始读」的窗口——水位必须已经到位，且不许假 EOF。
        let body = pattern(200_000);
        let (port, _seen) = spawn_server(
            body.clone(),
            vec![Script::SlowSplit],
            60_000,
            Duration::from_millis(500),
        );
        let dir = test_dir("stream_progress");
        let buf = StreamBuffer::new(dir.join("p.flac"));
        let watcher = Arc::clone(&buf);
        let f = Arc::new(
            HttpAudioFetcher::new()
                .unwrap()
                .with_write_buffer(16 * 1024),
        );
        let target = url(port);
        let task = tokio::spawn(async move { f.stream_to_buffer(&target, &buf).await });

        // 攒满一整个写盘缓冲才落盘，所以「按住」期间的水位是缓冲大小的整数倍：
        // 32 KB 这个门槛既能证明进度已发布，又远小于整首长度。
        let mut plateau = 0u64;
        for _ in 0..100 {
            if watcher.written() >= 32_000 {
                plateau = watcher.written();
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(plateau > 0, "前半段应在下载结束前就落盘并公布水位");
        assert!(
            plateau < body.len() as u64,
            "水位停在 {}，说明测的确实是中途而不是终态",
            plateau
        );
        assert!(!task.is_finished(), "此时服务器还按着后半段");
        assert_eq!(watcher.phase(), BufferPhase::Growing);
        assert_eq!(
            watcher.declared(),
            Some(body.len() as u64),
            "总长必须在响应头到手时就声明"
        );

        // 关键证据：**下载没结束，读方已经能按偏移读到正确字节**
        let mut head = vec![0u8; plateau as usize];
        StreamReader::open(Arc::clone(&watcher))
            .unwrap()
            .read_exact(&mut head)
            .expect("水位以内的读取不该等、也不该报 EOF");
        assert_eq!(head, body[..plateau as usize], "读到的是原文前缀");

        let n = task.await.expect("任务不该 panic").expect("流式下载应完成");
        assert_eq!(n as usize, body.len());
        assert_eq!(watcher.phase(), BufferPhase::Done);
        assert_eq!(std::fs::read(watcher.path()).unwrap(), body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn aborting_a_stream_stops_on_the_next_poll_not_the_next_chunk() {
        // 服务器发 1 KiB 后按住连接 5 s 不再吐字节：如果取消只能在下个分片到来
        // 时才被发现，换台就要等整整 5 s——那正是用户能感觉到的卡顿。
        let body = pattern(200_000);
        let (port, _seen) = spawn_server(body, vec![Script::Stall], 1024, Duration::from_secs(5));
        let dir = test_dir("stream_abort");
        let buf = StreamBuffer::new(dir.join("a.flac"));
        let watcher = Arc::clone(&buf);
        let f = Arc::new(HttpAudioFetcher::new().unwrap().with_write_buffer(1024));
        let target = url(port);
        let task = tokio::spawn(async move { f.stream_to_buffer(&target, &buf).await });

        for _ in 0..200 {
            if watcher.written() >= 1024 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            watcher.written() >= 1024,
            "先确认确实下到了字节，否则这个测试什么都没测"
        );

        let started = std::time::Instant::now();
        watcher.abort();
        let err = task.await.expect("任务不该 panic").unwrap_err();
        assert_eq!(err, ProviderError::Cancelled, "取消要区别于失败");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "取消靠轮询定时器生效（实际 {:?}），不该等服务器把那 5 s 熬完",
            started.elapsed()
        );
        assert_eq!(watcher.phase(), BufferPhase::Aborted);
        assert!(
            watcher.path().exists(),
            "取消不许删文件：读方可能还开着它，处置权在装载器"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_new_stream_round_continues_at_the_buffer_watermark() {
        // 签名 URL 失效后换新地址重进本方法：起点是水位，不是 0。
        let body = pattern(300_000);
        let prefix = 120_000;
        let (port, seen) = spawn_server(
            body.clone(),
            vec![Script::Partial],
            0,
            Duration::from_millis(0),
        );
        let dir = test_dir("stream_resume");
        let path = dir.join("r.flac");
        std::fs::write(&path, &body[..prefix]).unwrap();
        let buf = StreamBuffer::new(path.clone());
        buf.declare_len(body.len() as u64);
        buf.commit(prefix as u64);

        let f = HttpAudioFetcher::new()
            .unwrap()
            .with_write_buffer(16 * 1024);
        let n = f
            .stream_to_buffer(&url(port), &buf)
            .await
            .expect("应从当前水位接下去");
        assert_eq!(
            n as usize,
            body.len(),
            "返回值是整首落盘字节数，不是本轮新增"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            body,
            "前缀 + 续传段必须逐字节拼回原文"
        );
        assert_eq!(
            requested_offset(&seen.lock().unwrap()[0]),
            Some(prefix as u64),
            "重进的请求必须带水位处的 Range"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_response_without_a_declared_length_is_refused_before_it_is_written() {
        // 没有 Content-Length（chunked）= 总长未知：解码器无法定位尾部，
        // 进度条没有分母。当场拒绝并留下 LengthUnknown 让上层回退整首，
        // 而不是把带宽花在一首播不利索的曲子上。
        let body = pattern(100_000);
        let (port, _seen) = spawn_server(body, vec![Script::Chunked], 0, Duration::from_millis(0));
        let dir = test_dir("stream_chunked");
        let buf = StreamBuffer::new(dir.join("c.flac"));
        let f = HttpAudioFetcher::new().unwrap();
        let err = f.stream_to_buffer(&url(port), &buf).await.unwrap_err();
        assert_eq!(err, ProviderError::UnsupportedResponse);
        assert_eq!(
            buf.phase(),
            BufferPhase::LengthUnknown,
            "上层靠这个阶段决定回退整首"
        );
        assert!(
            !buf.path().exists(),
            "判定不可流式之前不许创建文件，更不许写一个字节"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_server_that_ignores_range_must_not_truncate_a_live_file() {
        // 整首下载遇到「无视 Range」应当归零重写；边下边播却**绝不能**：
        // 读方的游标还在文件后面，清空会让它立刻撞上假 EOF。
        let body = pattern(300_000);
        let prefix = 100_000;
        let (port, seen) = spawn_server(
            body.clone(),
            vec![Script::Full],
            0,
            Duration::from_millis(0),
        );
        let dir = test_dir("stream_ignore_range");
        let path = dir.join("ir.flac");
        std::fs::write(&path, &body[..prefix]).unwrap();
        let buf = StreamBuffer::new(path.clone());
        buf.declare_len(body.len() as u64);
        buf.commit(prefix as u64);

        let f = HttpAudioFetcher::new().unwrap();
        let err = f.stream_to_buffer(&url(port), &buf).await.unwrap_err();
        assert_eq!(
            err,
            ProviderError::UnsupportedResponse,
            "无视 Range 时判不可续传，交上层重开"
        );
        assert_eq!(
            std::fs::read(&path).unwrap().len(),
            prefix,
            "文件一个字节都不能被动过"
        );
        assert!(
            !buf.phase().is_terminal(),
            "拒一轮不等于终局：Done/Failed 由传输任务落定"
        );
        assert_eq!(
            requested_offset(&seen.lock().unwrap()[0]),
            Some(prefix as u64)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
