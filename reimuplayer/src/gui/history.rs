//! 播放历史（"最近播放"的数据层）。
//!
//! 设计取舍：
//!
//! - **记录时机在"听够"之后**，不在装载成功时。切歌就走的曲目不该出现在
//!   最近播放里——那会让列表变成"最近点过"，与用户对"听过"的直觉不符。
//!   判定规则见 [`qualifies`]：播放 ≥30 秒，或达到全曲一半。
//! - **保留多次播放**（不去重）：推荐算法要靠"某艺人被听了 N 次"判断偏好；
//!   展示层用 [`PlayHistory::recent`] 去重（同一首只出现一次，取最近那次）。
//! - **存曲目快照**：曲库重扫后文件路径可能变、在线曲目可能下架，
//!   但历史里仍应显示当时的标题。
//!
//! 落盘：`%APPDATA%\ReimuPlayer\play_history.json`（与 prefs/曲库索引同一目录）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::music::provider::{ProviderId, Track};

use super::persist;

/// 历史文件名。
pub const HISTORY_FILE: &str = "play_history.json";

/// 格式版本（语义变化时 +1）。
pub const HISTORY_VERSION: u32 = 1;

/// 条目上限（FIFO）。200 条足够覆盖 30 天窗口，又不至于让 JSON 膨胀。
pub const MAX_ENTRIES: usize = 200;

/// "算一次播放"的时长门槛（毫秒）。
pub const QUALIFY_MS: u64 = 30_000;

/// 一次播放记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayRecord {
    /// 曲目快照。
    pub track: Track,
    /// 播放发生时间（Unix 秒）。
    pub played_at: i64,
    /// 实际播放时长（毫秒），完播判定的依据。
    pub played_ms: u64,
}

/// 播放历史（新 → 旧）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlayHistory {
    pub entries: Vec<PlayRecord>,
}

impl PlayHistory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 追加一次播放（新记录插到最前，超出上限裁掉最旧的）。
    pub fn record(&mut self, track: &Track, played_ms: u64, now: i64) {
        self.entries.insert(
            0,
            PlayRecord {
                track: track.clone(),
                played_at: now,
                played_ms,
            },
        );
        self.entries.truncate(MAX_ENTRIES);
    }

    /// 最近 `hours` 小时内的播放，**同一首只保留最近一次**。
    pub fn recent(&self, hours: i64, now: i64) -> Vec<&PlayRecord> {
        let since = now - hours.max(0) * 3600;
        let mut seen: HashSet<(ProviderId, &str)> = HashSet::new();
        let mut out = Vec::new();
        for record in self.entries.iter().filter(|r| r.played_at > since) {
            let key = (record.track.provider, record.track.id.as_str());
            if seen.insert(key) {
                out.push(record);
            }
        }
        out
    }

    /// 最近 `days` 天内播放过的曲目集合（推荐算法的"已听"判据）。
    pub fn played_within_days(&self, days: i64, now: i64) -> HashSet<(ProviderId, String)> {
        let since = now - days.max(0) * 86_400;
        self.entries
            .iter()
            .filter(|r| r.played_at > since)
            .map(|r| (r.track.provider, r.track.id.clone()))
            .collect()
    }

    /// 最近 `days` 天内各艺人的播放次数（推荐算法的"偏好"判据）。
    pub fn artist_counts(&self, days: i64, now: i64) -> Vec<(String, usize)> {
        let since = now - days.max(0) * 86_400;
        let mut counts: Vec<(String, usize)> = Vec::new();
        for record in self.entries.iter().filter(|r| r.played_at > since) {
            for artist in &record.track.artists {
                let name = artist.trim();
                if name.is_empty() || name == "—" {
                    continue;
                }
                match counts.iter_mut().find(|(a, _)| a == name) {
                    Some((_, n)) => *n += 1,
                    None => counts.push((name.to_string(), 1)),
                }
            }
        }
        counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        counts
    }

    /// 最近 `days` 天内各专辑的播放次数。
    pub fn album_counts(&self, days: i64, now: i64) -> Vec<(String, usize)> {
        let since = now - days.max(0) * 86_400;
        let mut counts: Vec<(String, usize)> = Vec::new();
        for record in self.entries.iter().filter(|r| r.played_at > since) {
            let Some(album) = record.track.album.as_deref().map(str::trim) else {
                continue;
            };
            if album.is_empty() {
                continue;
            }
            match counts.iter_mut().find(|(a, _)| a == album) {
                Some((_, n)) => *n += 1,
                None => counts.push((album.to_string(), 1)),
            }
        }
        counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        counts
    }

    /// 序列化为 JSON 文本（纯函数）。
    pub fn to_json(&self) -> String {
        let entries: Vec<serde_json::Value> = self
            .entries
            .iter()
            .map(|r| {
                let mut value = persist::track_to_json(&r.track);
                let map = value.as_object_mut().expect("track_to_json 恒为对象");
                map.insert("played_at".into(), serde_json::json!(r.played_at));
                map.insert("played_ms".into(), serde_json::json!(r.played_ms));
                value
            })
            .collect();
        serde_json::json!({
            "version": HISTORY_VERSION,
            "entries": entries,
        })
        .to_string()
    }

    /// 解析 JSON 文本（版本不符/结构损坏 → `None`；单条坏记录跳过）。
    pub fn from_json(text: &str) -> Option<Self> {
        let root: serde_json::Value = serde_json::from_str(text).ok()?;
        if root.get("version")?.as_u64()? != HISTORY_VERSION as u64 {
            tracing::info!("播放历史版本不匹配，按空历史处理");
            return None;
        }
        let mut entries = Vec::new();
        for item in root.get("entries")?.as_array()? {
            let Some(track) = persist::track_from_json(item) else {
                continue;
            };
            let Some(played_at) = item.get("played_at").and_then(|v| v.as_i64()) else {
                continue;
            };
            entries.push(PlayRecord {
                track,
                played_at,
                played_ms: item.get("played_ms").and_then(|v| v.as_u64()).unwrap_or(0),
            });
        }
        entries.truncate(MAX_ENTRIES);
        Some(Self { entries })
    }

    /// 从指定目录读取（不存在/损坏 → 空历史）。
    pub fn load_from(dir: &Path) -> Self {
        persist::read_json(&dir.join(HISTORY_FILE))
            .and_then(|v| Self::from_json(&v.to_string()))
            .unwrap_or_default()
    }

    /// 写入指定目录（原子写）。
    pub fn save_to(&self, dir: &Path) -> std::io::Result<()> {
        persist::write_atomic(&dir.join(HISTORY_FILE), &self.to_json())
    }

    /// 文件路径（配置目录不可得时 `None`）。
    pub fn path() -> Option<PathBuf> {
        super::library::config_dir().map(|d| d.join(HISTORY_FILE))
    }

    /// 从真实配置目录读取。
    pub fn load() -> Self {
        Self::path()
            .as_deref()
            .and_then(|p| p.parent())
            .map(Self::load_from)
            .unwrap_or_default()
    }

    /// 保存到真实配置目录（失败只记日志——历史存不下不该影响播放）。
    pub fn save(&self) {
        let Some(path) = Self::path() else {
            tracing::warn!("配置目录不可得，播放历史不持久化");
            return;
        };
        let Some(dir) = path.parent() else { return };
        if let Err(e) = self.save_to(dir) {
            tracing::warn!("播放历史保存失败（不影响播放）：{e}");
        }
    }
}

/// 这次播放是否够格记进历史。
///
/// 规则（与教程一致）：播放满 30 秒，或达到全曲时长的一半。
/// 两个条件取「或」——短曲（如 40 秒的间奏）听 20 秒就该算一次，
/// 而 10 分钟的长曲听 30 秒同样算一次。
pub fn qualifies(played_ms: u64, duration_ms: Option<u64>) -> bool {
    if played_ms >= QUALIFY_MS {
        return true;
    }
    duration_ms.is_some_and(|total| total > 0 && played_ms.saturating_mul(2) >= total)
}

/// 当前 Unix 秒。
///
/// 单独抽出来是为了让「记录时刻」只有一个来源：历史窗口过滤、收藏时间戳、
/// 离线推荐的日序号都走它，避免各处各写一份 `SystemTime` 换算。
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("reimu-player-hist-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn track(id: &str, artist: &str, album: Option<&str>) -> Track {
        Track {
            provider: ProviderId::Netease,
            id: id.to_string(),
            title: format!("T{id}"),
            artists: vec![artist.to_string()],
            album: album.map(str::to_string),
            duration_ms: Some(200_000),
            cover_url: None,
            ..Track::default()
        }
    }

    #[test]
    fn qualifies_by_duration_or_half_length() {
        assert!(qualifies(30_000, Some(600_000)));
        assert!(!qualifies(29_999, Some(600_000)));
        // 短曲：20 秒即过半
        assert!(qualifies(20_000, Some(40_000)));
        assert!(!qualifies(19_000, Some(40_000)));
        // 时长未知时只能靠 30 秒门槛
        assert!(!qualifies(20_000, None));
        assert!(qualifies(30_000, None));
        // 0 时长不该被"两倍"规则误判
        assert!(!qualifies(0, Some(0)));
    }

    #[test]
    fn record_keeps_newest_first_and_caps() {
        let mut history = PlayHistory::new();
        for i in 0..(MAX_ENTRIES + 5) {
            history.record(&track(&i.to_string(), "A", None), 40_000, 1_000 + i as i64);
        }
        assert_eq!(history.len(), MAX_ENTRIES);
        assert_eq!(history.entries[0].track.id, (MAX_ENTRIES + 4).to_string());
    }

    #[test]
    fn recent_dedupes_and_filters_by_window() {
        let mut history = PlayHistory::new();
        history.record(&track("1", "A", None), 40_000, 1_000);
        history.record(&track("2", "B", None), 40_000, 2_000);
        history.record(&track("1", "A", None), 40_000, 3_000);
        let now = 3_000;
        let recent = history.recent(24, now);
        assert_eq!(recent.len(), 2, "同一首只出现一次");
        assert_eq!(recent[0].track.id, "1", "保留最近一次播放时间");
        // 窗口外过滤
        assert!(history
            .recent(1, now + 7_200)
            .iter()
            .all(|r| r.track.id != "2"));
    }

    #[test]
    fn played_within_days_builds_exclusion_set() {
        let mut history = PlayHistory::new();
        history.record(&track("1", "A", None), 40_000, 1_000);
        let now = 1_000 + 31 * 86_400;
        assert!(history.played_within_days(30, now).is_empty());
        let now = 1_000 + 29 * 86_400;
        assert_eq!(history.played_within_days(30, now).len(), 1);
    }

    #[test]
    fn artist_and_album_counts_rank_by_frequency() {
        let mut history = PlayHistory::new();
        history.record(&track("1", "A", Some("X")), 40_000, 1_000);
        history.record(&track("2", "A", Some("Y")), 40_000, 2_000);
        history.record(&track("3", "B", Some("X")), 40_000, 3_000);
        history.record(&track("4", "A", Some("X")), 40_000, 4_000);
        let now = 4_000;
        let artists = history.artist_counts(30, now);
        assert_eq!(artists[0], ("A".to_string(), 3));
        let albums = history.album_counts(30, now);
        assert_eq!(albums[0], ("X".to_string(), 3));
        // 占位艺人名不算偏好
        let mut h2 = PlayHistory::new();
        h2.record(&track("9", "—", None), 40_000, 1);
        assert!(h2.artist_counts(30, 1).is_empty());
    }

    #[test]
    fn json_round_trips_and_rejects_version_mismatch() {
        let mut history = PlayHistory::new();
        history.record(&track("1", "A", Some("X")), 45_000, 1_700_000_000);
        let text = history.to_json();
        assert_eq!(PlayHistory::from_json(&text), Some(history));
        assert_eq!(
            PlayHistory::from_json("{\"version\": 99, \"entries\": []}"),
            None
        );
        assert_eq!(PlayHistory::from_json("not json"), None);
    }

    #[test]
    fn broken_entry_is_skipped_not_fatal() {
        let text = r#"{"version":1,"entries":[
            {"provider":"netease","id":"1","title":"ok","played_at":10,"played_ms":100},
            {"provider":"netease","title":"no id","played_at":20}
        ]}"#;
        let history = PlayHistory::from_json(text).expect("整体仍可解析");
        assert_eq!(history.len(), 1);
        assert_eq!(history.entries[0].track.id, "1");
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = temp_dir("roundtrip");
        let mut history = PlayHistory::new();
        history.record(&track("7", "A", None), 40_000, 1_000);
        history.save_to(&dir).unwrap();
        assert_eq!(PlayHistory::load_from(&dir), history);
        assert!(!dir.join(format!("{HISTORY_FILE}.tmp")).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_loads_as_empty() {
        assert!(PlayHistory::load_from(&temp_dir("missing")).is_empty());
    }
}
