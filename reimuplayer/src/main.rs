//! CLI 验证入口。
//!
//! 本二进制**不是播放器 UI**，而是用来做人工验证与调试的命令行外壳：
//! 每个子命令对应一条核心能力，输出可读结果与日志，方便确认行为是否符合预期。
//!
//! ```text
//! reimuplayer info  <文件>              验证媒体加载与元数据
//! reimuplayer play  <文件> [--秒数 N]    验证播放控制与进度
//! reimuplayer search <关键词> [--limit N] 验证阶段 B 在线搜索
//! reimuplayer song <id[,id,...]>         验证阶段 B 歌曲详情（批量）
//! reimuplayer playlist <id> [--offset N] [--limit N]  验证阶段 B 歌单分页
//! reimuplayer check <歌曲ID>             验证阶段 B 可播预检（不做绕过）
//! reimuplayer url <歌曲ID>               验证阶段 B 播放地址（不做绕过）
//! reimuplayer online <歌曲ID> [--秒数 N] [--switch <歌曲ID>] [--lyric]
//!                                        验证阶段 C 在线播放链路（M3）；
//!                                        --lyric 用实际播放时钟驱动歌词同步
//! reimuplayer stream <歌曲ID> [--秒数 N]  实测边下边播（方案 2A）：闸门耗时、
//!                                        声明总长、收工耗时、平均下行速率
//! reimuplayer lyric <歌曲ID> [--at 秒]   验证阶段 D 歌词加载与同步（M4）
//! reimuplayer queue <id[,id,…]> | --search <关键词> [--limit N]
//!                                        验证阶段 D 播放队列交互（M4）
//! reimuplayer help
//! ```
//!
//! 日志级别由 `RUST_LOG` 控制，例如 `RUST_LOG=reimuplayer=debug`。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reimuplayer::audio::RodioEngine;
use reimuplayer::music::provider::QualityCeiling;
use reimuplayer::music::stream_buffer::{start_gate, BufferPhase};
use reimuplayer::music::PlaybackController;
use reimuplayer::providers::NeteaseProvider;
use reimuplayer::AudioEngine;
use reimuplayer::MusicProvider;

fn main() {
    reimuplayer::logging::init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match dispatch(&args) {
        Ok(()) => 0,
        Err(msg) => {
            eprintln!("错误: {}", msg);
            1
        }
    };
    std::process::exit(code);
}

fn dispatch(args: &[String]) -> std::result::Result<(), String> {
    let Some(cmd) = args.first().map(String::as_str) else {
        print_usage();
        return Ok(());
    };

    match cmd {
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        "info" => cmd_info(&args[1..]),
        "play" => cmd_play(&args[1..]),
        "search" => cmd_search(&args[1..]),
        "song" => cmd_song(&args[1..]),
        "playlist" => cmd_playlist(&args[1..]),
        "check" => cmd_check(&args[1..]),
        "url" => cmd_url(&args[1..]),
        "online" => cmd_online(&args[1..]),
        "stream" => cmd_stream(&args[1..]),
        "lyric" => cmd_lyric(&args[1..]),
        "queue" => cmd_queue(&args[1..]),
        other => Err(format!(
            "未知命令「{}」。使用 `reimuplayer help` 查看用法。",
            other
        )),
    }
}

fn print_usage() {
    println!(
        "reimuplayer —— 播放器核心验证工具

用法:
  reimuplayer info   <媒体文件>              显示媒体加载结果（格式/时长/标签）
  reimuplayer play   <媒体文件> [--秒数 N]    播放 N 秒并每秒打印进度（默认 5 秒）
  reimuplayer search <关键词>   [--limit N]  在线搜索歌曲（阶段 B，默认 10 条）
  reimuplayer song   <id[,id,...]>           按 ID 批量查歌曲详情（阶段 B）
  reimuplayer playlist <歌单ID> [--offset N] [--limit N]
                                             歌单分页详情（阶段 B，默认 offset=0 limit=10）
  reimuplayer check  <歌曲ID>                可播预检（阶段 B，如实上报，不做绕过）
  reimuplayer url    <歌曲ID>                播放地址（阶段 B，如实上报，不做绕过）
  reimuplayer online <歌曲ID> [--秒数 N] [--switch <歌曲ID>] [--lyric]
                                             在线播放链路（阶段 C）：取址→下载→装载→
                                             播放→seek→暂停/恢复→（可选）切歌→清理；
                                             --lyric 同时验证歌词随播放时钟同步（阶段 D）
  reimuplayer stream <歌曲ID> [--秒数 N]    边下边播实测（方案 2A）：起播闸门耗时、
                                             声明总长、下载收工耗时、平均下行速率、
                                             断供时的「缓冲中」是否如实上报
                                             [--音质 standard|higher|exhigh|lossless]
                                             探针取样旋钮；权益不足时如实回退并上报
  reimuplayer lyric  <歌曲ID> [--at 秒]      歌词加载 + 按播放位置同步（阶段 D）
  reimuplayer queue  <id[,id,…]>             播放队列交互（阶段 D）：入队→next/prev→jump
  reimuplayer queue  --search <关键词> [--limit N]
                                             搜索结果入队并导航（阶段 D）
  reimuplayer help

环境变量:
  RUST_LOG=reimuplayer=debug   控制日志级别（默认 info）
"
    );
}

/// 取位置参数（第一个非 `--` 参数）。
///
/// 已知的**带值**选项（`--键 值`）之后的一个参数视为取值被跳过；
/// 布尔选项（如 `--lyric`）不吞下一个参数。
fn positional(args: &[String]) -> std::result::Result<String, String> {
    let mut skip_next = false;
    for a in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if a.starts_with("--") {
            skip_next = matches!(
                a.as_str(),
                "--秒数" | "--limit" | "--offset" | "--switch" | "--at" | "--search"
            );
            continue;
        }
        return Ok(a.clone());
    }
    Err("缺少必要参数".to_string())
}

/// 是否存在布尔选项 `--键`。
fn has_flag(args: &[String], key: &str) -> bool {
    let flag = format!("--{}", key);
    args.iter().any(|a| *a == flag)
}

/// 取 `--键 值` 形式的字符串选项。
fn string_option(args: &[String], key: &str) -> Option<String> {
    let flag = format!("--{}", key);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if *a == flag {
            return it.next().cloned();
        }
    }
    None
}

/// 取 `--键 值` 形式的整数选项。
fn int_option(args: &[String], key: &str, default: u64) -> std::result::Result<u64, String> {
    let flag = format!("--{}", key);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if *a == flag {
            let v = it.next().ok_or_else(|| format!("{} 后缺少数值", flag))?;
            return v
                .parse::<u64>()
                .map_err(|_| format!("{} 的数值无效: {}", flag, v));
        }
    }
    Ok(default)
}

// ---------------------------------------------------------------------------

/// 验证「媒体加载」：文件探测、格式、时长、标签。
fn cmd_info(args: &[String]) -> std::result::Result<(), String> {
    let path = PathBuf::from(positional(args)?);

    let info = reimuplayer::load_media(&path).map_err(|e| e.to_string())?;

    println!("媒体加载成功");
    println!("  路径     : {}", info.path.display());
    println!("  格式     : {}", info.format);
    println!(
        "  时长     : {}",
        info.duration
            .map(reimuplayer::music::models::format_duration)
            .unwrap_or_else(|| "未知".into())
    );
    println!("  文件大小 : {} 字节", info.file_size);
    println!(
        "  标题     : {}{}",
        info.display_title(),
        if info.tags.title.is_some() {
            "（来自标签）"
        } else {
            "（回退自文件名）"
        }
    );
    println!("  艺人     : {}", info.display_artist());
    println!(
        "  专辑     : {}",
        info.tags.album.as_deref().unwrap_or("未知")
    );
    Ok(())
}

/// 验证「播放控制 + 进度 + 状态管理」。
fn cmd_play(args: &[String]) -> std::result::Result<(), String> {
    let path = PathBuf::from(positional(args)?);
    let seconds = int_option(args, "秒数", 5)?;

    let engine = RodioEngine::open().map_err(|e| {
        format!(
            "{}\n提示: 本命令需要音频输出设备。若设备不可用，可用 `info` 验证媒体加载，\
             或运行 `cargo test` 验证播放状态机（测试使用确定性引擎，不依赖声卡）。",
            e
        )
    })?;

    let mut controller = PlaybackController::new(engine);

    println!("== 加载 ==");
    let title = {
        let media = controller.load(&path).map_err(|e| e.to_string())?;
        media.display_title()
    };
    println!("  状态 = {} | 媒体 = {}", controller.state(), title);
    println!("  进度 = {}", controller.progress());

    println!("== 播放 ==");
    controller.play().map_err(|e| e.to_string())?;

    // 每 500ms 轮询一次状态并打印进度；进度值来自音频输出时钟。
    let ticks = seconds * 2;
    for i in 0..ticks {
        std::thread::sleep(Duration::from_millis(500));
        // poll() 更新状态机（检测自然播完），返回值即刻释放借用
        controller.poll();
        println!(
            "  [{:>4.1}s] 状态 = {:<6} 进度 = {}",
            (i + 1) as f32 / 2.0,
            controller.state().name(),
            controller.progress()
        );
        if matches!(controller.state(), reimuplayer::PlaybackState::Ended) {
            println!("  播放自然结束");
            break;
        }
    }

    println!("== 暂停/恢复 ==");
    // 媒体可能已在上一段自然播完（状态 Ended），此时 pause 是非法操作。
    // 先重新起播，保证「暂停/恢复」这一段始终可被验证。
    if matches!(
        controller.state(),
        reimuplayer::PlaybackState::Ended | reimuplayer::PlaybackState::Ready
    ) {
        controller.play().map_err(|e| e.to_string())?;
        println!("  已重新起播 | {}", controller.progress());
    }

    controller.pause().map_err(|e| e.to_string())?;
    let at = controller.progress();
    std::thread::sleep(Duration::from_millis(600));
    let after = controller.progress();
    println!("  暂停后 600ms 进度保持: {} -> {}（应不变）", at, after);

    controller.play().map_err(|e| e.to_string())?;
    println!("  已恢复播放 | {}", controller.progress());

    println!("== 停止 ==");
    controller.stop().map_err(|e| e.to_string())?;
    println!(
        "  状态 = {} | 进度 = {}",
        controller.state(),
        controller.progress()
    );

    Ok(())
}

/// 验证阶段 B「在线搜索」。
fn cmd_search(args: &[String]) -> std::result::Result<(), String> {
    let keyword = positional(args)?;
    let limit = int_option(args, "limit", 10)? as u32;

    // 搜索是异步操作，经 run_provider 在独立运行时执行，不阻塞音频相关线程。
    let kw_for_call = keyword.clone();
    let tracks =
        run_provider(|p| Box::pin(async move { p.search_songs(&kw_for_call, limit).await }))
            .map_err(|e| format!("搜索失败: {}", e))?;

    println!("搜索「{}」命中 {} 条:", keyword, tracks.len());
    for (i, t) in tracks.iter().enumerate() {
        print_track_row(i, t);
    }
    Ok(())
}

/// 打印一条歌曲详情行（`song` / `playlist` 共用）。
fn print_track_row(index: usize, t: &reimuplayer::Track) {
    let dur = t
        .duration_ms
        .map(|ms| reimuplayer::music::models::format_duration(Duration::from_millis(ms)))
        .unwrap_or_else(|| "--:--".into());
    println!(
        "  {:>2}. [{}] {:<28} {:>6}  专辑: {}",
        index + 1,
        t.id,
        t.display_line(),
        dur,
        t.album.as_deref().unwrap_or("-")
    );
}

/// 在独立运行时里执行异步 Provider 调用（不阻塞音频相关线程）。
///
/// 全部 Provider 通道共用 [`LoadState`] 加载状态流转：
/// 发起前「加载中…」，结束后「已加载」或「加载失败：{人话提示}」
/// （规范 8.2：不向用户暴露 JSON/堆栈；规范 8.4：与本地播放状态分离）。
fn run_provider<T>(
    op: impl FnOnce(
        NeteaseProvider,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, reimuplayer::ProviderError>> + Send>,
    >,
) -> Result<T, String> {
    let mut state = reimuplayer::LoadState::<T>::new();
    state.begin_load();
    println!(
        "  状态 = {}",
        state.status_text(reimuplayer::i18n::Lang::Zh)
    );

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    let result = rt.block_on(async {
        let provider = NeteaseProvider::new();
        op(provider).await
    });
    state.finish(result);

    if let Some(err) = state.error() {
        let msg = err.to_string();
        println!(
            "  状态 = {} | {}",
            state.status_text(reimuplayer::i18n::Lang::Zh),
            state.message(reimuplayer::i18n::Lang::Zh)
        );
        return Err(msg);
    }
    println!(
        "  状态 = {}",
        state.status_text(reimuplayer::i18n::Lang::Zh)
    );
    match state {
        reimuplayer::LoadState::Ready(data) => Ok(data),
        _ => unreachable!("无错误时必然处于 Ready"),
    }
}

/// 验证阶段 B「歌曲详情」（规范 7.2）。
fn cmd_song(args: &[String]) -> std::result::Result<(), String> {
    let raw = positional(args)?;
    let ids: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if ids.is_empty() {
        return Err("缺少歌曲 ID".into());
    }

    let ids_for_call = ids.clone();
    let tracks = run_provider(|p| Box::pin(async move { p.song_details(&ids_for_call).await }))
        .map_err(|e| format!("歌曲详情失败: {}", e))?;

    println!("歌曲详情: {} 个 ID -> {} 条:", ids.len(), tracks.len());
    for (i, t) in tracks.iter().enumerate() {
        print_track_row(i, t);
    }
    Ok(())
}

/// 验证阶段 B「歌单分页」（规范 7.3）。
fn cmd_playlist(args: &[String]) -> std::result::Result<(), String> {
    let id = positional(args)?;
    let offset = int_option(args, "offset", 0)? as u32;
    let limit = int_option(args, "limit", 10)? as u32;

    let playlist = run_provider(|p| Box::pin(async move { p.playlist(&id, offset, limit).await }))
        .map_err(|e| format!("歌单获取失败: {}", e))?;

    println!(
        "歌单 [{}] {} | 总数: {} | 本页 offset={} limit={}",
        playlist.id,
        playlist.name.as_deref().unwrap_or("（名称未知）"),
        playlist
            .total
            .map(|n| n.to_string())
            .unwrap_or_else(|| "未知".into()),
        offset,
        limit
    );
    println!("本页曲目 {} 条:", playlist.tracks.len());
    for (i, t) in playlist.tracks.iter().enumerate() {
        print_track_row(i, t);
    }
    Ok(())
}

/// 验证阶段 B「可播预检」（计划项 5，不做绕过）。
fn cmd_check(args: &[String]) -> std::result::Result<(), String> {
    let id = positional(args)?;

    let id_for_call = id.clone();
    let availability =
        run_provider(|p| Box::pin(async move { p.check_availability(&id_for_call).await }))
            .map_err(|e| format!("可播预检失败: {}", e))?;

    println!("歌曲 [{}] {}", id, availability);
    Ok(())
}

/// 验证阶段 B「播放地址」（规范 7.5 / 6.3，不做绕过）。
fn cmd_url(args: &[String]) -> std::result::Result<(), String> {
    let id = positional(args)?;

    let id_for_call = id.clone();
    let source = run_provider(|p| Box::pin(async move { p.playback_source(&id_for_call).await }))
        .map_err(|e| format!("播放地址获取失败: {}", e))?;

    println!("歌曲 [{}] 播放地址:", id);
    println!("  url          : {}", source.url);
    match source.expires_at {
        Some(ts) => println!("  expires_at   : {}（Unix 秒，由接口 expi 推导）", ts),
        None => println!("  expires_at   : 未知（接口未提供有效期，不伪造）"),
    }
    println!(
        "  quality      : {}",
        source
            .quality
            .map(|q| q.to_string())
            .unwrap_or_else(|| "未知".into())
    );
    println!(
        "  content_type : {}",
        source.content_type.as_deref().unwrap_or("未知")
    );
    Ok(())
}

/// 从 URL 中提取主机名（日志/输出只记主机名，不落完整签名地址）。
fn url_host(url: &str) -> &str {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    rest.split(['/', '?']).next().unwrap_or(rest)
}

/// 验证阶段 C「在线播放链路」（M3）。
///
/// 覆盖：取播放地址 → 下载临时文件 → 校验 → 装载进统一播放控制器 →
/// 播放/进度 → seek → 暂停/恢复 →（可选 `--switch`）切歌 → 清理临时文件。
///
/// 线程约束（规范 M3 验收）：网络请求全部发生在独立 tokio 运行时里，
/// 音频引擎线程/回调不做任何网络 I/O。
fn cmd_online(args: &[String]) -> std::result::Result<(), String> {
    let song_id = positional(args)?;
    let seconds = int_option(args, "秒数", 5)?;
    let switch_id = string_option(args, "switch");
    let with_lyric = has_flag(args, "lyric");

    println!("== 阶段 C 在线播放链路（M3）==");
    println!("  歌曲 ID : {}", song_id);

    // ---- 网络侧：独立 tokio 运行时（不阻塞、不占用音频线程）----
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    let fetcher = reimuplayer::HttpAudioFetcher::new()
        .map_err(|e| format!("HTTP 下载器初始化失败: {}", e))?;
    let mut loader = reimuplayer::OnlineTrackLoader::new(NeteaseProvider::new(), fetcher);

    println!("== 取播放地址 + 下载 ==");
    let prepared_at = Instant::now();
    let prepared = rt
        .block_on(loader.prepare(&song_id))
        .map_err(|e| format!("在线装载失败: {}", e))?;
    println!("  装载耗时 : {:?}", prepared_at.elapsed());

    println!(
        "  播放地址 : {}/…（只记主机名；完整地址用 `url` 命令查看）",
        url_host(&prepared.source.url)
    );
    match prepared.source.expires_at {
        Some(ts) => println!("  有效期至 : {}（Unix 秒，由接口 expi 推导）", ts),
        None => println!("  有效期至 : 未知（接口未提供，不伪造）"),
    }
    println!(
        "  音质     : {}",
        prepared
            .source
            .quality
            .map(|q| q.to_string())
            .unwrap_or_else(|| "未知".into())
    );
    let file_size = match prepared.stream.as_ref() {
        // 边下边播：磁盘上的实时长度永远落后于水位，报**声明总长**才是事实
        Some(buf) => buf.declared().unwrap_or_else(|| buf.written()),
        None => std::fs::metadata(&prepared.path)
            .map(|m| m.len())
            .unwrap_or(0),
    };
    println!(
        "  装载方式 : {}",
        match prepared.stream.as_ref() {
            Some(buf) => format!(
                "边下边播（闸门 {} B 已过，起播时已落盘 {} B / 声明 {} B）",
                start_gate(prepared.source.bitrate_kbps),
                buf.written(),
                buf.declared().unwrap_or(0)
            ),
            None => "整首下载".to_string(),
        }
    );
    println!(
        "  本地文件 : {}（{} 字节）",
        prepared.path.display(),
        file_size
    );

    // ---- 音频侧：走已验证的本地解码/播放链路 ----
    let engine = RodioEngine::open().map_err(|e| {
        format!(
            "{}\n提示: 播放段需要音频输出设备。若设备不可用，可用 `url` 验证取址、\
             `cargo test` 验证在线装载状态机（测试用确定性替身，不依赖网络与声卡）。",
            e
        )
    })?;
    let mut controller = PlaybackController::new(engine);

    println!("== 装载与播放 ==");
    let title = {
        match prepared.stream.as_ref() {
            // 文件还在长：整首 `load` 会撞上尾部空洞，必须交给解码泵
            Some(buffer) => controller
                .load_stream_probed(Arc::clone(buffer), prepared.spec.clone())
                .map_err(|e| format!("边下边播装载失败: {}", e))?,
            None => controller
                .load(&prepared.path)
                .map_err(|e| format!("本地装载失败: {}", e))?,
        }
        .display_title()
    };
    println!("  媒体 = {}", title);
    controller.play().map_err(|e| e.to_string())?;

    // 歌词（阶段 D，--lyric）：歌词失败**只提示，不影响播放**（M4 验收第 1 条）。
    // 同步用实际播放进度（规范 7.4「使用实际播放时钟」），每行变化时打印。
    let lyrics = if with_lyric {
        let id_for_lyric = song_id.clone();
        let result = rt.block_on(async {
            let provider = NeteaseProvider::new();
            provider.lyrics(&id_for_lyric).await
        });
        match result {
            Ok(l) => {
                println!("  歌词 = {}（{} 行）", l.kind, l.lines.len());
                Some(l)
            }
            Err(e) => {
                println!("  歌词不可用: {}（不影响播放）", e);
                None
            }
        }
    } else {
        None
    };
    let mut last_line: Option<usize> = None;

    let ticks = seconds * 2;
    for i in 0..ticks {
        std::thread::sleep(Duration::from_millis(500));
        controller.poll();
        println!(
            "  [{:>4.1}s] 状态 = {:<6} 缓冲 = {:<2} 进度 = {}",
            (i + 1) as f32 / 2.0,
            controller.state().name(),
            if controller.engine().is_buffering() {
                "是"
            } else {
                "否"
            },
            controller.progress()
        );
        // 歌词同步：以实际播放位置查询当前行（纯内存二分，无 I/O）
        if let Some(l) = &lyrics {
            let pos = controller.progress().position;
            if let Some(idx) = l.line_at(pos) {
                if last_line != Some(idx) {
                    println!(
                        "        ♪ [{}] {}",
                        fmt_ms(l.lines[idx].start_ms),
                        l.lines[idx].text
                    );
                    last_line = Some(idx);
                }
            }
        }
        if matches!(controller.state(), reimuplayer::PlaybackState::Ended) {
            println!("  播放自然结束");
            break;
        }
    }

    println!("== seek ==");
    if matches!(controller.state(), reimuplayer::PlaybackState::Ended) {
        controller.play().map_err(|e| e.to_string())?;
    }
    let before = controller.progress();
    // 定位到总时长一半（时长未知则退到 3 秒处）
    let target = before
        .total
        .map(|t| t / 2)
        .filter(|t| *t > Duration::ZERO)
        .unwrap_or(Duration::from_secs(3));
    controller
        .seek(target)
        .map_err(|e| format!("seek 失败: {}", e))?;
    println!(
        "  seek 到 {:?} | 进度 {} -> {}",
        target,
        before,
        controller.progress()
    );
    // seek 后歌词重定位（同步查询跟随实际播放位置，不是单调累加）
    if let Some(l) = &lyrics {
        if let Some(idx) = l.line_at(controller.progress().position) {
            println!(
                "        ♪ seek 后定位: [{}] {}",
                fmt_ms(l.lines[idx].start_ms),
                l.lines[idx].text
            );
        }
    }

    println!("== 暂停/恢复 ==");
    if matches!(
        controller.state(),
        reimuplayer::PlaybackState::Ended | reimuplayer::PlaybackState::Ready
    ) {
        controller.play().map_err(|e| e.to_string())?;
    }
    controller.pause().map_err(|e| e.to_string())?;
    let at = controller.progress();
    std::thread::sleep(Duration::from_millis(600));
    let after = controller.progress();
    println!("  暂停后 600ms 进度保持: {} -> {}（应不变）", at, after);
    controller.play().map_err(|e| e.to_string())?;
    println!("  已恢复播放 | {}", controller.progress());

    if let Some(sid) = switch_id {
        println!("== 切歌（--switch {}）==", sid);
        let next = rt
            .block_on(loader.prepare(&sid))
            .map_err(|e| format!("切歌在线装载失败: {}", e))?;
        let next_title = {
            match next.stream.as_ref() {
                Some(buffer) => {
                    let info = controller
                        .load_stream_probed(Arc::clone(buffer), next.spec.clone())
                        .map_err(|e| format!("切歌（边下边播）装载失败: {}", e))?;
                    let title = info.display_title();
                    controller.play().map_err(|e| e.to_string())?;
                    title
                }
                None => controller
                    .switch_to(&next.path)
                    .map_err(|e| format!("切歌失败: {}", e))?
                    .display_title(),
            }
        };
        println!(
            "  已切换到 [{}] {} | 状态 = {}",
            sid,
            next_title,
            controller.state()
        );
        for _ in 0..4 {
            std::thread::sleep(Duration::from_millis(500));
            controller.poll();
        }
        println!("  切歌后进度 = {}", controller.progress());
    }

    println!("== 停止与清理 ==");
    controller.stop().map_err(|e| e.to_string())?;
    println!(
        "  状态 = {} | 进度 = {}",
        controller.state(),
        controller.progress()
    );
    // 先释放播放器对文件的占用，再删临时文件（Windows 下删除打开中的文件会失败）
    drop(controller);
    loader.cleanup();
    println!("  临时文件已清理: {}", loader.temp_dir().display());

    Ok(())
}

/// 实测「边下边播」（方案 2A）里**用户能感觉到的那几个数字**。
///
/// `online` 验的是链路通不通，本命令量的是等了多久：从点播到闸门过（≈ 能出声）
/// 的耗时、声明总长、下载收工耗时与平均下行速率，并把每一拍的水位与「缓冲中」
/// 如实打出来。无损卡顿有没有真被消掉，看这三行就够了：
/// - `可出声耗时` 是秒级而不是几十秒；
/// - `交回时水位` 远小于 `声明总长`（文件还在长就交出去了）；
/// - 播放过程中 `水位` 单调推进，且只在**供不上**时才出现 `缓冲 = 是`。
///
/// 走整首路径（容器不在白名单，或闸门前就下完）时同样给数字，好让两种路径
/// 能在同一台机器上直接对比。
fn cmd_stream(args: &[String]) -> std::result::Result<(), String> {
    let song_id = positional(args)?;
    let seconds = int_option(args, "秒数", 10)?;

    println!("== 边下边播实测（方案 2A）==");
    println!("  歌曲 ID   : {}", song_id);

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    let fetcher = reimuplayer::HttpAudioFetcher::new()
        .map_err(|e| format!("HTTP 下载器初始化失败: {}", e))?;
    let provider = NeteaseProvider::new();
    // `--音质` 只是探针的取样旋钮：无损档是否真能拿到，取决于账号权益，这里如实上报。
    if let Some(name) = string_option(args, "音质") {
        let ceiling = match name.as_str() {
            "standard" | "标准" => QualityCeiling::Standard,
            "higher" | "较高" => QualityCeiling::Higher,
            "exhigh" | "极高" => QualityCeiling::ExtraHigh,
            "lossless" | "无损" => QualityCeiling::Lossless,
            other => {
                return Err(format!(
                    "未知音质: {other}（可选 standard/higher/exhigh/lossless）"
                ))
            }
        };
        provider.set_quality(ceiling);
        println!("  音质上限  : {:?}", ceiling);
    }
    let mut loader = reimuplayer::OnlineTrackLoader::new(provider, fetcher);
    println!("  流式开关  : {}", loader.is_streaming());

    let prepare_at = Instant::now();
    let prepared = rt
        .block_on(loader.prepare(&song_id))
        .map_err(|e| format!("在线装载失败: {}", e))?;
    let to_gate = prepare_at.elapsed();

    println!(
        "  音质      : {}",
        prepared
            .source
            .quality
            .map(|q| q.to_string())
            .unwrap_or_else(|| "未知".into())
    );
    println!(
        "  接口声明  : {} / 码率 {} kbps → 起播闸门 {} B",
        prepared.source.content_type.as_deref().unwrap_or("未知"),
        prepared
            .source
            .bitrate_kbps
            .map(|b| b.to_string())
            .unwrap_or_else(|| "未知".into()),
        start_gate(prepared.source.bitrate_kbps)
    );
    println!("  可出声耗时: {to_gate:?}");
    match prepared.stream.as_ref() {
        Some(buf) => println!(
            "  交回时水位: {} B / 声明 {} B（文件仍在长）",
            buf.written(),
            buf.declared().unwrap_or(0)
        ),
        None => println!("  交回时水位: 整首已在盘上（未开流：容器不在白名单，或闸门前就下完）"),
    }
    println!(
        "  本地文件  : {}（格式 {} / 采样率 {} / 位深 {}）",
        prepared.path.display(),
        prepared.spec.codec.as_deref().unwrap_or("—"),
        prepared
            .spec
            .sample_rate
            .map(|v| v.to_string())
            .unwrap_or_else(|| "—".into()),
        prepared
            .spec
            .bit_depth
            .map(|v| v.to_string())
            .unwrap_or_else(|| "—".into())
    );

    let engine = RodioEngine::open().map_err(|e| {
        format!(
            "打开音频输出失败: {e}\n提示: 本命令需要音频输出设备；取址与下载部分可用 \
             `url` / `online` 单独验证。"
        )
    })?;
    let mut controller = PlaybackController::new(engine);

    let load_at = Instant::now();
    let title = match prepared.stream.as_ref() {
        Some(buf) => controller
            .load_stream_probed(Arc::clone(buf), prepared.spec.clone())
            .map_err(|e| format!("边下边播装载失败: {}（文件头可能不认这个容器）", e))?,
        None => controller
            .load_probed(&prepared.path, prepared.spec.clone())
            .map_err(|e| format!("本地装载失败: {}", e))?,
    }
    .display_title();
    println!("  解码器起泵: {:?} | 媒体 = {}", load_at.elapsed(), title);
    controller.play().map_err(|e| e.to_string())?;

    println!("== 逐拍观测（250ms）==");
    let play_at = Instant::now();
    let mut done_at: Option<Duration> = None;
    let mut ticks = 0usize;
    let mut buffering_ticks = 0usize;
    loop {
        std::thread::sleep(Duration::from_millis(250));
        controller.poll();
        ticks += 1;
        let elapsed = play_at.elapsed();
        let buffering = controller.engine().is_buffering();
        if buffering {
            buffering_ticks += 1;
        }
        match prepared.stream.as_ref() {
            Some(buf) => {
                // 收工时刻按「点播起算」量：含一次取址，与用户等的总时长同口径
                if done_at.is_none() && buf.phase() != BufferPhase::Growing {
                    done_at = Some(prepare_at.elapsed());
                }
                println!(
                    "  [{:>5.1}s] 状态 = {:<6} 缓冲 = {:<2} 进度 = {} | 水位 {} B / {} B | 阶段 {:?}",
                    elapsed.as_secs_f32(),
                    controller.state().name(),
                    if buffering { "是" } else { "否" },
                    controller.progress(),
                    buf.written(),
                    buf.declared().unwrap_or(0),
                    buf.phase()
                );
            }
            None => println!(
                "  [{:>5.1}s] 状态 = {:<6} 缓冲 = {:<2} 进度 = {}",
                elapsed.as_secs_f32(),
                controller.state().name(),
                if buffering { "是" } else { "否" },
                controller.progress()
            ),
        }
        if elapsed >= Duration::from_secs(seconds) {
            break;
        }
        if matches!(controller.state(), reimuplayer::PlaybackState::Ended) {
            println!("  播放自然结束");
            break;
        }
    }

    println!("== 收工 ==");
    match prepared.stream.as_ref() {
        Some(buf) => {
            let total = buf.declared().unwrap_or_else(|| buf.written());
            match done_at {
                Some(t) if t.as_secs_f32() > 0.0 => println!(
                    "  下载收工  : {t:?}（点播起算，{} B）→ 平均 {:.2} MiB/s",
                    total,
                    total as f64 / t.as_secs_f64() / (1024.0 * 1024.0)
                ),
                Some(_) => println!("  下载收工  : 几乎瞬间（{} B）", total),
                None => println!(
                    "  下载未收工: 观测窗口内水位停在 {} B / {} B",
                    buf.written(),
                    total
                ),
            }
            println!(
                "  「缓冲中」: {} / {} 拍（供得上就该是 0 或个位数）",
                buffering_ticks, ticks
            );
        }
        None => println!("  整首路径  : 可出声耗时 {to_gate:?}（等的是整首，不是闸门）"),
    }
    println!("  最终进度  : {}", controller.progress());

    controller.stop().map_err(|e| e.to_string())?;
    // 先释放播放器对文件的占用，再删临时文件（Windows 下删除打开中的文件会失败）
    drop(controller);
    loader.cleanup();
    println!("  临时文件已清理: {}", loader.temp_dir().display());

    Ok(())
}

/// 毫秒 → `mm:ss.mmm` 展示。
fn fmt_ms(ms: u64) -> String {
    let secs = ms / 1000;
    format!("{:02}:{:02}.{:03}", secs / 60, secs % 60, ms % 1000)
}

/// 验证阶段 D「歌词加载 + 同步」（M4，规范 7.4）。
///
/// 歌词缺失只提示、不影响任何播放路径（M4 验收第 1 条）。
fn cmd_lyric(args: &[String]) -> std::result::Result<(), String> {
    let id = positional(args)?;
    let at_secs = int_option(args, "at", 0)?;

    println!("== 阶段 D 歌词（M4）==");
    println!("  歌曲 ID : {}", id);

    let id_for_call = id.clone();
    let lyrics = run_provider(|p| Box::pin(async move { p.lyrics(&id_for_call).await }));

    match lyrics {
        Err(msg) => {
            // 歌词缺失/失败仅提示——绝不影响播放路径（验收第 1 条）
            println!("  歌词不可用: {}", msg);
            println!("  （歌词缺失不影响播放——本命令仅验证歌词通道）");
            Ok(())
        }
        Ok(lyrics) => {
            println!(
                "  种类 = {} | 行数 = {} | offset = {}ms | 丢弃坏行 = {}",
                lyrics.kind,
                lyrics.lines.len(),
                lyrics.offset_ms,
                lyrics.dropped_lines
            );
            for (i, l) in lyrics.lines.iter().take(20).enumerate() {
                println!("  {:>3}. [{}] {}", i + 1, fmt_ms(l.start_ms), l.text);
            }
            if lyrics.lines.len() > 20 {
                println!("  …（共 {} 行，仅显示前 20 行）", lyrics.lines.len());
            }

            // 按播放位置同步（实际播放时钟的抽象——CLI 用 --at 模拟位置）
            let position = Duration::from_secs(at_secs);
            match lyrics.line_at(position) {
                Some(idx) => println!(
                    "  同步 @{}s → 第 {} 行: [{}] {}",
                    at_secs,
                    idx + 1,
                    fmt_ms(lyrics.lines[idx].start_ms),
                    lyrics.lines[idx].text
                ),
                None => println!("  同步 @{}s → 无歌词行（位置早于第一行或无歌词）", at_secs),
            }
            Ok(())
        }
    }
}

/// 验证阶段 D「播放队列交互」（M4，规范 9.2）。
///
/// 两个入口：`<id[,id…]>` 详情批量入队；`--search <关键词>` 搜索结果入队。
/// 展示：入队（有界截断）→ 导航 next/prev/jump → 队列内存有界不变式。
fn cmd_queue(args: &[String]) -> std::result::Result<(), String> {
    let search = string_option(args, "search");

    let tracks = match search {
        Some(keyword) => {
            let limit = int_option(args, "limit", 10)? as u32;
            run_provider(|p| Box::pin(async move { p.search_songs(&keyword, limit).await }))
                .map_err(|e| format!("搜索失败: {}", e))?
        }
        None => {
            let raw = positional(args)?;
            let ids: Vec<String> = raw
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if ids.is_empty() {
                return Err("缺少歌曲 ID（或使用 --search <关键词>）".into());
            }
            let ids_for_call = ids.clone();
            run_provider(|p| Box::pin(async move { p.song_details(&ids_for_call).await }))
                .map_err(|e| format!("歌曲详情失败: {}", e))?
        }
    };

    println!("== 阶段 D 播放队列（M4）==");
    let mut queue = reimuplayer::PlayQueue::with_default_capacity();
    let total = tracks.len();
    let added = queue.extend(tracks);
    println!(
        "  入队 {} / {} 条（容量上限 {}，超限截断——长歌单内存有界）",
        added,
        total,
        queue.capacity()
    );

    let print_current = |q: &reimuplayer::PlayQueue, tag: &str| match q.current() {
        Some(t) => println!(
            "  {} [{}/{}] {} —— {}",
            tag,
            q.current_index().unwrap_or(0) + 1,
            q.len(),
            t.display_line(),
            t.album.as_deref().unwrap_or("-")
        ),
        None => println!("  {} （队列为空）", tag),
    };

    print_current(&queue, "当前:");
    if queue.next().is_some() {
        print_current(&queue, "next :");
    } else {
        println!("  next : 已在末尾（位置不变）");
    }
    if queue.prev().is_some() {
        print_current(&queue, "prev :");
    } else {
        println!("  prev : 已在开头（位置不变）");
    }
    if queue.len() > 1 {
        if queue.jump(queue.len() - 1).is_ok() {
            print_current(&queue, "jump :");
        }
    }
    if queue.remove(0).is_ok() {
        print_current(&queue, "remove(0) 后:");
    }
    println!(
        "  内存有界检查: len={} <= capacity={} ✓",
        queue.len(),
        queue.capacity()
    );
    Ok(())
}
