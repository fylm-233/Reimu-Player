//! 用户偏好持久化（目前只有播放音质上限）。
//!
//! 为什么单独一个文件：本地曲库索引（[`super::library::INDEX_FILE`]）是**可重建的
//! 缓存**（丢了重扫就行），而偏好是**用户的选择**，丢了就是行为回退。两者混在
//! 一起会让"清缓存"顺带清掉设置。
//!
//! 落盘策略与曲库索引一致（`docs/` 里的既有约定）：
//! - 版本号不符**直接丢弃并回默认**——宁可让用户重选一次，也不要用错位字段；
//! - 写盘走 `*.tmp` + `rename`，避免断电留下半截 JSON。
//!
//! 目录：`%APPDATA%\ReimuPlayer\prefs.json`（非 Windows 回退 XDG / `~/.config`）。

use std::io;
use std::path::{Path, PathBuf};

use crate::music::provider::QualityCeiling;

use crate::i18n::Lang;

use super::lyric_control::{ProgressStyle, DEFAULT_OPACITY};

/// 偏好文件名。
pub const PREFS_FILE: &str = "prefs.json";

/// 格式版本。字段语义变化时 +1（旧文件随之作废）。
///
/// ⚠ 只**新增可选字段**时不必 +1：`lang` 就是这样加进来的——老文件里没有它，
/// 解析时回落到默认语言，用户既不会丢音质设置，也不会被强制重置。
pub const PREFS_VERSION: u32 = 1;

/// 用户偏好。
///
/// `Default` **手写**（不用 derive）：`lyric_opacity` 的"无偏好"值是
/// [`DEFAULT_OPACITY`]（66%），而 derive 会给 0（全透明）——那会让第一次启动的
/// 桌面歌词直接看不见底板。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prefs {
    /// 播放音质上限。
    pub quality: QualityCeiling,
    /// 界面语言。
    pub lang: Lang,
    /// 关闭窗口时收进托盘（而不是退出）。
    ///
    /// **默认 `false`**：托盘图标一旦出现就常驻任务栏，属于"装上就撤不掉"的
    /// 东西，用户没主动要之前不该擅自占用系统托盘。
    pub tray: bool,
    /// 桌面歌词背景不透明度（百分比 0..=100）。
    ///
    /// 只影响**外观**，与锁定态无关：锁定不会改它，解锁也不会清它。
    pub lyric_opacity: u8,
    /// 桌面歌词行内进度条样式（默认细白线，彩色跟随主题）。
    pub lyric_progress: ProgressStyle,
    /// 上次的音量（0..=100）。重启后恢复，避免"一开机就炸耳"。
    pub volume: u8,
    /// 是否静音。与 [`Self::volume`] 分开存：取消静音要回到原来的音量。
    pub muted: bool,
    /// 随机播放开关（跨启动保持）。
    pub shuffle: bool,
    /// 循环档位（`0=顺序不循环 / 1=列表循环 / 2=单曲循环`，用 u8 保持 `Prefs: Copy`）。
    /// 与 [`Self::shuffle`] 合成播放模式键的四档；越界值回默认档 0。
    pub repeat: u8,
    /// 主题方案下标（指向 [`super::theme::THEMES`]）。
    ///
    /// 存下标而不是名字：名字是给人看的，会随文案改；下标稳定，且**只许往
    /// [`super::theme::THEMES`] 末尾追加**。越界值夹回最后一套，不 panic。
    pub theme: u8,
    /// 歌词显示中文翻译（接口 `tlyric` / `ytlrc`）。
    ///
    /// 默认 `false`：翻译是**额外一行**，会改变歌词面板的行高节奏，没被要求
    /// 之前不该擅自开启。开关打开后原文旁会多出一行译文。
    pub lyric_translation: bool,
    /// 歌词显示罗马音（接口 `romalrc` / `yromalrc`）。
    ///
    /// 两个开关都开时**译文优先**（用户选定）：罗马音只在某行没有译文时补位。
    pub lyric_roman: bool,
}

impl Prefs {
    /// **全新安装**的初始偏好（磁盘上还没有 `prefs.json`）。
    ///
    /// 与 [`Default`] 的唯一区别是音质：新装落在
    /// [`QualityCeiling::DEFAULT_FOR_NEW_INSTALL`]（320 kbps），而 `Default`
    /// 保持无损作为"配置损坏时的保守回退"。
    ///
    /// 为什么值得分成两个构造函数：无损单曲 30–60 MB，首播等待明显更长，而
    /// 320k 与无损在多数设备上听不出差别。但对**已有**用户绝不能这么改——那
    /// 等于悄悄降质，所以只有"文件不存在"这一条路径用本函数。
    pub fn new_install() -> Self {
        Self {
            quality: QualityCeiling::DEFAULT_FOR_NEW_INSTALL,
            ..Self::default()
        }
    }
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            quality: QualityCeiling::default(),
            lang: Lang::default(),
            tray: false,
            lyric_opacity: DEFAULT_OPACITY,
            lyric_progress: ProgressStyle::default(),
            volume: 72,
            muted: false,
            shuffle: false,
            // 0 = 顺序播放（不循环）：与历史配置一致，也是模式键四档循环的起点
            repeat: 0,
            // 首次启动落在「博丽灵梦」；用户选过主题后以磁盘上的值为准
            theme: super::theme::DEFAULT_THEME as u8,
            // 翻译 / 罗马音默认关闭：它们是额外的行，会改变歌词面板的节奏
            lyric_translation: false,
            lyric_roman: false,
        }
    }
}

/// 偏好文件路径（配置目录不可得时返回 `None`，调用方退化为"不持久化"）。
pub fn prefs_path() -> Option<PathBuf> {
    super::library::config_dir().map(|dir| dir.join(PREFS_FILE))
}

/// 从指定目录读取（不存在 → 新装默认值；损坏/版本不符 → 保守默认值，**不报错**）。
///
/// 两条"没有偏好"的路径必须分开：
/// - 文件**不存在** = 全新安装 → [`Prefs::new_install`]（音质 320k）；
/// - 文件**存在但读不出来** = 老用户的配置坏了 → [`Prefs::default`]（音质保持
///   无损）。混成一条的话，一次 JSON 写坏就会把无损用户静默降级。
pub fn load_from(dir: &Path) -> Prefs {
    let Ok(raw) = std::fs::read_to_string(dir.join(PREFS_FILE)) else {
        return Prefs::new_install();
    };
    parse(&raw).unwrap_or_default()
}

/// 解析偏好 JSON（纯函数，可测）。
///
/// 先剥 UTF-8 BOM：Windows 记事本 / PowerShell 5.1 的 `Set-Content -Encoding utf8`
/// 都会在文件头写 `EF BB BF`，而 `serde_json` 不认它。不剥的后果不是"这一项读不出"，
/// 而是**整份偏好静默回退默认值**（主题、音量、语言、音质全被重置）。
pub fn parse(raw: &str) -> Option<Prefs> {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let version = value.get("version")?.as_u64()? as u32;
    if version != PREFS_VERSION {
        tracing::debug!(
            "偏好文件版本 {} 与当前 {} 不符，按默认值处理",
            version,
            PREFS_VERSION
        );
        return None;
    }
    // 音质档位按 code 存（可读、跨版本稳定、与界面语言无关），未知 code 回默认档
    let quality = value
        .get("quality")
        .and_then(|v| v.as_str())
        .map(|label| {
            QualityCeiling::ALL
                .iter()
                .copied()
                .find(|q| q.code().eq_ignore_ascii_case(label))
                .unwrap_or_default()
        })
        .unwrap_or_default();
    // 语言按代码存；字段缺失（阶段 10 之前写的文件）或取值未知 → 默认中文
    let lang = value
        .get("lang")
        .and_then(|v| v.as_str())
        .and_then(Lang::from_code)
        .unwrap_or_default();
    // 托盘开关：老文件里没有这个键 → `false`（默认不进托盘，与首次安装一致）
    let tray = value.get("tray").and_then(|v| v.as_bool()).unwrap_or(false);
    // 桌面歌词背景不透明度：老文件没有这个键 → 默认 66%；越界值夹回区间
    // （手改配置写成 200 也不该让底板"不透明到看不见"，更不该 panic）
    let lyric_opacity = value
        .get("lyric_opacity")
        .and_then(|v| v.as_u64())
        .map(|pct| super::lyric_control::clamp_opacity(pct as i32))
        .unwrap_or(DEFAULT_OPACITY);
    // 进度条样式按代码存；缺失/未知 → 默认细白线（**不是**彩色）
    let lyric_progress = value
        .get("lyric_progress")
        .and_then(|v| v.as_str())
        .and_then(ProgressStyle::from_code)
        .unwrap_or_default();
    // 缺失/越界的音量 → 默认 72%（与 `PlayerCore::new` 的初始值一致）
    let volume = value
        .get("volume")
        .and_then(|v| v.as_u64())
        .map(|v| v.min(100) as u8)
        .unwrap_or(Prefs::default().volume);
    let muted = value
        .get("muted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let shuffle = value
        .get("shuffle")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // 循环档位：缺失/越界一律回默认档（顺序播放），不猜；`0..=2` 是合法码，
    // 由 `RepeatMode::from_code` 还原成档位
    let repeat = value
        .get("repeat")
        .and_then(|v| v.as_u64())
        .filter(|v| *v <= 2)
        .map(|v| v as u8)
        .unwrap_or(Prefs::default().repeat);
    // 主题下标：缺失 → 默认方案；越界（比如从主题更多的构建里拷来的文件）
    // → 夹回最后一套。这里不能直接信文件里的数，`theme::pal()` 会拿它当下标。
    let theme = value
        .get("theme")
        .and_then(|v| v.as_u64())
        .map(|v| v.min(super::theme::THEMES.len().saturating_sub(1) as u64) as u8)
        .unwrap_or(Prefs::default().theme);
    // 歌词辅助轨开关：老文件里没有这两个键 → `false`（与首次安装一致），
    // 不需要 `PREFS_VERSION` +1
    let lyric_translation = value
        .get("lyric_translation")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let lyric_roman = value
        .get("lyric_roman")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // 阶段 24 起两个开关**互斥**（最多一个为真）。老文件里可能两个都是 true
    // （那时语义是"译文优先、罗马音让位"）→ 归一到译文；不升 PREFS_VERSION，
    // 只是把非法组合拉回合法。
    let (lyric_translation, lyric_roman) =
        super::settings::normalize_gloss(lyric_translation, lyric_roman);
    Some(Prefs {
        quality,
        lang,
        tray,
        lyric_opacity,
        lyric_progress,
        volume,
        muted,
        shuffle,
        repeat,
        theme,
        lyric_translation,
        lyric_roman,
    })
}

/// 序列化为 JSON 文本（纯函数，可测）。
pub fn to_json(prefs: &Prefs) -> String {
    format!(
        "{{\n  \"version\": {},\n  \"quality\": \"{}\",\n  \"lang\": \"{}\",\n  \"tray\": {},\n  \"lyric_opacity\": {},\n  \"lyric_progress\": \"{}\",\n  \"volume\": {},\n  \"muted\": {},\n  \"shuffle\": {},\n  \"repeat\": {},\n  \"theme\": {},\n  \"lyric_translation\": {},\n  \"lyric_roman\": {}\n}}\n",
        PREFS_VERSION,
        prefs.quality.code(),
        prefs.lang.code(),
        prefs.tray,
        prefs.lyric_opacity,
        prefs.lyric_progress.code(),
        prefs.volume,
        prefs.muted,
        prefs.shuffle,
        prefs.repeat,
        prefs.theme,
        prefs.lyric_translation,
        prefs.lyric_roman
    )
}

/// 写入指定目录（`*.tmp` + `rename`）。目录不存在会自动创建。
pub fn save_to(dir: &Path, prefs: &Prefs) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let target = dir.join(PREFS_FILE);
    let tmp = dir.join(format!("{PREFS_FILE}.tmp"));
    std::fs::write(&tmp, to_json(prefs))?;
    std::fs::rename(&tmp, &target)
}

/// 保存到真实配置目录（失败只记日志——设置存不下不该影响播放）。
pub fn save(prefs: &Prefs) {
    let Some(path) = prefs_path() else {
        tracing::warn!("配置目录不可得，音质设置不持久化");
        return;
    };
    let Some(dir) = path.parent() else { return };
    if let Err(e) = save_to(dir, prefs) {
        tracing::warn!("偏好保存失败（不影响播放）：{e}");
    }
}

/// 从真实配置目录读取。
///
/// 配置目录都定位不到时退化成 [`Prefs::new_install`]（而不是 `Default`）：
/// 这种环境下偏好本来就写不下去，取新装档不会永久降质——但和"磁盘上有一份
/// 坏文件"必须区分开，后者仍走保守默认。
pub fn load() -> Prefs {
    prefs_path()
        .as_deref()
        .and_then(|p| p.parent())
        .map(load_from)
        .unwrap_or_else(Prefs::new_install)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("reimu-player-prefs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_utf8_bom_does_not_reset_every_preference() {
        // PowerShell 5.1 的 `Set-Content -Encoding utf8` 会写 BOM；不剥掉时
        // serde_json 整份解析失败，主题/音量/语言会被静默重置成默认值。
        let raw = "\u{feff}{\"version\":1,\"theme\":2,\"volume\":31,\"lang\":\"en\"}";
        let prefs = parse(raw).expect("带 BOM 的合法 JSON 必须能读");
        assert_eq!(prefs.theme, 2);
        assert_eq!(prefs.volume, 31);
        assert_eq!(prefs.lang, Lang::En);
    }

    #[test]
    fn round_trips_every_quality_level() {
        let dir = temp_dir("roundtrip");
        for level in QualityCeiling::ALL {
            save_to(
                &dir,
                &Prefs {
                    quality: level,
                    ..Prefs::default()
                },
            )
            .unwrap();
            assert_eq!(load_from(&dir).quality, level);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn round_trips_both_languages() {
        let dir = temp_dir("lang");
        for lang in Lang::ALL {
            save_to(
                &dir,
                &Prefs {
                    lang,
                    ..Prefs::default()
                },
            )
            .unwrap();
            assert_eq!(load_from(&dir).lang, lang);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_file_without_lang_keeps_quality_and_defaults_language() {
        // 阶段 10 之前写的文件没有 lang 字段：音质必须保住，语言回默认
        let prefs = parse("{\"version\": 1, \"quality\": \"EX-HIGH\"}").unwrap();
        assert_eq!(prefs.quality, QualityCeiling::ExtraHigh);
        assert_eq!(prefs.lang, Lang::Zh);
    }

    #[test]
    fn a_fresh_install_starts_at_extra_high() {
        // 文件不存在 = 全新安装。320k 是刻意的选择：无损单曲 30–60 MB，
        // 首播等待明显更长，而多数设备上听不出差别。
        let dir = temp_dir("missing");
        let prefs = load_from(&dir);
        assert_eq!(prefs.quality, QualityCeiling::ExtraHigh);
        assert_eq!(prefs, Prefs::new_install(), "其余字段仍是常规默认值");
    }

    #[test]
    fn an_existing_lossless_prefs_is_untouched() {
        // 老用户已经选了无损：升级不得改动它
        let dir = temp_dir("keeplossless");
        save_to(
            &dir,
            &Prefs {
                quality: QualityCeiling::Lossless,
                ..Prefs::default()
            },
        )
        .unwrap();
        assert_eq!(load_from(&dir).quality, QualityCeiling::Lossless);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_prefs_never_downgrades_a_lossless_user() {
        // 关键分界：**文件在、但读不出来** ≠ 全新安装。
        // 走 new_install 的话，一次写坏的 JSON 就把无损用户静默降成 320k。
        let dir = temp_dir("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(PREFS_FILE), "{\"version\": 1, \"quality\": ").unwrap();
        assert_eq!(load_from(&dir).quality, QualityCeiling::Lossless);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_two_defaults_are_distinct_concepts() {
        // `Default` = 配置不可信时的保守回退（无损，不降质）
        // `DEFAULT_FOR_NEW_INSTALL` = 从未有过配置时的初始档（320k，省首播流量）
        assert_eq!(QualityCeiling::default(), QualityCeiling::Lossless);
        assert_eq!(
            QualityCeiling::DEFAULT_FOR_NEW_INSTALL,
            QualityCeiling::ExtraHigh
        );
        assert_ne!(
            QualityCeiling::default(),
            QualityCeiling::DEFAULT_FOR_NEW_INSTALL,
            "两者混用会静默降级老用户"
        );
        assert_ne!(Prefs::default(), Prefs::new_install());
    }

    #[test]
    fn version_mismatch_is_discarded() {
        // 宁可回默认，也不要把错位字段读进来
        assert!(parse("{\"version\": 99, \"quality\": \"STANDARD\"}").is_none());
        // 磁盘上确实有一份版本不符的文件：走保守默认（无损），不是新装档
        let dir = temp_dir("nomatch");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(PREFS_FILE), "{\"version\": 99}").unwrap();
        assert_eq!(load_from(&dir), Prefs::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_json_falls_back_to_default() {
        assert!(parse("not json").is_none());
        assert!(parse("{\"quality\": \"LOSSLESS\"}").is_none()); // 缺 version
    }

    #[test]
    fn unknown_quality_label_falls_back_to_default_level() {
        let prefs = parse("{\"version\": 1, \"quality\": \"SUPER-DUPER\"}").unwrap();
        assert_eq!(prefs.quality, QualityCeiling::default());
        // 大小写不敏感（手改配置文件不该炸）
        let prefs = parse("{\"version\": 1, \"quality\": \"standard\"}").unwrap();
        assert_eq!(prefs.quality, QualityCeiling::Standard);
        // 未知语言同样回默认，不影响音质
        let prefs = parse("{\"version\": 1, \"quality\": \"HIGHER\", \"lang\": \"xx\"}").unwrap();
        assert_eq!(prefs.quality, QualityCeiling::Higher);
        assert_eq!(prefs.lang, Lang::Zh);
    }

    #[test]
    fn save_is_atomic_and_leaves_no_tmp() {
        let dir = temp_dir("atomic");
        save_to(
            &dir,
            &Prefs {
                quality: QualityCeiling::Higher,
                lang: Lang::En,
                tray: true,
                ..Prefs::default()
            },
        )
        .unwrap();
        assert!(dir.join(PREFS_FILE).exists());
        assert!(!dir.join(format!("{PREFS_FILE}.tmp")).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tray_defaults_to_off_for_new_and_legacy_files() {
        // 「默认关闭进入托盘模式」是产品硬要求：新装与老配置都必须是不进托盘。
        // 老文件里没有 tray 键——这条断言就是防"顺手改成 unwrap_or(true)"。
        assert!(!Prefs::default().tray);
        let legacy =
            parse("{\"version\": 1, \"quality\": \"LOSSLESS\", \"lang\": \"zh\"}").unwrap();
        assert!(!legacy.tray, "老配置不得被读成进托盘");
        let off = parse("{\"version\": 1, \"quality\": \"LOSSLESS\", \"tray\": false}").unwrap();
        assert!(!off.tray);
    }

    #[test]
    fn tray_choice_round_trips() {
        let dir = temp_dir("tray");
        for tray in [false, true] {
            save_to(
                &dir,
                &Prefs {
                    tray,
                    ..Prefs::default()
                },
            )
            .unwrap();
            assert_eq!(load_from(&dir).tray, tray);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_bool_tray_value_falls_back_to_off() {
        // 手改配置写成字符串也不该炸，且仍按"不进托盘"处理
        let prefs = parse("{\"version\": 1, \"tray\": \"yes\"}").unwrap();
        assert!(!prefs.tray);
    }

    #[test]
    fn lyric_defaults_are_white_line_and_66_percent() {
        // 两条产品硬要求都钉在这里：
        // ① 彩色进度条不再是默认（默认细白线）；
        // ② 默认底板不透明度必须等于旧版硬编码值，升级后视觉不跳变。
        let d = Prefs::default();
        assert_eq!(d.lyric_progress, ProgressStyle::Line);
        assert_ne!(d.lyric_progress, ProgressStyle::Acid, "彩色不得是默认");
        assert_eq!(d.lyric_opacity, DEFAULT_OPACITY);
        assert_eq!(DEFAULT_OPACITY, 66);
    }

    #[test]
    fn legacy_file_without_lyric_keys_keeps_them_default() {
        // 阶段 15 之前写的文件没有这两个键：不得报错，也不得变成彩色/全透明
        let prefs =
            parse("{\"version\": 1, \"quality\": \"LOSSLESS\", \"lang\": \"zh\", \"tray\": true}")
                .unwrap();
        assert_eq!(prefs.lyric_progress, ProgressStyle::Line);
        assert_eq!(prefs.lyric_opacity, DEFAULT_OPACITY);
        assert!(prefs.tray, "新增字段不得影响既有字段");
    }

    #[test]
    fn both_aux_switches_on_in_an_old_file_is_normalized() {
        // 阶段 23 及以前两个开关可以同时为真（译文优先）。升级后必须归一成互斥，
        // 否则界面上会出现两个按钮同时点亮、却只显示译文。
        let prefs = parse("{\"version\": 1, \"lyric_translation\": true, \"lyric_roman\": true}")
            .unwrap();
        assert!(prefs.lyric_translation);
        assert!(!prefs.lyric_roman, "两个都开时必须归一成只有译文");
    }

    #[test]
    fn lyric_prefs_round_trip_through_disk() {
        let dir = temp_dir("lyric");
        for style in ProgressStyle::ALL {
            for pct in [0u8, 30, 100] {
                save_to(
                    &dir,
                    &Prefs {
                        lyric_opacity: pct,
                        lyric_progress: style,
                        ..Prefs::default()
                    },
                )
                .unwrap();
                let back = load_from(&dir);
                assert_eq!(back.lyric_opacity, pct);
                assert_eq!(back.lyric_progress, style);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn playback_mode_prefs_round_trip_and_default_for_legacy() {
        let dir = temp_dir("playmode");
        let prefs = Prefs {
            volume: 38,
            muted: true,
            shuffle: true,
            repeat: 2,
            ..Prefs::default()
        };
        save_to(&dir, &prefs).unwrap();
        let back = load_from(&dir);
        assert_eq!(back.volume, 38);
        assert!(back.muted);
        assert!(back.shuffle);
        assert_eq!(back.repeat, 2);
        let _ = std::fs::remove_dir_all(&dir);

        // 老配置没有这些键 → 默认值，不能把音量读成 0
        let legacy = parse("{\"version\": 1, \"quality\": \"HIGHER\"}").unwrap();
        assert_eq!(legacy.volume, 72);
        assert!(!legacy.muted);
        assert!(!legacy.shuffle);
        assert_eq!(legacy.repeat, 0, "缺 repeat 键 → 默认顺序播放（不循环）");
        // 手改越界值 → 夹回 / 回默认
        let junk = parse("{\"version\": 1, \"volume\": 900, \"repeat\": 7}").unwrap();
        assert_eq!(junk.volume, 100);
        assert_eq!(junk.repeat, 0, "越界的循环码不能留成非法值");
        // 三档码值都要原样往返：0 是「顺序播放」档，不是非法值
        for code in 0..=2u8 {
            let back = parse(&format!("{{\"version\": 1, \"repeat\": {code}}}")).unwrap();
            assert_eq!(back.repeat, code, "循环码 {code} 应原样读出");
        }
    }

    #[test]
    fn corrupted_lyric_values_fall_back_safely() {
        // 手改配置：透明度写成 300 / 负数 / 字符串，进度条写成未知代码
        let over = parse("{\"version\": 1, \"lyric_opacity\": 300}").unwrap();
        assert_eq!(over.lyric_opacity, 100, "越界透明度必须夹回上限");
        let junk = parse("{\"version\": 1, \"lyric_opacity\": \"half\"}").unwrap();
        assert_eq!(junk.lyric_opacity, DEFAULT_OPACITY);
        let neg = parse("{\"version\": 1, \"lyric_opacity\": -5}").unwrap();
        assert_eq!(
            neg.lyric_opacity, DEFAULT_OPACITY,
            "负数不是合法 u64 → 默认"
        );
        let style = parse("{\"version\": 1, \"lyric_progress\": \"rainbow\"}").unwrap();
        assert_eq!(style.lyric_progress, ProgressStyle::Line);
    }

    #[test]
    fn theme_index_round_trips_and_clamps() {
        // 主题存的是下标；默认值是 1 号「博丽灵梦」，用户选过哪套就原样往返
        let dir = temp_dir("theme");
        let last = crate::gui::theme::THEMES.len() - 1;
        for index in 0..=last as u8 {
            save_to(
                &dir,
                &Prefs {
                    theme: index,
                    ..Prefs::default()
                },
            )
            .unwrap();
            assert_eq!(
                load_from(&dir).theme,
                index,
                "主题下标 {index} 必须原样往返"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);

        let legacy = parse("{\"version\": 1, \"quality\": \"HIGHER\"}").unwrap();
        assert_eq!(
            legacy.theme,
            crate::gui::theme::DEFAULT_THEME as u8,
            "老配置没有 theme 键 → 首次启动默认档（博丽灵梦）"
        );
        let over = parse(&format!("{{\"version\": 1, \"theme\": {}}}", last + 5)).unwrap();
        assert_eq!(
            over.theme as usize, last,
            "越界下标必须夹到最后一套，不能 panic"
        );
        let junk = parse("{\"version\": 1, \"theme\": \"sanae\"}").unwrap();
        assert_eq!(
            junk.theme,
            crate::gui::theme::DEFAULT_THEME as u8,
            "名字写进数字键位 → 默认档"
        );
    }
}
