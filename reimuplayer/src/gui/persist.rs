//! 用户数据的落盘工具（播放历史 / 收藏 / 本地歌单共用）。
//!
//! 为什么单独一层：这三份数据都是「用户自己攒出来的」，丢了就是体验回退，
//! 与可重建的曲库索引（[`super::library`]）不是一类东西。它们共用同一套纪律：
//!
//! - 版本号不符 → 直接当没有（宁可让用户重攒，也不要用错位字段）；
//! - 写盘走 `*.tmp` + `rename`（断电不会留下半截 JSON）；
//! - 读取失败一律 `None`，**不报错**——用户数据损坏不该阻止程序启动。
//!
//! 曲目快照统一用 [`track_to_json`] / [`track_from_json`]：三份文件都存
//! `Track` 的完整快照（而不是只存 ID），这样曲库重扫、在线曲目下架之后，
//! 历史与收藏里仍然显示得出当时的标题与艺人。

use std::path::Path;

use crate::music::provider::{ProviderId, Track};

/// 读取 JSON 文件（不存在/损坏 → `None`，不报错）。
pub fn read_json(path: &Path) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// 原子写入：建目录 → 写 `.tmp` → `rename`。
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// Provider 的持久化代码（与界面语言无关，跨版本稳定）。
pub fn provider_code(provider: ProviderId) -> &'static str {
    match provider {
        ProviderId::Local => "local",
        ProviderId::Netease => "netease",
    }
}

/// 持久化代码 → Provider（未知值返回 `None`，调用方跳过该条目）。
pub fn provider_from_code(code: &str) -> Option<ProviderId> {
    match code {
        "local" => Some(ProviderId::Local),
        "netease" => Some(ProviderId::Netease),
        _ => None,
    }
}

/// 曲目 → JSON 快照。
pub fn track_to_json(track: &Track) -> serde_json::Value {
    serde_json::json!({
        "provider": provider_code(track.provider),
        "id": track.id,
        "title": track.title,
        "artists": track.artists,
        // 专辑/艺人的 **ID** 也要存：否则重启后收藏里的歌点专辑名还得再请求一次
        // `song/detail` 才知道该跳哪个专辑页。缺失就是 null / 空数组，不编造。
        "album_id": track.album_id,
        "artist_ids": track.artist_ids,
        "album": track.album,
        "duration_ms": track.duration_ms,
        "cover_url": track.cover_url,
    })
}

/// JSON 快照 → 曲目。
///
/// `id` 与 `provider` 是**必需**字段：缺了它们，这条记录既不能播放也不能去重，
/// 留着只会变成幽灵行。其余字段缺失按空值处理（老版本文件兼容）。
pub fn track_from_json(value: &serde_json::Value) -> Option<Track> {
    let provider = provider_from_code(value.get("provider")?.as_str()?)?;
    let id = value.get("id")?.as_str()?.to_string();
    let artists = value
        .get("artists")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let artist_ids = value
        .get("artist_ids")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some(Track {
        provider,
        id,
        title: value
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        artists,
        album: value
            .get("album")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        album_id: value
            .get("album_id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        artist_ids,
        duration_ms: value.get("duration_ms").and_then(|v| v.as_u64()),
        cover_url: value
            .get("cover_url")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn online() -> Track {
        Track {
            provider: ProviderId::Netease,
            id: "42".to_string(),
            title: "起风了".to_string(),
            artists: vec!["买辣椒也用券".to_string()],
            album: Some("起风了".to_string()),
            duration_ms: Some(325_000),
            cover_url: Some("https://cdn.example/42.jpg".to_string()),
            ..Track::default()
        }
    }

    fn local() -> Track {
        Track {
            provider: ProviderId::Local,
            id: "D:/Music/a.flac".to_string(),
            title: "A".to_string(),
            artists: vec!["—".to_string()],
            album: None,
            duration_ms: None,
            cover_url: None,
            ..Track::default()
        }
    }

    #[test]
    fn track_snapshot_round_trips_both_providers() {
        for track in [online(), local()] {
            let json = track_to_json(&track);
            assert_eq!(track_from_json(&json), Some(track));
        }
    }

    #[test]
    fn provider_codes_round_trip_and_reject_unknown() {
        for p in [ProviderId::Local, ProviderId::Netease] {
            assert_eq!(provider_from_code(provider_code(p)), Some(p));
        }
        assert_eq!(provider_from_code("qq"), None);
        assert_eq!(provider_from_code(""), None);
    }

    #[test]
    fn snapshot_without_id_or_provider_is_rejected() {
        assert_eq!(
            track_from_json(&serde_json::json!({"provider": "local"})),
            None
        );
        assert_eq!(track_from_json(&serde_json::json!({"id": "x"})), None);
        // 未知 provider 同样拒绝：不能把它猜成 local
        assert_eq!(
            track_from_json(&serde_json::json!({"provider": "kugou", "id": "x"})),
            None
        );
    }

    #[test]
    fn optional_fields_default_instead_of_failing() {
        let track = track_from_json(&serde_json::json!({
            "provider": "netease",
            "id": "7",
        }))
        .expect("最小字段应可解析");
        assert_eq!(track.title, "");
        assert!(track.artists.is_empty());
        assert_eq!(track.album, None);
        assert_eq!(track.duration_ms, None);
    }

    #[test]
    fn atomic_write_creates_dir_and_leaves_no_tmp() {
        let dir = std::env::temp_dir().join(format!("reimu-player-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("data.json");
        write_atomic(&path, "{\"a\":1}").unwrap();
        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());
        assert_eq!(read_json(&path).unwrap()["a"], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_or_broken_file_reads_as_none() {
        let dir =
            std::env::temp_dir().join(format!("reimu-player-persist-miss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(read_json(&dir.join("none.json")).is_none());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bad.json"), "{oops").unwrap();
        assert!(read_json(&dir.join("bad.json")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
