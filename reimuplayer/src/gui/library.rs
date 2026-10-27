//! 本地曲库扫描（阶段 G）：指定文件夹 → 音频文件列表。
//!
//! 约束（规范 6.2 / 8.4）：
//! - 本地路径与在线 ID 语义不同：扫描产物是 [`Track`]（`provider = Local`，
//!   `id` = 文件路径字符串），播放走 `PlayerCore::play_local`，**不触网**；
//! - 扫描只读文件系统、**不深读元数据**：时长未知如实置 `None`，不伪造；
//! - UI 线程调用方直接使用：目录深度与文件数双封顶，防止大目录卡顿
//!   （超出即截断并如实报告 [`LocalLibrary::truncated`]）。
//!
//! 文件夹选择走 `rfd` 系统对话框（[`pick_folder`]）；核心扫描是纯函数，
//! 单元测试自建临时目录验证，不依赖对话框。
//!
//! 持久化（[`save`] / [`load`]）：扫描结果落盘成索引，重启后直接恢复成
//! 「本地曲库」歌单，**无需重新扫描**。索引只存路径与展示名，不缓存音频内容。

use std::path::{Path, PathBuf};

use crate::music::provider::{ProviderId, Track};

/// 可识别的音频扩展名（小写比较）。
pub const AUDIO_EXTS: &[&str] = &[
    "mp3", "flac", "wav", "ogg", "m4a", "aac", "opus", "wma", "aiff", "aif",
];

/// 扫描目录深度上限（起点目录为第 0 层）。
pub const MAX_DEPTH: usize = 4;

/// 扫描曲目数上限：达到即停止扫描并置 [`LocalLibrary::truncated`]。
pub const MAX_FILES: usize = 2000;

/// 曲库索引文件名（位于 [`config_dir`]）。
pub const INDEX_FILE: &str = "local_library.json";

/// 索引格式版本。
///
/// 版本不匹配时**直接丢弃旧索引**并返回 `None`（不尝试迁移）：
/// 曲库可以从文件夹重新扫出来，为它写迁移代码不划算。
pub const INDEX_VERSION: u32 = 1;

/// 是否音频文件（按扩展名判断，大小写不敏感；无扩展名 → 否）。
pub fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// 从文件名（无扩展名）解析展示用「艺人 / 标题」。
///
/// 约定：`Artist - Title`（含空格的 ` - `）→ 拆分；
/// 其余情况艺人未知 → `("—", 原名)`，**不编造**艺人。
pub fn parse_display_name(stem: &str) -> (String, String) {
    if let Some((artist, title)) = stem.split_once(" - ") {
        let (a, t) = (artist.trim(), title.trim());
        if !a.is_empty() && !t.is_empty() {
            return (a.to_string(), t.to_string());
        }
    }
    ("—".to_string(), stem.trim().to_string())
}

/// 扫描结果：目录 + 曲目列表 + 截断标记。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalLibrary {
    /// 扫描的起点目录。
    pub dir: PathBuf,
    /// 按路径字典序排序的曲目（`provider = Local`，`id` = 文件路径）。
    pub tracks: Vec<Track>,
    /// 因达到 [`MAX_FILES`] 提前停止扫描（列表不完整，如实报告）。
    pub truncated: bool,
}

impl LocalLibrary {
    /// 展示名：目录名（非 UTF-8 / 根目录回退全路径）。
    pub fn display_name(&self) -> String {
        self.dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.dir.to_string_lossy().into_owned())
    }

    /// 本地检索：标题 / 艺人 / 文件名（不含扩展名）三路**子串**匹配。
    ///
    /// 为什么不做倒排索引：曲库本身有 [`MAX_FILES`] 上限、且是内存里的 `Vec`，
    /// 一次线性扫描在千级规模下是微秒量级；引 SQLite/FTS5 只会换来一个
    /// 需要迁移、需要落盘、需要处理并发写入的新组件。
    ///
    /// 空查询返回**空列表**（不是全量）：Search 页在没有关键词时不该列出整库。
    /// 匹配不区分大小写；中文按原文子串匹配（无需分词——中文没有词边界，
    /// 子串匹配对"搜一个片段"的直觉反而更准）。
    pub fn filter(&self, query: &str) -> Vec<&Track> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return Vec::new();
        }
        self.tracks
            .iter()
            .filter(|track| {
                let title = track.title.to_lowercase();
                let artists = track.artists.join(" ").to_lowercase();
                let file_name = track
                    .id
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or_default()
                    .to_lowercase();
                title.contains(&needle) || artists.contains(&needle) || file_name.contains(&needle)
            })
            .collect()
    }
}

/// 递归扫描目录内的音频文件。
///
/// - 深度超过 [`MAX_DEPTH`] 的子树不再进入；
/// - 曲目达到 [`MAX_FILES`] 即停止（`truncated = true`）；
/// - 结果按 `id`（路径字符串）排序，保证同目录多次扫描顺序确定。
///
/// # 错误
/// 目录不存在或不是目录 → `Err(人话)`。子目录不可读**容忍跳过**（记日志）。
/// 扫描失败原因（**类型化**，文案由 UI 按语言渲染）。
///
/// 为什么不用 `String`：错误文案是用户可见的，必须能跟着界面语言切换；
/// 库层只负责"说清是什么错"，措辞交给 [`super::i18n`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanError {
    /// 路径存在但不是目录（或根本不存在）。
    NotADirectory(std::path::PathBuf),
}

impl ScanError {
    /// 按界面语言渲染成人话。
    pub fn message(&self, lang: crate::i18n::Lang) -> String {
        match self {
            ScanError::NotADirectory(path) => crate::i18n::pick(
                lang,
                &format!("{} 不是有效目录", path.display()),
                &format!("{} is not a valid directory", path.display()),
            )
            .to_string(),
        }
    }
}

pub fn scan_dir(dir: &Path) -> Result<LocalLibrary, ScanError> {
    if !dir.is_dir() {
        return Err(ScanError::NotADirectory(dir.to_path_buf()));
    }
    let mut tracks = Vec::new();
    let mut truncated = false;
    walk(dir, 0, &mut tracks, &mut truncated);
    tracks.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(LocalLibrary {
        dir: dir.to_path_buf(),
        tracks,
        truncated,
    })
}

/// 深度优先遍历；条目按路径排序保证确定性。
fn walk(dir: &Path, depth: usize, tracks: &mut Vec<Track>, truncated: &mut bool) {
    if depth >= MAX_DEPTH || *truncated {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("曲库子目录不可读，跳过: {} — {}", dir.display(), e);
            return;
        }
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if *truncated {
            return;
        }
        if path.is_dir() {
            walk(&path, depth + 1, tracks, truncated);
        } else if is_audio_file(&path) {
            if tracks.len() >= MAX_FILES {
                *truncated = true;
                return;
            }
            tracks.push(track_from_path(&path));
        }
    }
}

/// 文件 → [`Track`]（本地语义：`id` = 路径；时长未知为 `None`，不伪造）。
///
/// 公开版本供"播放历史/收藏"在只有路径时构造曲目快照（拖放、设置面板装载）。
pub fn track_for_path(path: &Path) -> Track {
    track_from_path(path)
}

fn track_from_path(path: &Path) -> Track {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (artist, title) = parse_display_name(&stem);
    Track {
        provider: ProviderId::Local,
        id: path.to_string_lossy().into_owned(),
        title,
        artists: vec![artist],
        album: None,
        album_id: None,
        artist_ids: Vec::new(),
        duration_ms: None,
        cover_url: None,
    }
}

/// 系统文件夹选择对话框（`rfd`）；用户取消返回 `None`。
///
/// `lang` 只影响对话框标题——那是 OS 画的原生窗口，标题是我们唯一能控的字。
pub fn pick_folder(lang: crate::i18n::Lang) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title(crate::i18n::pick(
            lang,
            "选择音乐文件夹",
            "Choose music folder",
        ))
        .pick_folder()
}

// ---------------------------------------------------------------------------
// 持久化：扫描结果 ↔ 索引文件
// ---------------------------------------------------------------------------

/// 应用配置目录（`%APPDATA%\ReimuPlayer`；非 Windows 回退 XDG / `~/.config`）。
///
/// 只用来放**应用自己的索引**，不碰用户的音乐文件目录。
pub fn config_dir() -> Option<PathBuf> {
    if let Some(base) = std::env::var_os("APPDATA") {
        return Some(PathBuf::from(base).join("ReimuPlayer"));
    }
    if let Some(base) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(base).join("reimu-player"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config").join("reimu-player"))
}

/// 改名前用过的配置目录名（Windows 大写、XDG 小写两种写法都要认）。
const LEGACY_DIR_NAMES: [&str; 2] = ["Sonora", "sonora"];

/// 把旧品牌目录里的文件**复制**进 [`config_dir`]，返回复制的条目数。
///
/// 复制而不是移动：迁移出 bug 时旧目录还完整，用户能退回旧版本；改名后已经
/// 重新登录/新建过的文件也不被覆盖（只补缺）。启动时调用一次即可，
/// 见 [`super::run_reimu_player`]。
pub fn migrate_legacy_config_dir() -> usize {
    let Some(target) = config_dir() else {
        return 0;
    };
    let mut bases: Vec<PathBuf> = Vec::new();
    if let Some(base) = std::env::var_os("APPDATA") {
        bases.push(PathBuf::from(base));
    }
    if let Some(base) = std::env::var_os("XDG_CONFIG_HOME") {
        bases.push(PathBuf::from(base));
    }
    if let Some(home) = std::env::var_os("HOME") {
        bases.push(PathBuf::from(home).join(".config"));
    }
    let mut copied = 0;
    for base in bases {
        for name in LEGACY_DIR_NAMES {
            let legacy = base.join(name);
            if legacy == target || !legacy.is_dir() {
                continue;
            }
            copied += copy_missing_entries(&legacy, &target);
        }
    }
    copied
}

/// 递归把 `from` 里 `to` 还缺的文件复制过去（已存在的一律不动）。
fn copy_missing_entries(from: &Path, to: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(from) else {
        return 0;
    };
    let mut copied = 0;
    for entry in entries.flatten() {
        let src = entry.path();
        let Some(name) = src.file_name() else {
            continue;
        };
        let dst = to.join(name);
        if src.is_dir() {
            copied += copy_missing_entries(&src, &dst);
        } else if dst.exists() {
            continue;
        } else if std::fs::create_dir_all(to).is_ok() && std::fs::copy(&src, &dst).is_ok() {
            copied += 1;
        }
    }
    copied
}

/// 索引文件完整路径。
pub fn index_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join(INDEX_FILE))
}

/// 曲库 → JSON 文本（纯函数，可测）。
pub fn to_json(lib: &LocalLibrary) -> String {
    let tracks: Vec<serde_json::Value> = lib
        .tracks
        .iter()
        .map(|t| {
            serde_json::json!({
                "id": t.id,
                "title": t.title,
                "artists": t.artists,
                "album": t.album,
                "duration_ms": t.duration_ms,
                // 匹配到的网易云封面：不落盘的话每次重启都要重新查一遍
                // `search_match`（读标签 + 算整文件 MD5），白耗一次磁盘与网络。
                "cover_url": t.cover_url,
            })
        })
        .collect();
    serde_json::json!({
        "version": INDEX_VERSION,
        "dir": lib.dir.to_string_lossy(),
        "truncated": lib.truncated,
        "tracks": tracks,
    })
    .to_string()
}

/// JSON 文本 → 曲库（纯函数，可测）。
///
/// 任何结构性缺失都返回 `None`——索引是**可再生数据**，
/// 与其半残地加载出一份错列表，不如当作没有、让用户重新扫描。
pub fn from_json(text: &str) -> Option<LocalLibrary> {
    let root: serde_json::Value = serde_json::from_str(text).ok()?;
    if root.get("version")?.as_u64()? != INDEX_VERSION as u64 {
        tracing::info!("本地曲库索引版本不匹配，丢弃并等待重新扫描");
        return None;
    }
    let dir = PathBuf::from(root.get("dir")?.as_str()?);
    let truncated = root
        .get("truncated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut tracks = Vec::new();
    for item in root.get("tracks")?.as_array()? {
        let Some(id) = item.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let artists = item
            .get("artists")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        tracks.push(Track {
            provider: ProviderId::Local,
            id: id.to_string(),
            title: item
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            artists,
            album: item
                .get("album")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            // 本地曲目没有在线专辑/艺人 ID，跳转能力自然缺席
            album_id: None,
            artist_ids: Vec::new(),
            duration_ms: item.get("duration_ms").and_then(|v| v.as_u64()),
            cover_url: item
                .get("cover_url")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string),
        });
    }
    Some(LocalLibrary {
        dir,
        tracks,
        truncated,
    })
}

/// 索引落盘失败原因（**类型化**：文案由 UI 按语言渲染）。
///
/// 为什么不用 `String`：这些错误会经 toast 显示在界面上，写死中文在
/// 英文界面里就是一个漏出点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveError {
    /// 拿不到配置目录（`%APPDATA%` 缺失）。
    NoConfigDir,
    /// 建目录失败。
    CreateDir {
        path: std::path::PathBuf,
        detail: String,
    },
    /// 写临时文件失败。
    Write {
        path: std::path::PathBuf,
        detail: String,
    },
    /// 改名落盘失败。
    Rename {
        path: std::path::PathBuf,
        detail: String,
    },
    /// 删除索引失败。
    Remove {
        path: std::path::PathBuf,
        detail: String,
    },
}

impl SaveError {
    /// 按界面语言渲染成人话。
    pub fn message(&self, lang: crate::i18n::Lang) -> String {
        use crate::i18n::pick;
        match self {
            SaveError::NoConfigDir => pick(
                lang,
                "无法定位配置目录",
                "Cannot locate the config directory",
            )
            .to_string(),
            SaveError::CreateDir { path, detail } => pick(
                lang,
                &format!("无法创建配置目录 {}：{detail}", path.display()),
                &format!(
                    "Cannot create the config directory {}: {detail}",
                    path.display()
                ),
            )
            .to_string(),
            SaveError::Write { path, detail } => pick(
                lang,
                &format!("索引写入失败 {}：{detail}", path.display()),
                &format!("Writing the index failed {}: {detail}", path.display()),
            )
            .to_string(),
            SaveError::Rename { path, detail } => pick(
                lang,
                &format!("索引落盘失败 {}：{detail}", path.display()),
                &format!("Saving the index failed {}: {detail}", path.display()),
            )
            .to_string(),
            SaveError::Remove { path, detail } => pick(
                lang,
                &format!("删除曲库索引失败 {}：{detail}", path.display()),
                &format!("Deleting the index failed {}: {detail}", path.display()),
            )
            .to_string(),
        }
    }
}

/// 写入指定路径（先写 `.tmp` 再 rename，避免中途崩溃留下半截索引）。
pub fn save_to(path: &Path, lib: &LocalLibrary) -> Result<(), SaveError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| SaveError::CreateDir {
            path: parent.to_path_buf(),
            detail: e.to_string(),
        })?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, to_json(lib)).map_err(|e| SaveError::Write {
        path: tmp.clone(),
        detail: e.to_string(),
    })?;
    std::fs::rename(&tmp, path).map_err(|e| SaveError::Rename {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })
}

/// 从指定路径读取；文件不存在或内容损坏返回 `None`（不报错）。
pub fn load_from(path: &Path) -> Option<LocalLibrary> {
    let text = std::fs::read_to_string(path).ok()?;
    let lib = from_json(&text)?;
    tracing::info!(
        "本地曲库索引已恢复: {} 首（{}）",
        lib.tracks.len(),
        lib.dir.display()
    );
    Some(lib)
}

/// 把曲库写入用户配置目录。
pub fn save(lib: &LocalLibrary) -> Result<(), SaveError> {
    let path = index_path().ok_or(SaveError::NoConfigDir)?;
    save_to(&path, lib)
}

/// 从用户配置目录读取曲库索引。
///
/// 目录本身不存在时**仍然返回**：外接盘临时拔掉不该让歌单凭空消失，
/// 真去播那首不存在的文件时会得到明确的人话错误。
pub fn load() -> Option<LocalLibrary> {
    load_from(&index_path()?)
}

/// 删除索引（下次启动不再恢复本地曲库）。
pub fn clear() -> Result<(), SaveError> {
    let Some(path) = index_path() else {
        return Ok(());
    };
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(SaveError::Remove {
            path,
            detail: e.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 测试专用临时目录（进程 + 用例名隔离；调用方负责清理）。
    fn temp_dir(case: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("reimu-player-lib-{}-{}", std::process::id(), case));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn audio_extension_detection_is_case_insensitive() {
        assert!(is_audio_file(Path::new("a/b/c.MP3")));
        assert!(is_audio_file(Path::new("x.flac")));
        assert!(!is_audio_file(Path::new("readme.txt")));
        assert!(!is_audio_file(Path::new("noext")));
    }

    #[test]
    fn display_name_splits_artist_title_convention() {
        assert_eq!(
            parse_display_name("YOASOBI - 夜に駆ける"),
            ("YOASOBI".to_string(), "夜に駆ける".to_string())
        );
        // 无约定分隔符：艺人未知如实置 "—"
        assert_eq!(
            parse_display_name("纯音乐01"),
            ("—".to_string(), "纯音乐01".to_string())
        );
        // 伪分隔符（单空格-单空格才算）不拆分
        assert_eq!(
            parse_display_name("A-B"),
            ("—".to_string(), "A-B".to_string())
        );
    }

    #[test]
    fn scan_dir_collects_audio_recursively_and_sorts() {
        let dir = temp_dir("scan");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("b.mp3"), b"").unwrap();
        fs::write(dir.join("a.flac"), b"").unwrap();
        fs::write(dir.join("sub").join("c.wav"), b"").unwrap();
        fs::write(dir.join("note.txt"), b"").unwrap();

        let lib = scan_dir(&dir).unwrap();
        let names: Vec<&str> = lib
            .tracks
            .iter()
            .map(|t| t.id.rsplit(['/', '\\']).next().unwrap_or(&t.id))
            .collect();
        assert_eq!(
            names,
            vec!["a.flac", "b.mp3", "c.wav"],
            "音频递归收集且排序"
        );
        assert!(!lib.truncated);
        assert!(lib.tracks.iter().all(|t| t.provider == ProviderId::Local));
        assert!(
            lib.tracks.iter().all(|t| t.duration_ms.is_none()),
            "时长未知不伪造"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_dir_rejects_non_directory() {
        assert!(scan_dir(Path::new("Q:\\definitely\\missing")).is_err());
    }

    #[test]
    fn scan_respects_depth_limit() {
        let dir = temp_dir("depth");
        // 第 5 层（depth=5 > MAX_DEPTH=4-1？walk 深度 0..3 进入，第 4 层停止）
        let mut deep = dir.clone();
        for i in 0..6 {
            deep = deep.join(format!("d{i}"));
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("deep.mp3"), b"").unwrap();

        let lib = scan_dir(&dir).unwrap();
        assert!(lib.tracks.is_empty(), "超出深度上限的文件不收集");
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- 持久化 ----

    fn sample_lib() -> LocalLibrary {
        LocalLibrary {
            dir: PathBuf::from("D:/Music/Collection"),
            tracks: vec![
                Track {
                    provider: ProviderId::Local,
                    id: "D:/Music/Collection/a.flac".into(),
                    title: "夜に駆ける".into(),
                    artists: vec!["YOASOBI".into()],
                    album: Some("THE BOOK".into()),
                    duration_ms: Some(261_000),
                    cover_url: None,
                    ..Track::default()
                },
                Track {
                    provider: ProviderId::Local,
                    id: "D:/Music/Collection/b.mp3".into(),
                    title: "b".into(),
                    artists: vec!["—".into()],
                    album: None,
                    duration_ms: None,
                    cover_url: None,
                    ..Track::default()
                },
            ],
            truncated: true,
        }
    }

    #[test]
    fn json_round_trips_every_field() {
        let lib = sample_lib();
        let back = from_json(&to_json(&lib)).expect("应能解析回曲库");
        assert_eq!(back, lib, "曲库经 JSON 往返必须完全一致");
    }

    #[test]
    fn json_rejects_foreign_version() {
        let mut v: serde_json::Value = serde_json::from_str(&to_json(&sample_lib())).unwrap();
        v["version"] = serde_json::json!(INDEX_VERSION + 1);
        assert!(from_json(&v.to_string()).is_none(), "版本不符应丢弃索引");
    }

    #[test]
    fn json_rejects_garbage_without_panicking() {
        assert!(from_json("").is_none());
        assert!(from_json("not json").is_none());
        assert!(from_json("{}").is_none());
        assert!(
            from_json(r#"{"version":1}"#).is_none(),
            "缺 dir/tracks 应拒绝"
        );
    }

    #[test]
    fn json_tolerates_partial_track_fields() {
        // 单条曲目缺字段时用中性默认值，不整体失败（索引可再生，容忍脏数据）
        let text = r#"{"version":1,"dir":"D:/M","tracks":[{"id":"D:/M/x.mp3"}]}"#;
        let lib = from_json(text).expect("应能解析");
        assert_eq!(lib.tracks.len(), 1);
        assert_eq!(lib.tracks[0].title, "");
        assert!(lib.tracks[0].artists.is_empty());
        assert!(lib.tracks[0].duration_ms.is_none());
        assert!(!lib.truncated, "缺失 truncated 视为 false");
    }

    #[test]
    fn save_and_load_from_disk() {
        let dir = temp_dir("persist");
        let path = dir.join("nested").join("local_library.json");
        let lib = sample_lib();
        save_to(&path, &lib).expect("写入应成功");
        assert!(path.is_file(), "索引文件应存在");
        assert!(
            !path.with_extension("json.tmp").exists(),
            "临时文件必须被 rename 掉"
        );
        assert_eq!(load_from(&path).as_ref(), Some(&lib));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_file_is_none_not_error() {
        let dir = temp_dir("missing");
        assert!(load_from(&dir.join("nope.json")).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_migration_fills_gaps_without_touching_existing_files() {
        // 改名迁移的全部风险都在"覆盖"：用户在改名后可能已经重新登录过，
        // 那份新 session.bin 绝不能被旧目录里的过期文件顶掉。
        let root = temp_dir("migrate");
        let legacy = root.join("Sonora");
        let target = root.join("ReimuPlayer");
        fs::create_dir_all(legacy.join("covers")).unwrap();
        fs::write(legacy.join("prefs.json"), b"old").unwrap();
        fs::write(legacy.join("covers").join("a.bin"), b"cover").unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("prefs.json"), b"new").unwrap();

        let copied = copy_missing_entries(&legacy, &target);
        assert_eq!(copied, 1, "只有缺失的那一个文件该被复制");
        assert_eq!(fs::read(target.join("prefs.json")).unwrap(), b"new");
        assert_eq!(
            fs::read(target.join("covers").join("a.bin")).unwrap(),
            b"cover",
            "子目录里的文件要跟着搬，且目录自动建出来"
        );
        assert!(
            legacy.join("prefs.json").is_file(),
            "迁移是复制，旧目录必须原样留着（出问题能退回旧版本）"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn legacy_dir_names_are_the_old_brand_spellings() {
        // 少认一种写法，那个平台上的用户升级后就是从零开始
        assert_eq!(LEGACY_DIR_NAMES, ["Sonora", "sonora"]);
        if let Some(dir) = config_dir() {
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            assert!(
                !LEGACY_DIR_NAMES.contains(&name.as_str()),
                "新配置目录不能和旧目录同名，否则迁移是在跟自己复制"
            );
        }
    }

    #[test]
    fn save_to_is_idempotent() {
        let dir = temp_dir("idem");
        let path = dir.join("idx.json");
        let lib = sample_lib();
        save_to(&path, &lib).unwrap();
        save_to(&path, &lib).unwrap();
        assert_eq!(load_from(&path).as_ref(), Some(&lib));
        let _ = fs::remove_dir_all(&dir);
    }

    fn search_lib() -> LocalLibrary {
        let track = |path: &str, title: &str, artist: &str| Track {
            provider: ProviderId::Local,
            id: path.to_string(),
            title: title.to_string(),
            artists: vec![artist.to_string()],
            album: None,
            duration_ms: None,
            cover_url: None,
            ..Track::default()
        };
        LocalLibrary {
            dir: PathBuf::from("D:/Music"),
            tracks: vec![
                track("D:/Music/01 - 起风了.flac", "起风了", "买辣椒也用券"),
                track("D:/Music/02 - City Pop.flac", "City Pop", "—"),
                track("D:/Music/03 - 夜に駆ける.flac", "夜に駆ける", "YOASOBI"),
            ],
            truncated: false,
        }
    }

    #[test]
    fn filter_matches_title_artist_and_file_name() {
        let lib = search_lib();
        // 标题（中文子串）
        let hits = lib.filter("起风");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "起风了");
        // 艺人
        assert_eq!(lib.filter("yoasobi").len(), 1);
        // 文件名（id 末段）
        assert_eq!(lib.filter("02 -").len(), 1);
        // 大小写不敏感
        assert_eq!(lib.filter("CITY").len(), 1);
    }

    #[test]
    fn filter_empty_query_returns_nothing() {
        // 没有关键词时列出整库会把 Search 页变成"第二个曲库页"
        assert!(search_lib().filter("").is_empty());
        assert!(search_lib().filter("   ").is_empty());
    }

    #[test]
    fn filter_no_match_is_empty_and_ignores_surrounding_space() {
        let lib = search_lib();
        assert!(lib.filter("不存在的歌").is_empty());
        assert_eq!(lib.filter("  起风  ").len(), 1);
    }
}
