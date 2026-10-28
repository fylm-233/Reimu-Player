//! Account-scoped playlist page cache. All I/O runs on the metadata worker.
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use crate::music::provider::{Playlist, ProviderId};

const TTL: u64 = 15 * 60;
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

fn path(scope: &str, id: &str, offset: u32, limit: u32) -> Option<PathBuf> {
    let key = format!("v1:{scope}:{id}:{offset}:{limit}");
    Some(super::library::config_dir()?.join("playlist-cache")
        .join(format!("{:016x}.json", super::widgets::hash_id(&key))))
}

pub fn read(scope: &str, id: &str, offset: u32, limit: u32, stale: bool) -> Option<Playlist> {
    let path = path(scope, id, offset, limit)?;
    if std::fs::metadata(&path).ok()?.len() > MAX_FILE_BYTES { return None; }
    let value = super::persist::read_json(&path)?;
    if value["version"].as_u64()? != 1 || value["id"].as_str()? != id
        || value["offset"].as_u64()? != offset as u64
        || value["limit"].as_u64()? != limit as u64 { return None; }
    let age = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs()
        .saturating_sub(value["saved"].as_u64()?);
    if age > if stale { 7 * 24 * 3600 } else { TTL } { return None; }
    let tracks = value["tracks"].as_array()?.iter()
        .map(super::persist::track_from_json).collect::<Option<Vec<_>>>()?;
    Some(Playlist { provider: ProviderId::Netease, id: id.to_owned(),
        name: value["name"].as_str().map(str::to_owned),
        total: value["total"].as_u64(), tracks })
}

pub fn write(scope: &str, playlist: &Playlist, offset: u32, limit: u32) {
    let Some(path) = path(scope, &playlist.id, offset, limit) else { return; };
    let saved = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let value = serde_json::json!({"version": 1, "saved": saved,
        "id": playlist.id, "name": playlist.name, "total": playlist.total,
        "offset": offset, "limit": limit,
        "tracks": playlist.tracks.iter().map(super::persist::track_to_json).collect::<Vec<_>>()});
    let text = value.to_string();
    if text.len() as u64 > MAX_FILE_BYTES { return; }
    if super::persist::write_atomic(&path, &text).is_err() { return; }
    // Bounded cache: at most 512 pages / 64 MiB, evict oldest saved pages.
    let Some(dir) = path.parent() else { return; };
    let Ok(entries) = std::fs::read_dir(dir) else { return; };
    let mut files: Vec<_> = entries.flatten().filter_map(|entry| {
        if entry.path().extension()?.to_str()? != "json" { return None; }
        let metadata = entry.metadata().ok()?;
        Some((entry.path(), metadata.modified().unwrap_or(UNIX_EPOCH), metadata.len()))
    }).collect();
    files.sort_by_key(|(_, time, _)| *time);
    let mut bytes: u64 = files.iter().map(|(_, _, size)| size).sum();
    let mut count = files.len();
    for (file, _, size) in files {
        if count <= 512 && bytes <= 64 * 1024 * 1024 { break; }
        if std::fs::remove_file(file).is_ok() { count -= 1; bytes = bytes.saturating_sub(size); }
    }
}
