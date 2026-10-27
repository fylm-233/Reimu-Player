//! 离线 HTTP 测试脚手架（只在 `cfg(test)` 编译）。
//!
//! 为什么手写服务器：项目里没有任何 mock HTTP 依赖，为一个测试脚手架新增依赖
//! 不划算；而 Range 续传、断流、超时这些语义，**恰好只能用真 HTTP 语义演出来**
//! （`reqwest` 对 mock 层的反应和对 CDN 的一样）。
//!
//! 用法：[`spawn_server`] 起一个只服务固定几条连接的本地 HTTP/1.1 服务器，
//! 返回端口与「收到过的请求头」，脚本跑完线程自行退出（listener 随之释放）。
//! 装载层（[`crate::music::stream_transport`]）也用它，所以放在 `providers`
//! 这一层的共享测试模块里，而不是埋在某个单测文件中。

#![cfg(test)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 一条连接要演什么。
#[derive(Clone, Copy, Debug)]
pub(crate) enum Script {
    /// 200 + 完整 body
    Full,
    /// 200 + 声明全长，只吐 `cut` 字节就掐断连接（模拟断流）
    Truncate,
    /// 按请求里的 Range 回 206 部分体
    Partial,
    /// 只吐 `cut` 字节然后停住不发（演「没有新字节」）
    Stall,
    /// 只吐 `cut` 字节，停 `stall` 这么久，再吐完剩下的（演「慢但没死」）
    SlowSplit,
    /// 连响应头都不发，停一下再断
    Mute,
    /// 读完请求后回 403（签名 URL 失效）
    Denied,
    /// 200 但响应头不带 `Content-Length`（chunked：总长未知）
    Chunked,
}

/// 起一个本地服务器，按 `script` 依次服务连接。
///
/// 返回 `(端口, 收到过的请求头)`。
pub(crate) fn spawn_server(
    body: Vec<u8>,
    script: Vec<Script>,
    cut: usize,
    stall: Duration,
) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("测试服务器绑定失败");
    let port = listener.local_addr().expect("无本地地址").port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_tx = Arc::clone(&seen);
    std::thread::spawn(move || {
        for step in script {
            let (mut sock, _) = match listener.accept() {
                Ok(v) => v,
                Err(_) => break,
            };
            let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
            let mut raw = Vec::new();
            let mut tmp = [0u8; 512];
            while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => raw.extend_from_slice(&tmp[..n]),
                }
            }
            let text = String::from_utf8_lossy(&raw).to_string();
            let off = requested_offset(&text).unwrap_or(0) as usize;
            // 先记账再回应：客户端收完字节就可能返回，push 晚了断言会飘
            seen_tx.lock().unwrap().push(text);
            let total = body.len();
            let head = cut.min(total);
            // `declared` 是响应头里声明的长度：断流形态必须是「声明全长、
            // 只吐一截」，否则框架自洽、客户端会当成正常下完。
            match step {
                Script::Full => {
                    let _ = write_head(&mut sock, 200, total);
                    let _ = sock.write_all(&body);
                }
                // Truncate 与 Stall 发的是同一批字节，区别只在发完之后断不断
                Script::Truncate => {
                    let _ = write_head(&mut sock, 200, total);
                    let _ = sock.write_all(&body[..head]);
                }
                Script::Stall => {
                    let _ = write_head(&mut sock, 200, total);
                    let _ = sock.write_all(&body[..head]);
                    let _ = sock.flush();
                    std::thread::sleep(stall);
                }
                Script::SlowSplit => {
                    let _ = write_head(&mut sock, 200, total);
                    let _ = sock.write_all(&body[..head]);
                    let _ = sock.flush();
                    std::thread::sleep(stall);
                    let _ = sock.write_all(&body[head..]);
                }
                Script::Partial => {
                    let from = off.min(total);
                    let _ = write_head(&mut sock, 206, total - from);
                    let _ = sock.write_all(&body[from..]);
                }
                Script::Mute => {
                    // 一个字节都不发，停一下再断
                    std::thread::sleep(stall);
                }
                Script::Denied => {
                    let _ = write_head(&mut sock, 403, 0);
                }
                Script::Chunked => {
                    let _ = write_chunked_head(&mut sock, 200);
                    write_body_in_chunks(&mut sock, &body, 16 * 1024);
                }
            }
            let _ = sock.flush();
            drop(sock);
        }
    });
    (port, seen)
}

/// 写响应头；`declared` 是响应头声明的本轮响应体长度。
fn write_head(sock: &mut std::net::TcpStream, status: u16, declared: usize) -> std::io::Result<()> {
    let line = match status {
        206 => "HTTP/1.1 206 Partial Content",
        403 => "HTTP/1.1 403 Forbidden",
        _ => "HTTP/1.1 200 OK",
    };
    let head = format!(
        "{line}\r\nContent-Length: {declared}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
    );
    sock.write_all(head.as_bytes())
}

/// 写**不带 `Content-Length`** 的响应头（chunked 传输编码）。
fn write_chunked_head(sock: &mut std::net::TcpStream, status: u16) -> std::io::Result<()> {
    let line = match status {
        206 => "HTTP/1.1 206 Partial Content",
        _ => "HTTP/1.1 200 OK",
    };
    let head = format!(
        "{line}\r\nTransfer-Encoding: chunked\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
    );
    sock.write_all(head.as_bytes())
}

/// 以 chunked 编码写出 body（含合法的终止块，让客户端能认出「正常结束」）。
fn write_body_in_chunks(sock: &mut std::net::TcpStream, body: &[u8], size: usize) {
    let mut rest = body;
    while !rest.is_empty() {
        let n = rest.len().min(size.max(1));
        let _ = write!(sock, "{:x}\r\n", n);
        let _ = sock.write_all(&rest[..n]);
        let _ = sock.write_all(b"\r\n");
        rest = &rest[n..];
    }
    let _ = sock.write_all(b"0\r\n\r\n");
}

/// 从请求头里解析 `Range: bytes=<start>-`。
pub(crate) fn requested_offset(req: &str) -> Option<u64> {
    req.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if !key.trim().eq_ignore_ascii_case("range") {
            return None;
        }
        let spec = value.trim().strip_prefix("bytes=")?;
        spec.split('-').next()?.trim().parse().ok()
    })
}

/// 每个测试一套独立临时目录（并发跑测时互不踩）。
pub(crate) fn test_dir(tag: &str) -> std::path::PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "reimuplayer_{}_{}_{}",
        tag,
        std::process::id(),
        stamp
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 位置相关的字节图案：续传点错一位都会被发现。
pub(crate) fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u64 * 31 + 7) as u8).collect()
}

#[test]
fn hand_written_flac_is_decodable_whole() {
    use crate::audio::RodioEngine;
    use crate::music::load_media;
    use crate::AudioEngine as _;
    use std::io::Read as _;

    // 一整块（4096 帧）完全落盘，验证手写 FLAC 本身是真能解的——把「文件坏了」
    // 和「流式水位不够」这两件事分开。
    let (body, _frame_len) = flac_verbatim(FLAC_BLOCK);
    let dir = test_dir("flac_self");
    let path = dir.join("a.flac");
    std::fs::write(&path, &body).unwrap();

    // 头部探测走的是装载器同一条路径（load_media，靠 symphonia probe）。
    let mut head = Vec::new();
    std::fs::File::open(&path)
        .unwrap()
        .take(1 << 20)
        .read_to_end(&mut head)
        .unwrap();
    let media = load_media(&path).expect("load_media 应认出 FLAC");
    println!(
        "load_media: codec={:?} rate={:?} ch={:?} bit_depth={:?}",
        media.audio.codec, media.audio.sample_rate, media.audio.channels, media.audio.bit_depth
    );
    assert_eq!(
        media.audio.codec.as_deref(),
        Some("FLAC"),
        "魔数/容器探测应报 FLAC"
    );

    // 整首解码走 RodioEngine，和 GUI/CLI 装载一致。
    let mut engine = RodioEngine::open().expect("打开音频设备");
    let m = engine.load(&path).expect("整首装载应成功");
    println!("engine.load: duration={:?}", m.duration);
    assert!(
        m.duration.is_some(),
        "总时长应能从 STREAMINFO 推出（块数 × 块长 / 采样率）"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- 真实 FLAC 素材（手写编码器）----

/// FLAC 帧的固定块大小（采样数）。帧头里的 `0xC` 码表示 `256 << 4 = 4096`。
pub(crate) const FLAC_BLOCK: usize = 4096;

/// 44.1 kHz / 16 bit / 立体声：两声道交替存样本。
pub(crate) const FLAC_RATE: u32 = 44_100;
pub(crate) const FLAC_CHANNELS: usize = 2;

/// 文件头长度：`fLaC` 魔数(4) + STREAMINFO 块头(4) + 34 字节块体。
///
/// 帧从这个偏移开始；测试要按「头 + 整数帧 + 半帧」掐按住点，得知道头多长。
pub(crate) const FLAC_HEADER_LEN: usize = 4 + 4 + 34;

/// 第 `i` 帧左声道的取值，与 [`crate::audio::stream_pump`] 测试里的斜坡同一条曲线。
///
/// 值随帧号严格递增、从 0.05 起步（避开 0.0），所以「解出多少样本」直接说明
/// 「解码器读到第几帧」，「非静音」的判据也干净。
pub(crate) fn flac_ramp_left(total_frames: usize, i: usize) -> i16 {
    ((0.05 + (i as f32 / total_frames as f32) * 0.4) * i16::MAX as f32) as i16
}

/// 手写 **真实可解**的 FLAC：`fLaC` + STREAMINFO + 若干 verbatim 帧。
///
/// 返回 `(文件字节, 单帧字节)`：测试要把「按住点」掐在**帧中段**才验得到边下边播
/// ——正好掐在帧起点只是「一整帧没到」，解码器并不用等。
///
/// 为什么要手写：验的是「解码器对着还在长的文件能不能一路解下去」，仓库里没有
/// 任何 FLAC 素材，也不值得为测试引入一个编码依赖。verbatim 子帧是 FLAC 容许的
/// 无损编码之一（原样存 PCM），帧头位布局按 [`symphonia`] 自己的解析实现对齐
/// （块大小 4 bits、采样率 4 bits、声道 4 bits、位深 3 bits、保留 1 bit），CRC-8
/// 用 poly 0x07、帧尾 CRC-16 用 poly 0x8005 非反射（与 symphonia 的校验口径一致）。
/// 解出来的样本与喂进去的逐个相同，正好拿来逐样本核对。
///
/// 限制：总采样数必须是 [`FLAC_BLOCK`] 的整数倍（不做部分帧）。
pub(crate) fn flac_verbatim(total_frames: usize) -> (Vec<u8>, usize) {
    assert_eq!(
        total_frames % FLAC_BLOCK,
        0,
        "只生成整块帧：部分帧要单独走 8/16 位块大小码"
    );

    let mut out = Vec::with_capacity(total_frames * FLAC_CHANNELS * 2);
    out.extend_from_slice(b"fLaC");
    out.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]);
    out.extend_from_slice(&stream_info_body(total_frames));

    let block_count = total_frames / FLAC_BLOCK;
    let mut frame_len = 0usize;
    for frame in 0..block_count {
        let bytes = flac_frame(frame * FLAC_BLOCK, total_frames, frame);
        frame_len = bytes.len();
        out.extend_from_slice(&bytes);
    }
    (out, frame_len)
}

/// STREAMINFO 的 34 字节体：块长、帧长、采样率、声道、位深、总采样数、MD5。
fn stream_info_body(total_frames: usize) -> Vec<u8> {
    let mut bw = BitWriter::new();
    bw.write(FLAC_BLOCK as u64, 16); // min block size
    bw.write(FLAC_BLOCK as u64, 16); // max block size
    bw.write(0, 24); // min frame size（未知）
    bw.write(0, 24); // max frame size（未知）
    bw.write(FLAC_RATE as u64, 20); // 采样率：STREAMINFO 里是 20 位（不是 32）
    bw.write((FLAC_CHANNELS - 1) as u64, 3); // 声道数 - 1
    bw.write(15, 5); // 位深 - 1
    bw.write(total_frames as u64, 36); // 总采样数
    for _ in 0..16 {
        bw.write(0, 8); // MD5：全零表示「未知」，解码器不会据此验签
    }
    bw.into_bytes()
}

/// 一帧：帧头（含 CRC-8）+ 两个 verbatim 子帧 + 对齐填充 + 帧尾 CRC-16。
fn flac_frame(first_frame_index: usize, total_frames: usize, frame_no: usize) -> Vec<u8> {
    // 帧头整字节对齐，所以先攒字节再算 CRC-8。
    let mut head = Vec::with_capacity(8);
    head.extend_from_slice(&[0xFF, 0xF8]); // 14 位同步字 + 保留 0 + 定长策略
    let desc = (0xCu32 << 12) // 块大小 4096
        | (0x9u32 << 8) // 采样率 44100
        | (0x1u32 << 4) // 声道归属：独立立体声
        | (0x4u32 << 1); // 位深 16（保留位 0 收尾）
                         // desc 是 u32，但帧头里只占 16 位：`to_be_bytes()` 会吐 4 字节，多出的两个
                         // 0x00 会被当成帧头的一部分，块大小码读成 0（保留值）→ 失步。
    head.extend_from_slice(&(desc as u16).to_be_bytes());
    head.extend_from_slice(&utf8_number(frame_no as u64));
    let c8 = crc8(&head);
    head.push(c8);

    let mut bw = BitWriter::new();
    for ch in 0..FLAC_CHANNELS {
        // 子帧头：1 位零填充 + 6 位类型（1 = verbatim）+ 1 位「无丢弃位」= 8 位。
        bw.write(0b0, 1);
        bw.write(0b000001, 6);
        bw.write(0, 1);
        for k in 0..FLAC_BLOCK {
            let i = first_frame_index + k;
            let left = flac_ramp_left(total_frames, i);
            let sample = if ch == 0 { left } else { -left };
            bw.write(sample as u16 as u64, 16); // 有符号二补码，MSB 在前
        }
    }
    bw.align();
    let body = bw.into_bytes();

    let mut frame = head;
    frame.extend_from_slice(&body);
    let c16 = crc16_ansi(&frame);
    frame.extend_from_slice(&c16.to_be_bytes());
    frame
}

/// FLAC 的「UTF-8 风格」变长整数：1 字节装 7 位，之后每多一字节多装 6 位。
fn utf8_number(value: u64) -> Vec<u8> {
    let groups = match value {
        v if v < 1 << 7 => 1,
        v if v < 1 << 11 => 2,
        v if v < 1 << 16 => 3,
        v if v < 1 << 21 => 4,
        v if v < 1 << 26 => 5,
        _ => 6,
    };
    // 首字节的有效位：1 字节时占满 7 位；多字节时前缀是 `groups` 个 1 + 1 个 0。
    let lead_bits = if groups == 1 { 7 } else { 7 - groups };
    let total = lead_bits + 6 * (groups - 1);

    let mut out = Vec::with_capacity(groups);
    let lead = (value >> (total - lead_bits)) & ((1 << lead_bits) - 1);
    out.push(if groups == 1 {
        lead as u8
    } else {
        (((1u16 << (groups + 1)) - 2) as u8) << lead_bits | lead as u8
    });
    let mut rest = total - lead_bits;
    while rest > 0 {
        rest -= 6;
        out.push(0b10_000000 | ((value >> rest) & 0x3F) as u8);
    }
    out
}

/// CRC-8（poly 0x07，初值 0，MSB 在前）：帧头用。
fn crc8(bytes: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &byte in bytes {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// CRC-16（poly 0x8005，初值 0，非反射）：帧尾用，与对称帧字节流一致。
fn crc16_ansi(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &byte in bytes {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x8005
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// MSB 在前的位写入器；FLAC 的比特流就是这样排的。
struct BitWriter {
    out: Vec<u8>,
    cur: u8,
    filled: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            cur: 0,
            filled: 0,
        }
    }

    /// 以 `bits` 位写入 `value` 的低位（`value` 是 `u64`，最多 64 位；本用例里
    /// 最宽的是 STREAMINFO 的 36 位总采样数）。
    fn write(&mut self, value: u64, bits: u32) {
        debug_assert!(bits <= 64, "位宽受 u64 限制");
        for i in (0..bits).rev() {
            self.cur = (self.cur << 1) | ((value >> i) & 1) as u8;
            self.filled += 1;
            if self.filled == 8 {
                self.out.push(self.cur);
                self.cur = 0;
                self.filled = 0;
            }
        }
    }

    /// 补齐到字节边界（不足一个字节的残位补 0）。
    fn align(&mut self) {
        if self.filled > 0 {
            self.cur <<= 8 - self.filled;
            self.out.push(self.cur);
            self.cur = 0;
            self.filled = 0;
        }
    }

    fn into_bytes(mut self) -> Vec<u8> {
        self.align();
        self.out
    }
}
