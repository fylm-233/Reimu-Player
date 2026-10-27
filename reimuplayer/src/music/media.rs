//! 媒体加载：文件探测与元数据读取。
//!
//! 与[`crate::audio::AudioEngine`]的分工：
//! - 本模块产出 [`MediaInfo`]，是**元数据视角**（标题、艺人、时长估计）。
//!   不需要音频输出设备，因此可用于列表展示与单元测试。
//! - 引擎产出 [`crate::audio::EngineMedia`]，是**解码器权威视角**。
//!   播放进度的总时长以引擎为准。
//!
//! 元数据读取失败不应让媒体无法播放——降级为「只有文件名」的 `MediaInfo`。

use std::path::{Path, PathBuf};
use std::time::Duration;

use lofty::prelude::*;
use lofty::probe::Probe;

use super::models::{AudioFormat, AudioSpec, MediaInfo, Tags};
use crate::error::{PlayerError, Result};

/// 从路径加载媒体描述。
///
/// # 错误
/// 文件不存在或不可读会返回错误；**元数据解析失败不会返回错误**，
/// 而是降级为不含标签的 [`MediaInfo`]（原因记录到日志）。
pub fn load_media(path: impl Into<PathBuf>) -> Result<MediaInfo> {
    let path: PathBuf = path.into();

    if !path.exists() {
        return Err(PlayerError::MediaNotFound(path));
    }
    if !path.is_file() {
        return Err(PlayerError::UnsupportedFormat(format!(
            "{} 不是普通文件",
            path.display()
        )));
    }

    let meta = std::fs::metadata(&path).map_err(|source| PlayerError::MediaRead {
        path: path.clone(),
        source,
    })?;

    let format = path
        .extension()
        .map(|e| AudioFormat::from_extension(&e.to_string_lossy()))
        .unwrap_or_else(|| AudioFormat::Unknown("无扩展名".into()));

    // 元数据失败不阻断加载
    let (tags, duration, audio) = read_tags(&path).unwrap_or_else(|e| {
        tracing::warn!(
            "元数据读取失败，降级为仅文件名展示: {} — {}",
            path.display(),
            e
        );
        (Tags::default(), None, AudioSpec::default())
    });

    tracing::debug!(
        "媒体已探测: {} | 格式={} | 时长={} | 音质={:?} | 标题={:?}",
        path.display(),
        format,
        duration
            .map(super::models::format_duration)
            .unwrap_or_else(|| "未知".into()),
        audio,
        tags.title
    );

    Ok(MediaInfo {
        path,
        format,
        duration,
        tags,
        file_size: meta.len(),
        audio,
    })
}

/// [`persist_id`] 读取的窗口大小。
const PERSIST_ID_WINDOW: u64 = 256 * 1024;

/// 本地曲目匹配用的 `persistId`（32 位小写十六进制摘要）；读不了返回 `None`。
///
/// 网易云「本地歌曲匹配」接口要求 `md5`/`persistId` **非空**（实测：空串一律
/// 返回空结果），但它只是这一次查询的相关标识——响应里的 `result.ids` 原样回显
/// 我们传进去的值，真正的匹配依据是标题/艺人/专辑/时长。
///
/// 因此这里**只摘要文件开头 256 KiB**，不读整首：这个值不参与匹配质量，而完整
/// 读一首 30 MB 的 FLAC 会把请求堵在桥的单线程 worker 上（封面、歌词、装载都在
/// 那条队列里等），是用户反馈"加载慢"的同一条路径上的额外开销。
pub fn persist_id(path: &Path) -> Option<String> {
    use md5::{Digest, Md5};

    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Md5::new();
    let mut remaining = PERSIST_ID_WINDOW;
    let mut buf = vec![0u8; 64 * 1024];
    while remaining > 0 {
        let want = (remaining as usize).min(buf.len());
        let n = match std::io::Read::read(&mut file, &mut buf[..want]) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        };
        if n == 0 {
            break;
        }
        remaining -= n as u64;
        hasher.update(&buf[..n]);
    }
    Some(
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
    )
}

/// 读取标签、时长与音频流参数。返回 `(Tags, Option<Duration>, AudioSpec)`。
fn read_tags(path: &Path) -> std::result::Result<(Tags, Option<Duration>, AudioSpec), String> {
    let tagged = Probe::open(path)
        .map_err(|e| e.to_string())?
        .read()
        .map_err(|e| e.to_string())?;

    let properties = tagged.properties();
    let duration = Duration::from_secs_f64(properties.duration().as_secs_f64());

    // 音频流参数：全部 `Option`，lofty 没解析出来就留空（不猜、不补默认值）
    let audio = AudioSpec {
        codec: path
            .extension()
            .map(|e| e.to_string_lossy().to_uppercase())
            .filter(|s| !s.is_empty()),
        quality: None,
        sample_rate: properties.sample_rate(),
        bit_depth: properties.bit_depth(),
        channels: properties.channels(),
        bitrate_kbps: properties.audio_bitrate(),
    };

    // 取首个标签即可；同文件可能有 ID3v1/v2 等多套标签。
    let tags = tagged
        .primary_tag()
        .or_else(|| tagged.first_tag())
        .map(|t| Tags {
            title: t
                .get_string(ItemKey::TrackTitle)
                .map(str::to_string)
                .filter(|s| !s.trim().is_empty()),
            artist: t
                .get_string(ItemKey::TrackArtist)
                .map(str::to_string)
                .filter(|s| !s.trim().is_empty()),
            album: t
                .get_string(ItemKey::AlbumTitle)
                .map(str::to_string)
                .filter(|s| !s.trim().is_empty()),
        })
        .unwrap_or_default();

    // 时长未知时返回 None，而不是伪造 0
    let duration = if duration.is_zero() {
        None
    } else {
        Some(duration)
    };

    Ok((tags, duration, audio))
}

#[cfg(test)]
/// 构造一个最小合法 WAV（PCM 8-bit mono 8kHz，`seconds` 秒）。
///
/// 让「媒体加载」与「本地匹配素材」两条链路都能在没有真实音频素材的机器上
/// 被测（`crate::gui::bridge` 的测试也要它的时长）。
pub(crate) fn minimal_wav(dir: &Path, name: &str, seconds: u32) -> PathBuf {
    use std::io::Write;
    let sample_rate: u32 = 8000;
    let data_len = sample_rate * seconds;
    let path = dir.join(name);

    let mut f = std::fs::File::create(&path).unwrap();
    let byte_rate = sample_rate; // 8bit mono
    let header: Vec<u8> = {
        let mut h = Vec::new();
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&(36 + data_len).to_le_bytes());
        h.extend_from_slice(b"WAVE");
        h.extend_from_slice(b"fmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes()); // PCM
        h.extend_from_slice(&1u16.to_le_bytes()); // mono
        h.extend_from_slice(&sample_rate.to_le_bytes());
        h.extend_from_slice(&byte_rate.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes()); // block align
        h.extend_from_slice(&8u16.to_le_bytes()); // bits per sample
        h.extend_from_slice(b"data");
        h.extend_from_slice(&data_len.to_le_bytes());
        h
    };
    f.write_all(&header).unwrap();
    f.write_all(&vec![128u8; data_len as usize]).unwrap();
    f.flush().unwrap();
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_media_not_found() {
        let err = load_media("definitely/not/here.mp3").unwrap_err();
        assert!(matches!(err, PlayerError::MediaNotFound(_)));
    }

    #[test]
    fn loads_wav_and_reads_duration() {
        let dir = std::env::temp_dir().join("reimuplayer_media_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = minimal_wav(&dir, "tone.wav", 2);

        let info = load_media(&path).unwrap();
        assert_eq!(info.format, AudioFormat::Wav);
        assert!(info.file_size > 0);
        // 2 秒 8kHz 8bit → 时长应接近 2 秒（允许元数据估计误差）
        let dur = info.duration.expect("WAV 时长应可解析");
        assert!(
            dur >= Duration::from_millis(1900) && dur <= Duration::from_millis(2100),
            "时长异常: {:?}",
            dur
        );
        // 无标签 → 降级到文件名，而不是报错
        assert_eq!(info.display_title(), "tone");
    }

    #[test]
    fn unknown_extension_is_recorded_not_rejected() {
        let dir = std::env::temp_dir().join("reimuplayer_media_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("thing.zzz");
        std::fs::write(&path, b"not audio").unwrap();

        let info = load_media(&path).unwrap();
        assert_eq!(info.format, AudioFormat::Unknown("zzz".into()));
        // 元数据解析失败 → 降级，不报错
        assert_eq!(info.tags, Tags::default());
    }

    #[test]
    fn persist_id_is_32_hex_and_content_derived() {
        let dir = std::env::temp_dir().join("reimuplayer_media_persist");
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.bin");
        let b = dir.join("b.bin");
        std::fs::write(&a, b"same bytes").unwrap();
        std::fs::write(&b, b"different bytes").unwrap();
        let ha = persist_id(&a).unwrap();
        assert_eq!(ha.len(), 32, "MD5 摘要恒 32 位十六进制");
        assert!(ha.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(ha, persist_id(&a).unwrap(), "同一文件必须稳定");
        assert_ne!(ha, persist_id(&b).unwrap());
        assert!(persist_id(&dir.join("nope.bin")).is_none(), "读不到不编造");
    }

    #[test]
    fn persist_id_only_reads_the_head_of_a_big_file() {
        // 窗口之外的内容不参与摘要：这是"不把整首 FLAC 压在单线程桥上"的代价，
        // 用测试钉住，免得日后有人以为它是完整文件指纹。
        let dir = std::env::temp_dir().join("reimuplayer_media_persist");
        std::fs::create_dir_all(&dir).unwrap();
        let head = vec![7u8; (PERSIST_ID_WINDOW + 8192) as usize];
        let a = dir.join("big_a.bin");
        let b = dir.join("big_b.bin");
        std::fs::write(&a, &head).unwrap();
        let mut tail = head.clone();
        let last = tail.len() - 1;
        tail[last] = 99;
        std::fs::write(&b, &tail).unwrap();
        assert_eq!(
            persist_id(&a),
            persist_id(&b),
            "窗口外的差异不该改变 persistId"
        );
    }
}
