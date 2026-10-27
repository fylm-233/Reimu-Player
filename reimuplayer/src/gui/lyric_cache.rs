//! 歌词缓存（跨曲目的轻量内存缓存，支撑「下一首预缓存」）。
//!
//! 现状（预缓存改造前）：每次 [`crate::gui::player_core::PlayerCore::play_online`]
//! 都无条件发一次歌词请求，切歌即重发；没有跨曲目缓存，也无法「提前把下一首的
//! 歌词准备好」。
//!
//! 本模块只做**纯内存缓存**：键 = 歌曲 ID（本应用只有网易云一个在线 Provider，
//! ID 已全局唯一，故不加 provider 前缀；将来多 Provider 时再扩展键结构）。
//! 只缓存**成功取到**的歌词；`Err(EmptyResult)`（纯音乐）不缓存——命不中的歌
//! 播放时仍会走正常网络路径，不会因此改变「暂无歌词」的提示语义。
//! 容量用上限保护，防止长时间漫游无界增长。
//!
//! 纯数据结构、无 IO、无线程，可直接单测。

use std::collections::HashMap;

use crate::music::lyrics::Lyrics;

/// 缓存条目：`Some` = 有歌词；`None` = 已确认无歌词（不重试）。
#[derive(Debug, Clone)]
pub struct LyricsCache {
    entries: HashMap<String, Lyrics>,
}

/// 缓存上限（与 [`super::app::COVER_CACHE_LIMIT`] 同思路，防无界增长）。
const CACHE_LIMIT: usize = 64;

impl Default for LyricsCache {
    fn default() -> Self {
        Self::new()
    }
}

impl LyricsCache {
    /// 空缓存。
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// 命中返回歌词克隆；未命中返回 `None`。
    pub fn get(&self, id: &str) -> Option<&Lyrics> {
        self.entries.get(id)
    }

    /// 写入一条歌词；触发淘汰时返回被淘汰的键（供诊断，可忽略）。
    pub fn insert(&mut self, id: String, lyrics: Lyrics) -> Option<String> {
        let mut evicted = None;
        if self.entries.len() >= CACHE_LIMIT && !self.entries.contains_key(&id) {
            // 淘汰最先插入的键（近似 FIFO/LRU；本应用体量下足够）
            let oldest = self.entries.keys().next().cloned();
            if let Some(key) = oldest {
                self.entries.remove(&key);
                evicted = Some(key);
            }
        }
        self.entries.insert(id, lyrics);
        evicted
    }

    /// 是否已缓存该 ID（含「已确认无歌词」）。
    pub fn contains(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    /// 当前缓存条目数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::music::lyrics::{LyricKind, LyricLine};

    fn lyric(id: &str) -> Lyrics {
        Lyrics {
            kind: LyricKind::Plain,
            lines: vec![LyricLine {
                start_ms: 0,
                text: id.to_string(),
            }],
            offset_ms: 0,
            dropped_lines: 0,
        }
    }

    #[test]
    fn get_returns_none_before_insert() {
        let cache = LyricsCache::new();
        assert!(cache.get("1").is_none());
    }

    #[test]
    fn insert_then_get_round_trips() {
        let mut cache = LyricsCache::new();
        cache.insert("1".into(), lyric("第一行"));
        assert_eq!(cache.get("1").unwrap().lines[0].text, "第一行");
    }

    #[test]
    fn cache_has_bounded_growth() {
        let mut cache = LyricsCache::new();
        for i in 0..(CACHE_LIMIT + 20) {
            cache.insert(i.to_string(), lyric(&i.to_string()));
        }
        assert!(cache.len() <= CACHE_LIMIT, "缓存不得无界增长");
    }
}
