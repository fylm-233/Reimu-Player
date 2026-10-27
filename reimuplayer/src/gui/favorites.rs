//! 本地收藏（"我的收藏"的数据层）。
//!
//! 为什么先做**纯本地**收藏：网易云的红心接口需要登录态，而收藏是播放器最基础
//! 的动作之一——不该因为没登录就不能用。登录后的云端红心同步属于增强项，
//! 且要先确认 SDK 是否暴露对应端点；在确认之前不做"看起来同步了其实没有"的假象。
//!
//! 判定键是 `(provider, id)`：本地曲目 id 是文件路径，在线曲目 id 是歌曲 ID，
//! 两者不会互相污染。存曲目快照的理由与播放历史相同（见 [`super::history`]）。

use std::path::{Path, PathBuf};

use crate::music::provider::Track;

use super::persist;

/// 收藏文件名。
pub const FAVORITES_FILE: &str = "favorites.json";

/// 格式版本。
pub const FAVORITES_VERSION: u32 = 1;

/// 一条收藏。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Favorite {
    pub track: Track,
    /// 收藏时间（Unix 秒）。
    pub added_at: i64,
}

/// 收藏夹（新收藏在前）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Favorites {
    pub entries: Vec<Favorite>,
}

impl Favorites {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 是否已收藏（按 provider + id 判定，不比较其余字段）。
    pub fn contains(&self, track: &Track) -> bool {
        self.entries
            .iter()
            .any(|f| f.track.provider == track.provider && f.track.id == track.id)
    }

    /// 切换收藏状态，返回切换后是否已收藏。
    ///
    /// 已收藏 → 移除；未收藏 → 插到最前（最新收藏优先展示）。
    pub fn toggle(&mut self, track: &Track, now: i64) -> bool {
        if self.contains(track) {
            self.entries
                .retain(|f| !(f.track.provider == track.provider && f.track.id == track.id));
            false
        } else {
            self.entries.insert(
                0,
                Favorite {
                    track: track.clone(),
                    added_at: now,
                },
            );
            true
        }
    }

    /// 收藏（已存在则不动），返回是否真的新增。
    ///
    /// 与 [`Self::toggle`] 分开：收藏双向同步是"把云端有的补到本地"，
    /// 用 toggle 会在竞态下把刚补进来的又删掉（点两次就没了）。
    pub fn insert_if_absent(&mut self, track: &Track, now: i64) -> bool {
        if self.contains(track) {
            return false;
        }
        self.entries.insert(
            0,
            Favorite {
                track: track.clone(),
                added_at: now,
            },
        );
        true
    }

    /// 收藏的曲目列表（供"我的收藏"列表直接当播放队列）。
    pub fn tracks(&self) -> Vec<Track> {
        self.entries.iter().map(|f| f.track.clone()).collect()
    }

    pub fn to_json(&self) -> String {
        let entries: Vec<serde_json::Value> = self
            .entries
            .iter()
            .map(|f| {
                let mut value = persist::track_to_json(&f.track);
                value
                    .as_object_mut()
                    .expect("track_to_json 恒为对象")
                    .insert("added_at".into(), serde_json::json!(f.added_at));
                value
            })
            .collect();
        serde_json::json!({
            "version": FAVORITES_VERSION,
            "entries": entries,
        })
        .to_string()
    }

    pub fn from_json(text: &str) -> Option<Self> {
        let root: serde_json::Value = serde_json::from_str(text).ok()?;
        if root.get("version")?.as_u64()? != FAVORITES_VERSION as u64 {
            tracing::info!("收藏文件版本不匹配，按空收藏处理");
            return None;
        }
        let mut entries = Vec::new();
        for item in root.get("entries")?.as_array()? {
            let Some(track) = persist::track_from_json(item) else {
                continue;
            };
            entries.push(Favorite {
                track,
                added_at: item.get("added_at").and_then(|v| v.as_i64()).unwrap_or(0),
            });
        }
        Some(Self { entries })
    }

    pub fn load_from(dir: &Path) -> Self {
        persist::read_json(&dir.join(FAVORITES_FILE))
            .and_then(|v| Self::from_json(&v.to_string()))
            .unwrap_or_default()
    }

    pub fn save_to(&self, dir: &Path) -> std::io::Result<()> {
        persist::write_atomic(&dir.join(FAVORITES_FILE), &self.to_json())
    }

    pub fn path() -> Option<PathBuf> {
        super::library::config_dir().map(|d| d.join(FAVORITES_FILE))
    }

    pub fn load() -> Self {
        Self::path()
            .as_deref()
            .and_then(|p| p.parent())
            .map(Self::load_from)
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(path) = Self::path() else {
            tracing::warn!("配置目录不可得，收藏不持久化");
            return;
        };
        let Some(dir) = path.parent() else { return };
        if let Err(e) = self.save_to(dir) {
            tracing::warn!("收藏保存失败（不影响播放）：{e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::music::provider::ProviderId;

    fn track(id: &str) -> Track {
        Track {
            provider: ProviderId::Netease,
            id: id.to_string(),
            title: format!("T{id}"),
            artists: vec!["A".to_string()],
            album: None,
            duration_ms: Some(100_000),
            cover_url: None,
            ..Track::default()
        }
    }

    #[test]
    fn toggle_adds_then_removes() {
        let mut favorites = Favorites::new();
        assert!(!favorites.contains(&track("1")));
        assert!(favorites.toggle(&track("1"), 100));
        assert!(favorites.contains(&track("1")));
        assert_eq!(favorites.len(), 1);
        assert!(!favorites.toggle(&track("1"), 200));
        assert!(!favorites.contains(&track("1")));
        assert!(favorites.is_empty());
    }

    #[test]
    fn newest_favorite_comes_first() {
        let mut favorites = Favorites::new();
        favorites.toggle(&track("1"), 100);
        favorites.toggle(&track("2"), 200);
        assert_eq!(favorites.tracks()[0].id, "2");
    }

    #[test]
    fn same_id_different_provider_are_distinct() {
        let mut favorites = Favorites::new();
        favorites.toggle(&track("1"), 100);
        let mut local = track("1");
        local.provider = ProviderId::Local;
        assert!(!favorites.contains(&local));
        favorites.toggle(&local, 200);
        assert_eq!(favorites.len(), 2);
    }

    #[test]
    fn json_round_trip_and_version_guard() {
        let mut favorites = Favorites::new();
        favorites.toggle(&track("9"), 1_700_000_000);
        let text = favorites.to_json();
        assert_eq!(Favorites::from_json(&text), Some(favorites));
        assert_eq!(
            Favorites::from_json("{\"version\": 2, \"entries\": []}"),
            None
        );
        assert_eq!(Favorites::from_json("[]"), None);
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("reimu-player-fav-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut favorites = Favorites::new();
        favorites.toggle(&track("3"), 42);
        favorites.save_to(&dir).unwrap();
        assert_eq!(Favorites::load_from(&dir), favorites);
        assert!(!dir.join(format!("{FAVORITES_FILE}.tmp")).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_empty_not_error() {
        let dir =
            std::env::temp_dir().join(format!("reimu-player-fav-miss-{}", std::process::id()));
        assert!(Favorites::load_from(&dir).is_empty());
    }
}
