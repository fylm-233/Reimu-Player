//! 边下边播的传输任务（方案 2A 的写侧）。
//!
//! 一句话：**下载必须活得比「装载完成」更久，所以它不能住在装载线程里**。
//!
//! [`crate::gui::bridge`] 的装载线程用 `current_thread` 运行器，只在处理一次
//! 请求期间 `block_on`，空闲时阻塞在 `recv()` 上——那时运行器不再被驱动。
//! 于是「把下载 `spawn` 到装载线程的运行器上」会在播放开始后彻底停摆：
//! 水位不再推进、读方等到耐心耗尽、用户看到永远转不完的缓冲圈。
//! 本模块的任务因此**自带线程 + 自带运行器**，与装载线程的生命周期脱钩。
//!
//! 分层（`NETEASE_INTEGRATION_SPEC.md` 3.2）：本模块只认
//! [`DynAudioFetcher`] 与 [`StreamBuffer`] 两个接口，不认 reqwest/rodio；
//! 「怎么换新地址」也不归它管——那需要 Provider，由装载器以
//! [`UrlRefresher`] 的形式注入一条回调。
//!
//! 缓冲状态由谁落定（读方据此区分「缓冲中」与「播完了」）：
//!
//! | 状态 | 谁写 |
//! |---|---|
//! | `Done` | fetcher（`stream_to_buffer` 返回 `Ok` 的契约） |
//! | `LengthUnknown` | fetcher（响应没声明 `Content-Length`，交上层回退整首） |
//! | `Aborted` | [`StreamTask::abort`]（换台/停止） |
//! | `Failed` | 本任务（认输时唯一的一个落点，携带人话原因） |
//!
//! 边界约束：
//! - 播放 URL 只在内存里流动，绝不落盘、绝不入日志（规范 9.1/3.2）；
//!   日志里出现的地址片段只有主机名，且由 `providers::http_fetch` 负责。
//! - 网络 I/O 全部在这条 worker 线程上（`DESIGN_SPEC.md` 15.2），
//!   与音频回调、UI 线程无关。

use std::sync::Arc;

use crate::music::online_playback::DynAudioFetcher;
use crate::music::provider::{BoxFuture, ProviderError};
use crate::music::stream_buffer::{BufferPhase, StreamBuffer};

/// 重新取一次播放地址（旧的签名过期时调用）。
///
/// 返回**全新的 URL**。装载器持有 Provider，所以这条回调由装载器构造并交进来；
/// 传输任务自己不认识 Provider，也不该认识。
pub type UrlRefresher =
    Box<dyn FnMut() -> BoxFuture<'static, Result<String, ProviderError>> + Send>;

/// 一次播放允许换地址重取的次数。
///
/// 网易云签名地址实测 TTL = 1200 s（20 分钟）；3 次重取就是 1 小时的下行窗口，
/// 足够 90 MB 母带档在 ~0.3 MB/s 的慢网上走完。上限存在的意义是**杜绝死循环**：
/// 拿回同一个地址、或每次都失败时，必须有个地方认输并给用户清晰报错。
const MAX_URL_REFRESHES: u32 = 3;

/// 一条在跑的流式下载。
///
/// 句柄被 UI/引擎持有的部分只用来**查询水位**与**取消**（[`Self::abort`]）；
/// 字节什么时候真的落盘，由任务线程说了算。
#[derive(Debug)]
pub struct StreamTask {
    buffer: Arc<StreamBuffer>,
    joiner: Option<std::thread::JoinHandle<()>>,
}

impl StreamTask {
    /// 起任务：`buffer` 对应的文件由写线程逐步创建、逐批追加。
    ///
    /// `refresher` 传 `None` 表示「地址不会过期，过期就认输」——预取这类
    /// 不值得为一条没人听的曲子反复取址的场合用它。
    pub fn spawn(
        fetcher: Arc<dyn DynAudioFetcher>,
        url: String,
        buffer: Arc<StreamBuffer>,
        refresher: Option<UrlRefresher>,
    ) -> Self {
        let started = std::thread::Builder::new()
            .name("reimu-stream".into())
            .spawn({
                let buffer = Arc::clone(&buffer);
                move || run(fetcher, url, buffer, refresher)
            });
        match started {
            Ok(joiner) => Self {
                buffer,
                joiner: Some(joiner),
            },
            // 起不了线程（系统资源耗尽）：立刻把终局告诉读方，别让它干等 45 s
            Err(e) => {
                tracing::error!("流式下载线程创建失败: {e}");
                buffer.mark_failed(format!("无法启动下载线程: {e}"));
                Self {
                    buffer,
                    joiner: None,
                }
            }
        }
    }

    /// 缓冲句柄（引擎侧用它做闸门判断、取消与状态展示）。
    pub fn buffer(&self) -> &Arc<StreamBuffer> {
        &self.buffer
    }

    /// 要求停手：写线程在下一个轮询边界退出，读线程立刻从等待中醒来。
    ///
    /// 幂等，且不等待——换台发生在 UI 线程，绝不能被一条网络连接的收尾拖住。
    pub fn abort(&self) {
        self.buffer.abort();
    }

    /// 取消并等线程退出，返回最终阶段。
    ///
    /// 只用于测试与进程退出路径：需要「线程确实退了」这个确定性时才值得阻塞。
    pub fn join(mut self) -> BufferPhase {
        self.buffer.abort();
        if let Some(handle) = self.joiner.take() {
            // 线程体内不 panic（失败一律走 BufferPhase）；真被 panic 打断了，
            // 缓冲会停在 Growing，读方按自己的超时收场，这里不二次报错。
            let _ = handle.join();
        }
        self.buffer.phase()
    }
}

impl Drop for StreamTask {
    /// 句柄没了 = 没人再需要这些字节（换台/停止/引擎析构）。
    ///
    /// 只请求停止、不等待：线程在下一个轮询边界看到 `Aborted` 自己退出，
    /// 已经落盘的字节交给文件所有者（装载器）清扫。
    fn drop(&mut self) {
        self.buffer.abort();
    }
}

/// 线程主体：自带 `current_thread` 运行器，跑到完 / 认输 / 被取消为止。
fn run(
    fetcher: Arc<dyn DynAudioFetcher>,
    mut url: String,
    buffer: Arc<StreamBuffer>,
    mut refresher: Option<UrlRefresher>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("无法创建流式下载运行时: {e}");
            buffer.mark_failed(format!("无法创建下载运行时: {e}"));
            return;
        }
    };

    runtime.block_on(async move {
        let mut refreshes = 0u32;
        loop {
            // 每一轮都从缓冲当前水位接下去（`stream_to_buffer` 自己读水位），
            // 所以「换新地址重来」天然就是原地续传，不是从头重下。
            match fetcher.stream_to_buffer_boxed(&url, &buffer).await {
                // Done 已由 fetcher 标记（见 AudioFetcher::stream_to_buffer 契约）
                Ok(bytes) => {
                    tracing::debug!("流式下载完成: {bytes} B");
                    return;
                }
                Err(e) => {
                    // 取消不是失败：状态早被 `abort()` 定成 Aborted，无需再写
                    if buffer.is_aborted() {
                        return;
                    }
                    if let Some(next) =
                        retry_with_new_url(&mut refresher, &mut url, &e, refreshes).await
                    {
                        refreshes = next;
                        continue;
                    }
                    tracing::warn!("流式下载终止（已落盘 {} B）: {e}", buffer.written());
                    buffer.mark_failed(e.to_string());
                    return;
                }
            }
        }
    });
}

/// 这一轮失败是否值得**换新地址**再续一段。
///
/// 只认「签名地址失效」这一类（[`ProviderError::PlaybackUrlUnavailable`]）：
/// 其余失败（不可播、需登录、内容不对、超限）换地址也没用。
/// 拿到不同的新地址时改写 `url` 并返回 `Some(已用次数)`。
async fn retry_with_new_url(
    refresher: &mut Option<UrlRefresher>,
    url: &mut String,
    e: &ProviderError,
    used: u32,
) -> Option<u32> {
    if !matches!(e, ProviderError::PlaybackUrlUnavailable) || used >= MAX_URL_REFRESHES {
        return None;
    }
    let refresh = refresher.as_mut()?;
    let fresh = refresh().await.ok()?;
    // 拿回同一个地址 = 服务端没给新签名，再来一轮只是原地打转
    if fresh.is_empty() || fresh == *url {
        tracing::warn!("重新取址拿回同一个播放地址，不再续传");
        return None;
    }
    *url = fresh;
    tracing::info!(
        "播放地址已失效，换新地址从当前水位续传（第 {} 次重取）",
        used + 1
    );
    Some(used + 1)
}

#[cfg(test)]
mod tests {
    // 测试直接压在真的 [`HttpAudioFetcher`] + 本地 HTTP 服务器上：本层的价值
    // 恰恰是「水位/终局/续传」与写盘方**合起来**对不对，用假 fetcher 只会验到
    // 自己写的断言。依赖方向仍守住——`music` 生产代码不引用 `providers`。
    use super::*;
    use crate::music::stream_buffer::StreamReader;
    use crate::providers::test_support::{
        pattern, requested_offset, spawn_server, test_dir, Script,
    };
    use crate::providers::HttpAudioFetcher;
    use std::io::Read;
    use std::time::{Duration, Instant};

    /// 最多等 `budget` 直到条件成立；返回是否等到（不成立时让断言报出真实状态）。
    fn until(mut cond: impl FnMut() -> bool, budget: Duration) -> bool {
        let step = Duration::from_millis(10);
        let started = Instant::now();
        while !cond() {
            if started.elapsed() >= budget {
                return false;
            }
            std::thread::sleep(step);
        }
        true
    }

    /// 快超时的下载器：测试里不等真网慢网，只验语义。
    fn fetcher() -> Arc<dyn DynAudioFetcher> {
        Arc::new(
            HttpAudioFetcher::new()
                .unwrap()
                .with_idle_timeout(Duration::from_millis(200))
                .with_write_buffer(16 * 1024),
        )
    }

    fn url(port: u16) -> String {
        format!("http://127.0.0.1:{port}/a.flac")
    }

    #[test]
    fn the_task_marks_the_buffer_done_when_the_fetcher_finishes() {
        let body = pattern(200_000);
        let (port, _seen) = spawn_server(
            body.clone(),
            vec![Script::Full],
            0,
            Duration::from_millis(0),
        );
        let dir = test_dir("task_done");
        let buf = StreamBuffer::new(dir.join("t.flac"));
        let task = StreamTask::spawn(fetcher(), url(port), Arc::clone(&buf), None);

        // 任务在**另一条线程**上跑：调用方一边轮询水位，一边就能去起播
        assert!(
            until(|| buf.phase().is_terminal(), Duration::from_secs(10)),
            "水位/终局应由任务线程推进，实际停在 {:?}",
            buf.phase()
        );
        assert_eq!(buf.phase(), BufferPhase::Done, "成功终局由 fetcher 落定");
        assert_eq!(buf.declared(), Some(body.len() as u64));
        assert_eq!(buf.written(), body.len() as u64);
        assert_eq!(std::fs::read(buf.path()).unwrap(), body);
        drop(task);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dead_writer_leaves_a_failed_buffer_not_a_silent_one() {
        // 只喂到一半就断，且不给本层任何续传机会：任务必须认输并写下 Failed，
        // 否则读方只能靠 45 s 耐心超时才发现「没人写了」。
        let body = pattern(120_000);
        let prefix = 40_000;
        let (port, _seen) = spawn_server(
            body,
            vec![Script::Truncate],
            prefix,
            Duration::from_millis(0),
        );
        let dir = test_dir("task_failed");
        let buf = StreamBuffer::new(dir.join("f.flac"));
        let stalled: Arc<dyn DynAudioFetcher> = Arc::new(
            HttpAudioFetcher::new()
                .unwrap()
                .with_idle_timeout(Duration::from_millis(200))
                .with_resume_tries(0)
                .with_write_buffer(16 * 1024),
        );
        let task = StreamTask::spawn(stalled, url(port), Arc::clone(&buf), None);

        assert!(
            until(|| buf.phase().is_terminal(), Duration::from_secs(10)),
            "认输必须写成终局状态"
        );
        let phase = buf.phase();
        assert!(
            matches!(phase, BufferPhase::Failed(_)),
            "携带原因的是 Failed，实际 {phase:?}"
        );
        let written = buf.written();
        let floor = prefix.saturating_sub(16 * 1024) as u64;
        assert!(
            written >= floor && written <= prefix as u64,
            "已落盘的水位是事实，失败不该把它抹掉（实际 {written}）"
        );
        // 认输时不删文件：读方可能还开着它，清扫归装载器
        assert!(buf.path().exists());
        drop(task);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_expired_url_is_refreshed_and_the_download_continues_at_the_watermark() {
        // 旧地址上的两轮：①声明全长只吐 80 KB 就断 → 本层 Range 续传一次；
        // ②签名已失效（403）→ 交出 PlaybackUrlUnavailable。
        // 新地址上的第三轮必须**从 80 KB 水位**接下去，而不是从 0 重下。
        let body = pattern(300_000);
        let prefix = 80_000;
        let (stale_port, stale_seen) = spawn_server(
            body.clone(),
            vec![Script::Truncate, Script::Denied],
            prefix,
            Duration::from_millis(0),
        );
        let (fresh_port, fresh_seen) = spawn_server(
            body.clone(),
            vec![Script::Partial],
            0,
            Duration::from_millis(0),
        );
        let dir = test_dir("task_refresh");
        let buf = StreamBuffer::new(dir.join("x.flac"));

        // 假装载器：重取只给「好地址」——坏地址是 spawn 时就在手里的那一个
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let refresher: UrlRefresher = {
            let calls = Arc::clone(&calls);
            let fresh = url(fresh_port);
            Box::new(move || {
                let (calls, fresh) = (Arc::clone(&calls), fresh.clone());
                Box::pin(async move {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(fresh)
                }) as BoxFuture<'static, Result<String, ProviderError>>
            })
        };

        let task = StreamTask::spawn(
            fetcher(),
            url(stale_port),
            Arc::clone(&buf),
            Some(refresher),
        );
        assert!(
            until(|| buf.phase().is_terminal(), Duration::from_secs(10)),
            "换新地址后应跑到终局，实际停在 {:?}",
            buf.phase()
        );
        assert_eq!(buf.phase(), BufferPhase::Done, "换地址后应把整首下完");
        assert_eq!(buf.declared(), Some(body.len() as u64));
        assert_eq!(buf.written(), body.len() as u64);
        assert_eq!(
            std::fs::read(buf.path()).unwrap(),
            body,
            "前缀 + 新地址续传段必须逐字节拼回原文"
        );
        assert_eq!(
            requested_offset(&fresh_seen.lock().unwrap()[0]),
            Some(prefix as u64),
            "重取之后从水位续传，不重下已到手的前缀"
        );
        assert_eq!(stale_seen.lock().unwrap().len(), 2, "旧地址上只该有两轮");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // 读方视角：Done 之后整首可读，越线即真 EOF
        let mut all = Vec::new();
        StreamReader::open(Arc::clone(&buf))
            .unwrap()
            .read_to_end(&mut all)
            .expect("整首都已落盘，不该需要等待");
        assert_eq!(all, body);
        drop(task);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dropping_the_handle_stops_the_writer_without_waiting_for_a_chunk() {
        // 服务器发 4 KB 后按住连接 300 ms 不再吐字节：取消只能靠轮询定时器生效
        let body = pattern(200_000);
        let (port, _seen) = spawn_server(
            body,
            vec![Script::Stall, Script::Stall],
            4096,
            Duration::from_millis(300),
        );
        let dir = test_dir("task_drop");
        let buf = StreamBuffer::new(dir.join("d.flac"));
        let quick: Arc<dyn DynAudioFetcher> = Arc::new(
            HttpAudioFetcher::new()
                .unwrap()
                .with_idle_timeout(Duration::from_millis(200))
                .with_write_buffer(4096),
        );
        let task = StreamTask::spawn(quick, url(port), Arc::clone(&buf), None);
        assert!(
            until(|| buf.written() > 0, Duration::from_secs(5)),
            "先让它下到位，才有「中途被丢弃」可测"
        );

        let started = Instant::now();
        // join() = 请求停止 + 等线程退出；耗时就是「取消反应速度」的实测值
        let phase = task.join();
        assert_eq!(
            phase,
            BufferPhase::Aborted,
            "丢弃句柄等于换台，终局是 Aborted"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "停止不该等服务器把下一批字节送来（实际 {:?}）",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
