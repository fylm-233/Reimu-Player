//! 有界播放队列（阶段 D / M4）。
//!
//! 设计约束（`NETEASE_INTEGRATION_SPEC.md` 9.2）：
//! - 「所有缓存必须设置上限」——队列容量**有界**（[`PlayQueue::new`] 指定），
//!   超限新增被拒绝并返回清晰错误，落实「长歌单不导致内存无限增长」。
//! - 「播放列表只存储必要元数据」——队列只持有 [`Track`] 元数据，
//!   不持有响应 JSON、音频数据或播放地址。
//! - 歌单/搜索结果**整页入队时截断到剩余容量**（部分成功 + 明确计数），
//!   单条 `add` 超限则报错拒绝，两种语义分开。

use crate::music::provider::Track;

/// 队列默认容量上限。长歌单分页进入队列也不会无限增长。
pub const DEFAULT_MAX_QUEUE_LEN: usize = 1000;

/// 队列操作错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueError {
    /// 队列已满，单条新增被拒绝。
    Full,
    /// 下标越界（jump / remove）。
    IndexOutOfBounds(usize),
}

impl std::fmt::Display for QueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueueError::Full => write!(f, "播放队列已满，无法继续添加"),
            QueueError::IndexOutOfBounds(i) => write!(f, "队列下标越界: {}", i),
        }
    }
}

impl std::error::Error for QueueError {}

/// 有界播放队列：曲目元数据列表 + 当前位置。
///
/// 内存有界：`len() <= capacity()` 恒成立。
pub struct PlayQueue {
    tracks: Vec<Track>,
    capacity: usize,
    /// 当前曲目下标；空队列为 `None`。
    current: Option<usize>,
}

impl PlayQueue {
    /// 创建指定容量的空队列。容量为 0 时队列不可添加。
    pub fn new(capacity: usize) -> Self {
        Self {
            tracks: Vec::new(),
            capacity,
            current: None,
        }
    }

    /// 默认容量（[`DEFAULT_MAX_QUEUE_LEN`]）的空队列。
    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_MAX_QUEUE_LEN)
    }

    /// 容量上限。
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 当前曲目数。
    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    /// 队列中的曲目元数据（只读）。
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// 当前曲目下标。
    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    /// 当前曲目。
    pub fn current(&self) -> Option<&Track> {
        self.current.map(|i| &self.tracks[i])
    }

    /// 单条新增。队列已满 → [`QueueError::Full`]（明确拒绝，不静默丢弃）。
    pub fn add(&mut self, track: Track) -> Result<(), QueueError> {
        if self.tracks.len() >= self.capacity {
            return Err(QueueError::Full);
        }
        self.tracks.push(track);
        if self.current.is_none() {
            self.current = Some(0);
        }
        Ok(())
    }

    /// 批量新增（搜索结果/歌单分页入队）。**截断到剩余容量**，
    /// 返回实际加入条数——长歌单入队不破坏内存有界性。
    pub fn extend(&mut self, tracks: impl IntoIterator<Item = Track>) -> usize {
        let mut added = 0usize;
        for t in tracks {
            if self.tracks.len() >= self.capacity {
                break;
            }
            self.tracks.push(t);
            added += 1;
        }
        if added > 0 && self.current.is_none() {
            self.current = Some(0);
        }
        added
    }

    /// 移除指定下标曲目。移除当前曲目时当前位置保持在原下标
    /// （即后一首顶上）；移除末尾的当前曲目时回退到新末尾。
    pub fn remove(&mut self, index: usize) -> Result<Track, QueueError> {
        if index >= self.tracks.len() {
            return Err(QueueError::IndexOutOfBounds(index));
        }
        let removed = self.tracks.remove(index);
        if let Some(cur) = self.current {
            self.current = Some(match cur.cmp(&index) {
                std::cmp::Ordering::Greater => cur - 1,
                std::cmp::Ordering::Equal => cur.min(self.tracks.len().saturating_sub(1)),
                std::cmp::Ordering::Less => cur,
            });
            if self.tracks.is_empty() {
                self.current = None;
            }
        }
        Ok(removed)
    }

    /// 清空队列。
    pub fn clear(&mut self) {
        self.tracks.clear();
        self.current = None;
    }

    /// 跳到指定下标。
    pub fn jump(&mut self, index: usize) -> Result<&Track, QueueError> {
        if index >= self.tracks.len() {
            return Err(QueueError::IndexOutOfBounds(index));
        }
        self.current = Some(index);
        Ok(&self.tracks[index])
    }

    /// 下一首。已是末尾或空队列返回 `None`（位置不变）。
    pub fn next(&mut self) -> Option<&Track> {
        let cur = self.current?;
        if cur + 1 >= self.tracks.len() {
            return None;
        }
        self.current = Some(cur + 1);
        Some(&self.tracks[cur + 1])
    }

    /// 上一首。已是开头或空队列返回 `None`（位置不变）。
    pub fn prev(&mut self) -> Option<&Track> {
        let cur = self.current?;
        if cur == 0 {
            return None;
        }
        self.current = Some(cur - 1);
        Some(&self.tracks[cur - 1])
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
            title: format!("歌曲{}", id),
            artists: vec![],
            album: None,
            duration_ms: None,
            cover_url: None,
            ..Track::default()
        }
    }

    #[test]
    fn add_sets_current_on_first_insert() {
        let mut q = PlayQueue::with_default_capacity();
        assert!(q.current().is_none());
        q.add(track("1")).unwrap();
        assert_eq!(q.current_index(), Some(0));
    }

    #[test]
    fn single_add_over_capacity_is_rejected_not_silently_dropped() {
        let mut q = PlayQueue::new(2);
        q.add(track("1")).unwrap();
        q.add(track("2")).unwrap();
        assert_eq!(q.add(track("3")), Err(QueueError::Full));
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn extend_truncates_to_remaining_capacity() {
        let mut q = PlayQueue::new(5);
        let batch: Vec<Track> = (0..100).map(|i| track(&i.to_string())).collect();
        let added = q.extend(batch);
        assert_eq!(added, 5, "长歌单入队截断到容量");
        assert_eq!(q.len(), 5);
        assert!(q.len() <= q.capacity(), "内存有界不变式");
    }

    #[test]
    fn queue_never_exceeds_capacity_with_repeated_bulk_inserts() {
        // 模拟长歌单多页灌入：容量恒定，不无限增长（M4 验收第 2 条）
        let mut q = PlayQueue::new(10);
        for page in 0..50 {
            let batch: Vec<Track> = (0..20)
                .map(|i| track(&format!("p{}-{}", page, i)))
                .collect();
            q.extend(batch);
            assert!(q.len() <= 10);
        }
        assert_eq!(q.len(), 10);
    }

    #[test]
    fn zero_capacity_rejects_all_adds() {
        let mut q = PlayQueue::new(0);
        assert_eq!(q.add(track("1")), Err(QueueError::Full));
        assert_eq!(q.extend(vec![track("1"), track("2")]), 0);
    }

    #[test]
    fn next_and_prev_walk_and_stop_at_ends() {
        let mut q = PlayQueue::with_default_capacity();
        q.extend(vec![track("1"), track("2"), track("3")]);

        assert_eq!(q.current().unwrap().id, "1");
        assert_eq!(q.next().unwrap().id, "2");
        assert_eq!(q.next().unwrap().id, "3");
        assert!(q.next().is_none(), "末尾 next 返回 None");
        assert_eq!(q.current().unwrap().id, "3", "末尾位置不变");

        assert_eq!(q.prev().unwrap().id, "2");
        assert_eq!(q.prev().unwrap().id, "1");
        assert!(q.prev().is_none(), "开头 prev 返回 None");
        assert_eq!(q.current().unwrap().id, "1");
    }

    #[test]
    fn next_prev_on_empty_queue_is_none() {
        let mut q = PlayQueue::with_default_capacity();
        assert!(q.next().is_none());
        assert!(q.prev().is_none());
    }

    #[test]
    fn jump_moves_current_and_rejects_out_of_bounds() {
        let mut q = PlayQueue::with_default_capacity();
        q.extend(vec![track("1"), track("2"), track("3")]);
        assert_eq!(q.jump(2).unwrap().id, "3");
        assert!(matches!(q.jump(3), Err(QueueError::IndexOutOfBounds(3))));
        assert_eq!(q.current_index(), Some(2), "越界 jump 不改变位置");
    }

    #[test]
    fn remove_shifts_current_correctly() {
        let mut q = PlayQueue::with_default_capacity();
        q.extend(vec![track("1"), track("2"), track("3")]);
        q.jump(1).unwrap();

        // 移除当前之前的位置 → 当前下标左移
        q.remove(0).unwrap();
        assert_eq!(q.current_index(), Some(0));
        assert_eq!(q.current().unwrap().id, "2");

        // 移除当前 → 后一首顶上
        q.remove(0).unwrap();
        assert_eq!(q.current().unwrap().id, "3");

        // 移除最后一个 → 队列空，当前位置清空
        q.remove(0).unwrap();
        assert!(q.is_empty());
        assert_eq!(q.current_index(), None);
    }

    #[test]
    fn remove_out_of_bounds_is_error() {
        let mut q = PlayQueue::with_default_capacity();
        q.add(track("1")).unwrap();
        assert!(matches!(q.remove(1), Err(QueueError::IndexOutOfBounds(1))));
    }

    #[test]
    fn clear_resets_everything() {
        let mut q = PlayQueue::with_default_capacity();
        q.extend(vec![track("1"), track("2")]);
        q.clear();
        assert!(q.is_empty());
        assert_eq!(q.current_index(), None);
    }
}
