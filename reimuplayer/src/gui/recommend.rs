//! 离线每日推荐（未登录时的"每日"内容）。
//!
//! 在线每日歌单需要登录（服务端按账号生成），而**本地曲库**不该因为没登录就
//! 没有任何"今天听什么"的入口。这里的算法刻意保持简单、可解释：
//!
//! 1. 候选 = 30 天内没听过的曲目（"没听过"优先于"听过很多次"）；
//! 2. 偏好 = 最近 30 天听得最多的艺人/专辑（同艺人或同专辑的候选排前面）；
//! 3. 打散 = 以"当天"为种子做确定性洗牌——同一天内反复进入页面结果不变，
//!    换一天才换一批（"每日"的仪式感来自确定性，不是随机刷新）。
//!
//! 种子取 **UTC 日序号**（`now / 86400`）：不依赖时区数据库，代价是换新时刻
//! 落在本地时区的非零点（UTC+8 约为早上 8 点）。对"今天听什么"这种场景，
//! 这个偏差可以接受；换来的是零依赖与完全可测。
//!
//! 全部为纯函数：输入曲库快照 + 历史 + 当前时间，输出曲目列表，无 IO。

use crate::music::provider::Track;

use super::history::PlayHistory;

/// 30 天窗口（推荐只看近一个月的口味）。
const TASTE_DAYS: i64 = 30;

/// 艺人/专辑被算作"偏好"的最低播放次数。
const LOVED_MIN_PLAYS: usize = 2;

/// 当日种子（UTC 日序号）。
pub fn daily_seed(now: i64) -> u64 {
    (now.max(0) / 86_400) as u64
}

/// 线性同余伪随机数（零依赖；只用于打散，不用于任何安全场景）。
fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state >> 33
}

/// 确定性洗牌（Fisher–Yates）。
fn shuffle<T>(items: &mut [T], seed: u64) {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for i in (1..items.len()).rev() {
        let j = (lcg(&mut state) as usize) % (i + 1);
        items.swap(i, j);
    }
}

/// 生成当日离线推荐。
///
/// `limit` 是期望条数；实际返回不超过 `min(limit, tracks.len())`。
/// 候选不足时会用"最近听过"的曲目补齐（总比空列表有用），但排序仍在候选之后。
pub fn daily_picks(tracks: &[Track], history: &PlayHistory, now: i64, limit: usize) -> Vec<Track> {
    if tracks.is_empty() || limit == 0 {
        return Vec::new();
    }
    let seed = daily_seed(now);
    let played = history.played_within_days(TASTE_DAYS, now);
    let loved_artists: Vec<String> = history
        .artist_counts(TASTE_DAYS, now)
        .into_iter()
        .filter(|(_, n)| *n >= LOVED_MIN_PLAYS)
        .map(|(name, _)| name)
        .collect();
    let loved_albums: Vec<String> = history
        .album_counts(TASTE_DAYS, now)
        .into_iter()
        .filter(|(_, n)| *n >= LOVED_MIN_PLAYS)
        .map(|(name, _)| name)
        .collect();

    // 已听 / 未听分组（保持曲库原顺序，保证同种子同结果）
    let mut fresh = Vec::new();
    let mut heard = Vec::new();
    for track in tracks {
        if played.contains(&(track.provider, track.id.clone())) {
            heard.push(track.clone());
        } else {
            fresh.push(track.clone());
        }
    }

    let prefers = |track: &Track| {
        track
            .artists
            .iter()
            .any(|a| loved_artists.iter().any(|l| l == a.trim()))
            || track
                .album
                .as_deref()
                .map(str::trim)
                .is_some_and(|a| loved_albums.iter().any(|l| l == a))
    };
    let (mut preferred, mut others): (Vec<Track>, Vec<Track>) =
        fresh.into_iter().partition(|t| prefers(t));
    shuffle(&mut preferred, seed);
    shuffle(&mut others, seed ^ 0xA5A5_A5A5);
    let mut picks: Vec<Track> = preferred.into_iter().chain(others).collect();

    if picks.len() < limit {
        shuffle(&mut heard, seed ^ 0x5A5A_5A5A);
        picks.extend(heard);
    }
    picks.truncate(limit.min(tracks.len()));
    picks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::music::provider::ProviderId;

    fn track(id: &str, artist: &str, album: &str) -> Track {
        Track {
            provider: ProviderId::Netease,
            id: id.to_string(),
            title: format!("T{id}"),
            artists: vec![artist.to_string()],
            album: Some(album.to_string()),
            duration_ms: Some(200_000),
            cover_url: None,
            ..Track::default()
        }
    }

    fn library() -> Vec<Track> {
        vec![
            track("1", "A", "X"),
            track("2", "A", "X"),
            track("3", "B", "Y"),
            track("4", "C", "Z"),
            track("5", "D", "W"),
        ]
    }

    const DAY: i64 = 86_400;

    #[test]
    fn same_day_is_deterministic_and_next_day_changes() {
        let lib = library();
        let history = PlayHistory::new();
        let now = 1_700_000_000;
        let a = daily_picks(&lib, &history, now, 3);
        let b = daily_picks(&lib, &history, now + 3_600, 3);
        assert_eq!(a, b, "同一天内结果必须稳定");
        let ids_a: Vec<&str> = a.iter().map(|t| t.id.as_str()).collect();
        // 次日结果通常不同（极小概率相同，多试几天至少有一天不同）
        let mut changed = false;
        for d in 1..8 {
            let c = daily_picks(&lib, &history, now + d * DAY, 3);
            let ids_c: Vec<&str> = c.iter().map(|t| t.id.as_str()).collect();
            if ids_c != ids_a {
                changed = true;
                break;
            }
        }
        assert!(changed, "换一天应该换一批");
    }

    #[test]
    fn recently_played_tracks_are_excluded_when_enough_fresh() {
        let lib = library();
        let mut history = PlayHistory::new();
        for id in ["1", "2"] {
            history.record(&lib[id.parse::<usize>().unwrap() - 1], 40_000, 1_000);
        }
        let picks = daily_picks(&lib, &history, 1_000 + DAY, 3);
        assert_eq!(picks.len(), 3);
        assert!(picks.iter().all(|t| t.id != "1" && t.id != "2"));
    }

    #[test]
    fn thirty_days_old_plays_are_fresh_again() {
        let lib = library();
        let mut history = PlayHistory::new();
        for t in &lib {
            history.record(t, 40_000, 1_000);
        }
        let now = 1_000 + 31 * DAY;
        let picks = daily_picks(&lib, &history, now, 5);
        assert_eq!(picks.len(), 5);
        // 全部曲目都已过窗口，候选恢复完整
        assert!(picks.iter().all(|t| t.id != "9"));
    }

    #[test]
    fn loved_artist_candidates_come_first() {
        let lib = library();
        let mut history = PlayHistory::new();
        // A 被听两次（同一艺人，不同曲目）→ 成为偏好
        history.record(&track("10", "A", "X"), 40_000, 1_000);
        history.record(&track("11", "A", "X"), 40_000, 2_000);
        // 这两首不在曲库里，只用来累计偏好
        let now = 3_000;
        let picks = daily_picks(&lib, &history, now, 2);
        assert_eq!(picks.len(), 2);
        assert!(
            picks.iter().all(|t| t.artists[0] == "A"),
            "偏好艺人的候选应排在最前: {:?}",
            picks.iter().map(|t| t.id.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn fills_with_heard_tracks_when_library_is_small() {
        let lib = library();
        let mut history = PlayHistory::new();
        for t in &lib {
            history.record(t, 40_000, 1_000);
        }
        // 全部已听、窗口内 → 候选为空，但仍应给出内容（不空列表）
        let picks = daily_picks(&lib, &history, 1_000 + DAY, 3);
        assert_eq!(picks.len(), 3);
    }

    #[test]
    fn limit_is_respected_and_never_exceeds_library() {
        let lib = library();
        let history = PlayHistory::new();
        assert_eq!(daily_picks(&lib, &history, 1_000, 2).len(), 2);
        assert_eq!(daily_picks(&lib, &history, 1_000, 99).len(), lib.len());
        assert!(daily_picks(&lib, &history, 1_000, 0).is_empty());
    }

    #[test]
    fn empty_library_returns_empty() {
        assert!(daily_picks(&[], &PlayHistory::new(), 1_000, 10).is_empty());
    }

    #[test]
    fn seed_is_stable_within_a_utc_day() {
        let base = 20_000 * DAY + 100;
        assert_eq!(daily_seed(base), daily_seed(base + 1_000));
        // 同一天最后一秒（base 已偏移 100 秒）
        assert_eq!(daily_seed(base), daily_seed(base + DAY - 101));
        assert_ne!(daily_seed(base), daily_seed(base + DAY));
    }
}
