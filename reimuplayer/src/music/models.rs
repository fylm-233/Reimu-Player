//! 领域模型：媒体描述、播放状态与进度。
//!
//! 设计约束：
//! - `DESIGN_SPEC.md` 15.3 —— 进度必须以音频输出时钟为准，不得用定时器累加。
//! - `NETEASE_INTEGRATION_SPEC.md` 6.2 —— 统一歌曲模型；本地路径与在线 ID 不共用语义字段；
//!   时长、封面允许未知，不伪造。

use std::path::PathBuf;
use std::time::Duration;

/// 音频容器/编码格式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioFormat {
    Mp3,
    Flac,
    Wav,
    Ogg,
    M4a,
    /// 未知或暂未支持的格式，保留原始描述便于诊断。
    Unknown(String),
}

impl AudioFormat {
    /// 由文件扩展名推断。仅用于预判与提示，权威判断由解码器决定。
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_ascii_lowercase().as_str() {
            "mp3" => AudioFormat::Mp3,
            "flac" => AudioFormat::Flac,
            "wav" | "wave" => AudioFormat::Wav,
            "ogg" | "oga" => AudioFormat::Ogg,
            "m4a" | "mp4" | "aac" => AudioFormat::M4a,
            other => AudioFormat::Unknown(other.to_string()),
        }
    }
}

impl std::fmt::Display for AudioFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioFormat::Mp3 => write!(f, "MP3"),
            AudioFormat::Flac => write!(f, "FLAC"),
            AudioFormat::Wav => write!(f, "WAV"),
            AudioFormat::Ogg => write!(f, "OGG"),
            AudioFormat::M4a => write!(f, "M4A"),
            AudioFormat::Unknown(s) => write!(f, "未知({})", s),
        }
    }
}

/// 内容标签。全部允许缺失——不得为凑数而伪造。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
}

/// 音频流参数（音质）。
///
/// 每个字段都是 `Option`：**接口/标签没给就不显示**，不猜、不补默认值。
/// `codec` / `quality` 保留原始取值（如 `flac` / `lossless`），
/// 大小写与措辞的转换只发生在展示层。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioSpec {
    /// 容器/编码标记（本地取扩展名，在线取接口 `type`）。
    pub codec: Option<String>,
    /// 接口音质等级原值（`standard`/`higher`/`exhigh`/`lossless`/`hires`…）。
    ///
    /// 仅在线曲目有；本地文件没有这个概念，保持 `None`。
    pub quality: Option<String>,
    /// 采样率（Hz）。
    pub sample_rate: Option<u32>,
    /// 位深（bit）。
    pub bit_depth: Option<u8>,
    /// 声道数。
    pub channels: Option<u8>,
    /// 音频码率（kbps）。
    pub bitrate_kbps: Option<u32>,
}

impl AudioSpec {
    /// 是否完全没有可用信息。
    pub fn is_empty(&self) -> bool {
        self.codec.is_none()
            && self.quality.is_none()
            && self.sample_rate.is_none()
            && self.bit_depth.is_none()
            && self.channels.is_none()
            && self.bitrate_kbps.is_none()
    }

    /// 展示用音质描述，如 `LOSSLESS · FLAC · 44.1 kHz · 16 bit · 2ch · 1411 kbps`。
    ///
    /// 无任何信息时返回空串，由 UI 显示占位符（不返回 `"未知"` 之类的假值）。
    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(q) = &self.quality {
            parts.push(q.to_uppercase());
        }
        if let Some(c) = &self.codec {
            parts.push(c.to_uppercase());
        }
        if let Some(rate) = self.sample_rate {
            parts.push(format_khz(rate));
        }
        if let Some(depth) = self.bit_depth {
            parts.push(format!("{} bit", depth));
        }
        if let Some(ch) = self.channels {
            parts.push(format!("{}ch", ch));
        }
        if let Some(kbps) = self.bitrate_kbps {
            parts.push(format!("{} kbps", kbps));
        }
        parts.join(" · ")
    }
}

/// 采样率展示：整数千赫去掉小数位（`48 kHz`），否则保留一位（`44.1 kHz`）。
fn format_khz(rate: u32) -> String {
    let k = rate as f64 / 1000.0;
    if (k - k.round()).abs() < 1e-9 {
        format!("{} kHz", k.round() as u32)
    } else {
        format!("{:.1} kHz", k)
    }
}

/// 本地媒体文件的描述。
///
/// 这是「媒体加载」的产物，可在**没有音频输出设备**的情况下获得，
/// 因此可用于列表展示、排序与测试。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaInfo {
    pub path: PathBuf,
    pub format: AudioFormat,
    /// 来自元数据的时长估计；未知则为 `None`，不伪造。
    pub duration: Option<Duration>,
    pub tags: Tags,
    /// 文件字节数，用于 UI 展示与异常诊断。
    pub file_size: u64,
    /// 音频流参数（采样率/位深/码率/声道），供「音质」展示。
    pub audio: AudioSpec,
}

impl MediaInfo {
    /// 展示用标题：优先标签，缺失时回退到文件名。
    ///
    /// 只影响显示，不修改 `tags` 本身——真实标签值不被伪造覆盖。
    pub fn display_title(&self) -> String {
        self.tags
            .title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| {
                self.path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| self.path.to_string_lossy().into_owned())
            })
    }

    /// 展示用艺人；未知时返回「未知艺人」，同样不写回 `tags`。
    pub fn display_artist(&self) -> String {
        self.tags
            .artist
            .clone()
            .filter(|a| !a.trim().is_empty())
            .unwrap_or_else(|| "未知艺人".to_string())
    }
}

/// 播放状态机。
///
/// 合法迁移：
/// ```text
///   Idle ──load──▶ Ready ──play──▶ Playing ──pause──▶ Paused
///     ▲              │                 │  ▲              │
///     │              │                 │  └────play──────┘
///     │              ▼                 ▼
///     └────clear── Ended ◀──播放自然结束──┘
///                    │
///                    └──(任意操作失败)──▶ Failed
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackState {
    /// 尚未加载任何媒体。
    Idle,
    /// 已加载、可播放，当前处于停止/待播。
    Ready,
    Playing,
    Paused,
    /// 播放自然结束。
    Ended,
    /// 出错。携带可读原因，UI 直接展示即可。
    Failed(String),
}

impl PlaybackState {
    /// 状态名，用于日志与错误提示。
    pub fn name(&self) -> &'static str {
        match self {
            PlaybackState::Idle => "Idle",
            PlaybackState::Ready => "Ready",
            PlaybackState::Playing => "Playing",
            PlaybackState::Paused => "Paused",
            PlaybackState::Ended => "Ended",
            PlaybackState::Failed(_) => "Failed",
        }
    }

    /// 是否处于可播放/播放中的活跃态。
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            PlaybackState::Ready | PlaybackState::Playing | PlaybackState::Paused
        )
    }
}

impl std::fmt::Display for PlaybackState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlaybackState::Failed(reason) => write!(f, "Failed({})", reason),
            other => write!(f, "{}", other.name()),
        }
    }
}

/// 播放进度快照。
///
/// `position` 来自**音频输出时钟**（见 [`crate::audio::AudioEngine::position`]），
/// 不是定时器累加值。这直接落实 `DESIGN_SPEC.md` 15.3 的防漂移要求。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// 当前播放位置。
    pub position: Duration,
    /// 总时长；解码器无法确定时为 `None`。
    pub total: Option<Duration>,
}

impl Progress {
    /// 完成度百分比（0–100）。
    ///
    /// 总时长未知或为 0 时返回 `None`，**不返回伪造的 0**。
    pub fn percent(&self) -> Option<f32> {
        let total = self.total?.as_secs_f32();
        if total <= 0.0 {
            return None;
        }
        Some((self.position.as_secs_f32() / total * 100.0).clamp(0.0, 100.0))
    }
}

impl std::fmt::Display for Progress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.total {
            Some(total) => write!(
                f,
                "{} / {}",
                format_duration(self.position),
                format_duration(total)
            ),
            None => write!(f, "{} / --:--", format_duration(self.position)),
        }
    }
}

/// 把时长格式化为 `mm:ss`。
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_is_mmss() {
        assert_eq!(format_duration(Duration::from_secs(0)), "00:00");
        assert_eq!(format_duration(Duration::from_secs(65)), "01:05");
        assert_eq!(format_duration(Duration::from_secs(3260)), "54:20");
    }

    #[test]
    fn progress_percent_requires_known_total() {
        let p = Progress {
            position: Duration::from_secs(50),
            total: Some(Duration::from_secs(200)),
        };
        assert_eq!(p.percent(), Some(25.0));

        let unknown = Progress {
            position: Duration::from_secs(50),
            total: None,
        };
        assert_eq!(unknown.percent(), None);

        let zero = Progress {
            position: Duration::from_secs(1),
            total: Some(Duration::ZERO),
        };
        assert_eq!(zero.percent(), None);
    }

    #[test]
    fn percent_is_clamped_to_100() {
        let p = Progress {
            position: Duration::from_secs(300),
            total: Some(Duration::from_secs(100)),
        };
        assert_eq!(p.percent(), Some(100.0));
    }

    #[test]
    fn display_title_falls_back_to_file_stem() {
        let m = MediaInfo {
            path: PathBuf::from("music/晴天.mp3"),
            format: AudioFormat::Mp3,
            duration: None,
            tags: Tags::default(),
            file_size: 0,
            audio: AudioSpec::default(),
        };
        assert_eq!(m.display_title(), "晴天");
        assert_eq!(m.display_artist(), "未知艺人");

        // 标签存在时优先使用标签
        let tagged = MediaInfo {
            tags: Tags {
                title: Some("晴天".into()),
                artist: Some("周杰伦".into()),
                album: Some("叶惠美".into()),
            },
            ..m
        };
        assert_eq!(tagged.display_title(), "晴天");
        assert_eq!(tagged.display_artist(), "周杰伦");
    }

    #[test]
    fn empty_tag_falls_back_rather_than_displaying_blank() {
        let m = MediaInfo {
            path: PathBuf::from("a.mp3"),
            format: AudioFormat::Mp3,
            duration: None,
            tags: Tags {
                title: Some("   ".into()),
                artist: Some("".into()),
                album: None,
            },
            file_size: 0,
            audio: AudioSpec::default(),
        };
        assert_eq!(m.display_title(), "a");
        assert_eq!(m.display_artist(), "未知艺人");
    }

    #[test]
    fn audio_spec_describe_lists_only_known_fields() {
        let empty = AudioSpec::default();
        assert!(empty.is_empty());
        assert_eq!(empty.describe(), "", "全空时不得伪造任何描述");

        let full = AudioSpec {
            codec: Some("flac".into()),
            quality: Some("lossless".into()),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            channels: Some(2),
            bitrate_kbps: Some(1411),
        };
        assert!(!full.is_empty());
        assert_eq!(
            full.describe(),
            "LOSSLESS · FLAC · 44.1 kHz · 16 bit · 2ch · 1411 kbps"
        );

        // 部分字段缺失时只显示已知项，顺序不变
        let partial = AudioSpec {
            quality: Some("exhigh".into()),
            bitrate_kbps: Some(320),
            ..AudioSpec::default()
        };
        assert_eq!(partial.describe(), "EXHIGH · 320 kbps");
    }

    #[test]
    fn audio_spec_formats_integer_khz_without_decimals() {
        let spec = |rate| AudioSpec {
            sample_rate: Some(rate),
            ..AudioSpec::default()
        };
        assert_eq!(spec(48_000).describe(), "48 kHz");
        assert_eq!(spec(44_100).describe(), "44.1 kHz");
        assert_eq!(spec(22_050).describe(), "22.1 kHz");
    }

    #[test]
    fn format_from_extension_covers_expected_and_unknown() {
        assert_eq!(AudioFormat::from_extension("MP3"), AudioFormat::Mp3);
        assert_eq!(AudioFormat::from_extension("flac"), AudioFormat::Flac);
        assert_eq!(AudioFormat::from_extension("wav"), AudioFormat::Wav);
        assert_eq!(AudioFormat::from_extension("ogg"), AudioFormat::Ogg);
        assert_eq!(AudioFormat::from_extension("m4a"), AudioFormat::M4a);
        assert_eq!(
            AudioFormat::from_extension("xyz"),
            AudioFormat::Unknown("xyz".into())
        );
    }

    #[test]
    fn state_helpers() {
        assert!(PlaybackState::Playing.is_active());
        assert!(PlaybackState::Ready.is_active());
        assert!(!PlaybackState::Idle.is_active());
        assert!(!PlaybackState::Ended.is_active());
        assert_eq!(PlaybackState::Failed("x".into()).name(), "Failed");
    }
}
