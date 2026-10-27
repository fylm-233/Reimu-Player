//! 本地歌单（自建歌单的增删改查与顺序管理）。
//!
//! 为什么需要：Playlist 页目前只能"消费"网易云歌单（贴 ID/链接加载，只读），
//! 本地曲库虽然有索引，却没有组织手段——用户扫了 500 首，只能按路径顺序听。
//!
//! ## 顺序策略（移植自教程 ch34，但落在 JSON 而不是 SQLite）
//!
//! 每条曲目带一个整数 `position`，显示时按它排序：
//! - 追加：`max(position) + STEP`；
//! - 首插：`next - STEP`；中插：`(prev + next) / 2`（**中点插入**，不动其他行）；
//! - 相邻间距小于 2（整数空间用尽）→ 整表按 `(i+1) * STEP` **重排**（rebalance）。
//!
//! 中点插入让"拖动排序"从 O(n) 次 UPDATE 变成 1 次；rebalance 极少发生。

use std::path::{Path, PathBuf};

use crate::music::provider::Track;

use super::persist;

/// 歌单文件名。
pub const PLAYLISTS_FILE: &str = "local_playlists.json";

/// 格式版本。
pub const PLAYLISTS_VERSION: u32 = 1;

/// 位置步长（与教程一致：1000）。
pub const POSITION_STEP: i64 = 1000;

/// 歌单内一条曲目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistEntry {
    pub position: i64,
    pub track: Track,
}

/// 一个本地歌单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalPlaylist {
    pub id: u64,
    pub name: String,
    pub created_at: i64,
    pub entries: Vec<PlaylistEntry>,
}

impl LocalPlaylist {
    /// 按 position 排好序的曲目。
    pub fn tracks(&self) -> Vec<Track> {
        let mut entries = self.entries.clone();
        entries.sort_by_key(|e| e.position);
        entries.into_iter().map(|e| e.track).collect()
    }

    /// 按 position 排好序的索引（拖动排序用）。
    fn sorted_indices(&self) -> Vec<usize> {
        let mut indices: Vec<usize> = (0..self.entries.len()).collect();
        indices.sort_by_key(|i| self.entries[*i].position);
        indices
    }
}

/// 全部本地歌单。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalPlaylists {
    pub lists: Vec<LocalPlaylist>,
    /// 下一个可用 id（从 1 起，删除后不复用——避免"旧卡片指向新歌单"）。
    next_id: u64,
}

/// 中点插入：给定前后邻居的 position，算出新条目该用的位置。
pub fn midpoint(prev: Option<i64>, next: Option<i64>) -> i64 {
    match (prev, next) {
        (None, None) => POSITION_STEP,
        (None, Some(n)) => n - POSITION_STEP,
        (Some(p), None) => p + POSITION_STEP,
        (Some(p), Some(n)) => p + (n - p) / 2,
    }
}

/// 相邻间距是否已经容不下新位置（需要整表重排）。
pub fn needs_rebalance(prev: Option<i64>, next: Option<i64>) -> bool {
    matches!((prev, next), (Some(p), Some(n)) if n - p < 2)
}

/// 整表重排后的位置序列：1000, 2000, 3000 …
pub fn rebalanced_positions(count: usize) -> Vec<i64> {
    (0..count).map(|i| (i as i64 + 1) * POSITION_STEP).collect()
}

impl LocalPlaylists {
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否已有同名歌单（忽略首尾空白与大小写；用于新建时拦截重名）。
    pub fn has_name(&self, name: &str) -> bool {
        let name = name.trim();
        !name.is_empty() && self.lists.iter().any(|l| l.name.eq_ignore_ascii_case(name))
    }

    /// 新建歌单，返回 id。空名按"未命名混音"处理（UI 层也会拦，双保险）。
    pub fn create(&mut self, name: &str, now: i64) -> u64 {
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        let name = name.trim();
        self.lists.push(LocalPlaylist {
            id,
            name: if name.is_empty() {
                "UNTITLED MIX".to_string()
            } else {
                name.to_string()
            },
            created_at: now,
            entries: Vec::new(),
        });
        id
    }

    pub fn get(&self, id: u64) -> Option<&LocalPlaylist> {
        self.lists.iter().find(|l| l.id == id)
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut LocalPlaylist> {
        self.lists.iter_mut().find(|l| l.id == id)
    }

    /// 重命名（空名拒绝）。
    pub fn rename(&mut self, id: u64, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() {
            return false;
        }
        match self.get_mut(id) {
            Some(list) => {
                list.name = name.to_string();
                true
            }
            None => false,
        }
    }

    /// 删除歌单。
    pub fn delete(&mut self, id: u64) -> bool {
        let before = self.lists.len();
        self.lists.retain(|l| l.id != id);
        self.lists.len() != before
    }

    /// 加曲目（同 provider+id 已在歌单里则拒绝，返回 `false`）。
    pub fn add_track(&mut self, id: u64, track: &Track) -> bool {
        let Some(list) = self.get_mut(id) else {
            return false;
        };
        if list
            .entries
            .iter()
            .any(|e| e.track.provider == track.provider && e.track.id == track.id)
        {
            return false;
        }
        let max = list.entries.iter().map(|e| e.position).max();
        list.entries.push(PlaylistEntry {
            position: midpoint(max, None),
            track: track.clone(),
        });
        true
    }

    /// 移除曲目。
    pub fn remove_track(&mut self, id: u64, track: &Track) -> bool {
        let Some(list) = self.get_mut(id) else {
            return false;
        };
        let before = list.entries.len();
        list.entries
            .retain(|e| !(e.track.provider == track.provider && e.track.id == track.id));
        list.entries.len() != before
    }

    /// 把排序中的第 `from` 条移动到第 `to` 位（0 基，按显示顺序）。
    ///
    /// 位置用中点插入；间距不足时整表重排。越界/原地移动返回 `false`。
    pub fn move_entry(&mut self, id: u64, from: usize, to: usize) -> bool {
        let Some(list) = self.get_mut(id) else {
            return false;
        };
        let order = list.sorted_indices();
        if from >= order.len() || to >= order.len() || from == to {
            return false;
        }
        // 按显示顺序取出实体，重排后再写回
        let mut entries: Vec<PlaylistEntry> =
            order.iter().map(|i| list.entries[*i].clone()).collect();
        let moved = entries.remove(from);
        entries.insert(to, moved);

        let prev = if to == 0 {
            None
        } else {
            Some(entries[to - 1].position)
        };
        let next = entries.get(to + 1).map(|e| e.position);
        if needs_rebalance(prev, next) {
            for (i, entry) in entries.iter_mut().enumerate() {
                entry.position = (i as i64 + 1) * POSITION_STEP;
            }
        } else {
            entries[to].position = midpoint(prev, next);
        }
        list.entries = entries;
        true
    }

    pub fn to_json(&self) -> String {
        let lists: Vec<serde_json::Value> = self
            .lists
            .iter()
            .map(|list| {
                let entries: Vec<serde_json::Value> = list
                    .entries
                    .iter()
                    .map(|e| {
                        let mut value = persist::track_to_json(&e.track);
                        value
                            .as_object_mut()
                            .expect("track_to_json 恒为对象")
                            .insert("position".into(), serde_json::json!(e.position));
                        value
                    })
                    .collect();
                serde_json::json!({
                    "id": list.id,
                    "name": list.name,
                    "created_at": list.created_at,
                    "entries": entries,
                })
            })
            .collect();
        serde_json::json!({
            "version": PLAYLISTS_VERSION,
            "next_id": self.next_id,
            "lists": lists,
        })
        .to_string()
    }

    pub fn from_json(text: &str) -> Option<Self> {
        let root: serde_json::Value = serde_json::from_str(text).ok()?;
        if root.get("version")?.as_u64()? != PLAYLISTS_VERSION as u64 {
            tracing::info!("本地歌单版本不匹配，按空歌单处理");
            return None;
        }
        let mut lists = Vec::new();
        let mut max_id = 0u64;
        for item in root.get("lists")?.as_array()? {
            let Some(id) = item.get("id").and_then(|v| v.as_u64()) else {
                continue;
            };
            max_id = max_id.max(id);
            let mut entries = Vec::new();
            for raw in item
                .get("entries")
                .and_then(|v| v.as_array())
                .map(Vec::as_slice)
                .unwrap_or(&[])
            {
                let Some(track) = persist::track_from_json(raw) else {
                    continue;
                };
                entries.push(PlaylistEntry {
                    position: raw.get("position").and_then(|v| v.as_i64()).unwrap_or(0),
                    track,
                });
            }
            lists.push(LocalPlaylist {
                id,
                name: item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("UNTITLED MIX")
                    .to_string(),
                created_at: item.get("created_at").and_then(|v| v.as_i64()).unwrap_or(0),
                entries,
            });
        }
        let next_id = root
            .get("next_id")
            .and_then(|v| v.as_u64())
            .unwrap_or(max_id + 1)
            .max(max_id + 1);
        Some(Self { lists, next_id })
    }

    pub fn load_from(dir: &Path) -> Self {
        persist::read_json(&dir.join(PLAYLISTS_FILE))
            .and_then(|v| Self::from_json(&v.to_string()))
            .unwrap_or_default()
    }

    pub fn save_to(&self, dir: &Path) -> std::io::Result<()> {
        persist::write_atomic(&dir.join(PLAYLISTS_FILE), &self.to_json())
    }

    pub fn path() -> Option<PathBuf> {
        super::library::config_dir().map(|d| d.join(PLAYLISTS_FILE))
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
            tracing::warn!("配置目录不可得，本地歌单不持久化");
            return;
        };
        let Some(dir) = path.parent() else { return };
        if let Err(e) = self.save_to(dir) {
            tracing::warn!("本地歌单保存失败（不影响播放）：{e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::music::provider::ProviderId;

    fn track(id: &str) -> Track {
        Track {
            provider: ProviderId::Local,
            id: format!("D:/Music/{id}.flac"),
            title: id.to_string(),
            artists: vec!["—".to_string()],
            album: None,
            duration_ms: None,
            cover_url: None,
            ..Track::default()
        }
    }

    fn seeded() -> (LocalPlaylists, u64) {
        let mut lists = LocalPlaylists::new();
        let id = lists.create("Mix", 1);
        for name in ["a", "b", "c"] {
            assert!(lists.add_track(id, &track(name)));
        }
        (lists, id)
    }

    fn order(lists: &LocalPlaylists, id: u64) -> Vec<String> {
        lists
            .get(id)
            .unwrap()
            .tracks()
            .into_iter()
            .map(|t| t.title)
            .collect()
    }

    #[test]
    fn midpoint_covers_first_middle_last() {
        assert_eq!(midpoint(None, None), POSITION_STEP);
        assert_eq!(midpoint(None, Some(1000)), 0);
        assert_eq!(midpoint(Some(1000), None), 2000);
        assert_eq!(midpoint(Some(1000), Some(2000)), 1500);
        // 整数除法向下取整，不会与邻居重合
        assert_eq!(midpoint(Some(10), Some(11)), 10);
        assert!(needs_rebalance(Some(10), Some(11)));
        assert!(!needs_rebalance(Some(1000), Some(2000)));
        assert_eq!(rebalanced_positions(3), vec![1000, 2000, 3000]);
    }

    #[test]
    fn create_rename_delete() {
        let mut lists = LocalPlaylists::new();
        let id = lists.create("  Mix  ", 1);
        assert_eq!(lists.get(id).unwrap().name, "Mix");
        assert!(lists.rename(id, "New"));
        assert!(!lists.rename(id, "   "), "空名必须拒绝");
        assert_eq!(lists.get(id).unwrap().name, "New");
        assert!(lists.delete(id));
        assert!(!lists.delete(id));
        // id 不复用
        let next = lists.create("X", 2);
        assert!(next > id);
    }

    #[test]
    fn add_rejects_duplicates_and_appends_at_end() {
        let (mut lists, id) = seeded();
        assert_eq!(order(&lists, id), vec!["a", "b", "c"]);
        assert!(!lists.add_track(id, &track("a")), "重复曲目必须拒绝");
        assert!(lists.add_track(id, &track("d")));
        assert_eq!(order(&lists, id), vec!["a", "b", "c", "d"]);
        let positions: Vec<i64> = {
            let mut entries = lists.get(id).unwrap().entries.clone();
            entries.sort_by_key(|e| e.position);
            entries.iter().map(|e| e.position).collect()
        };
        assert_eq!(positions, vec![1000, 2000, 3000, 4000]);
    }

    #[test]
    fn remove_track_works_by_identity() {
        let (mut lists, id) = seeded();
        assert!(lists.remove_track(id, &track("b")));
        assert_eq!(order(&lists, id), vec!["a", "c"]);
        assert!(!lists.remove_track(id, &track("b")));
    }

    #[test]
    fn move_entry_reorders_and_keeps_midpoint_semantics() {
        let (mut lists, id) = seeded();
        // a b c → 把 c 拖到最前
        assert!(lists.move_entry(id, 2, 0));
        assert_eq!(order(&lists, id), vec!["c", "a", "b"]);
        // 再拖回末尾
        assert!(lists.move_entry(id, 0, 2));
        assert_eq!(order(&lists, id), vec!["a", "b", "c"]);
        // 原地/越界是 no-op
        assert!(!lists.move_entry(id, 1, 1));
        assert!(!lists.move_entry(id, 0, 9));
    }

    #[test]
    fn move_into_tight_gap_triggers_rebalance() {
        let mut lists = LocalPlaylists::new();
        let id = lists.create("Tight", 1);
        for name in ["a", "b", "c"] {
            lists.add_track(id, &track(name));
        }
        // 手工制造紧邻间距（模拟历史数据被反复中点插入挤压）
        {
            let list = lists.get_mut(id).unwrap();
            for (i, entry) in list.entries.iter_mut().enumerate() {
                entry.position = i as i64 + 1; // 1,2,3
            }
        }
        assert!(lists.move_entry(id, 2, 1));
        assert_eq!(order(&lists, id), vec!["a", "c", "b"]);
        // rebalance 后恢复 1000 步长
        let mut entries = lists.get(id).unwrap().entries.clone();
        entries.sort_by_key(|e| e.position);
        assert_eq!(
            entries.iter().map(|e| e.position).collect::<Vec<_>>(),
            vec![1000, 2000, 3000]
        );
    }

    #[test]
    fn json_round_trip_preserves_order_and_next_id() {
        let (lists, _) = seeded();
        let text = lists.to_json();
        let back = LocalPlaylists::from_json(&text).expect("应可解析");
        assert_eq!(back, lists);
        // 版本守卫
        assert_eq!(LocalPlaylists::from_json("{\"version\":9}"), None);
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("reimu-player-pl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (lists, _) = seeded();
        lists.save_to(&dir).unwrap();
        assert_eq!(LocalPlaylists::load_from(&dir), lists);
        assert!(!dir.join(format!("{PLAYLISTS_FILE}.tmp")).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_entries_are_skipped() {
        let text = r#"{"version":1,"next_id":2,"lists":[
            {"id":1,"name":"Mix","created_at":0,"entries":[
                {"provider":"local","id":"D:/x.flac","title":"x","position":1000},
                {"provider":"local","title":"no id","position":2000}
            ]}
        ]}"#;
        let lists = LocalPlaylists::from_json(text).unwrap();
        assert_eq!(lists.get(1).unwrap().entries.len(), 1);
    }
}
