//! 设置面板：**账户登录** + **界面语言** + **播放音质上限** + **桌面歌词**。
//!
//! 职责边界：本模块只做**展示与取值**（纯函数式面板），副作用经
//! [`SettingsAction`] 回传由 [`crate::gui::app::ReimuApp`] 执行，
//! 保持 immediate-mode 单向数据流。
//!
//! 内容变更史（重要，避免回退）：
//! - 阶段 G 这里曾是「SEARCH TIMEOUT」滑杆。用户要求删除——超时是工程参数，
//!   不是用户要调的东西，而且它的 `APPLY` 会**重建 Provider**，连带丢掉登录 Cookie。
//! - 阶段 10：改成「播放音质 + 桌面歌词（预留）」。
//! - 阶段 11：**登录入口从标题栏 chip 移到这里**（用户要求），并新增中英切换。
//!
//! 版式：一行一项（标签固定占左侧 [`LABEL_W`]，控件跟在右边）。
//! 为什么不用"标签在上、控件在下"的竖排：四项竖排要 158px，会把磁带架挤掉；
//! 横排一行 14px，四项 + 一条注释 = 89px，正好塞进 Track Info 面板。
//!
//! 设计约束：颜色/字号只经 [`crate::gui::theme`] 与 [`crate::gui::widgets`]，
//! 与其余面板风格统一（System 6 字距标签 + 反白按钮）。

use eframe::egui::{vec2, Align2, Layout, Rect, Sense, Ui, UiBuilder};

use crate::music::provider::QualityCeiling;

use super::lyric_control::ProgressStyle;
use super::theme::*;
use super::widgets;
use crate::i18n::{pick, Lang};

/// 设置行的标签列宽（px）。
///
/// 与 [`widgets::INFO_LABEL_W`] 不同：这里是设置项名（ACCOUNT / DESKTOP LYRIC），
/// 比 TRACK/ARTIST 这类短词长，需要更宽。
pub const LABEL_W: f32 = 92.0;

/// 设置区在 Track Info 面板里**预留**的高度（px）。
///
/// 由 [`settings_panel`] 的内容决定，改动内容必须重算这里——留少了
/// `OFF` 按钮会被面板底边切掉（阶段 10 踩过：留 88 时差 11px）。
/// 单测 [`tests::settings_panel_fits_its_reserved_height`] 用真实 egui
/// 上下文量一遍，防止再靠肉眼估。
///
/// 当前内容实测占用 216px（阶段 23 把 LYRIC TRANS / ROMAJI 两行并成一行
/// 「歌词转译」，比阶段 22 的 237 少了 21px），留 3 给字体度量抖动——
/// 再大就白吃磁带架的高度了。
pub const PANEL_H: f32 = 219.0;

/// 设置面板动作（副作用由 app 执行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsAction {
    /// 无操作。
    None,
    /// 切换播放音质上限。
    SetQuality(QualityCeiling),
    /// 开关桌面歌词分发。
    SetDesktopLyric(bool),
    /// 切换桌面歌词的进度条样式（细白线 / 关 / 彩色）。
    ///
    /// 锁定按钮**不在这里**：它已经移到歌词窗口自带的控件条上
    /// （见 [`super::lyric_window`]），那才是用户眼睛盯着的地方。
    SetDesktopLyricProgress(ProgressStyle),
    /// 开关全局媒体键（同时开关 SMTC 媒体会话）。
    SetMediaKeys(bool),
    /// 开关托盘图标 / 关闭窗口时收进托盘。
    SetTray(bool),
    /// 歌词是否显示中文翻译（作为辅助行显示在原文旁）。
    SetLyricTranslation(bool),
    /// 歌词是否显示罗马音（两个开关都开时译文优先）。
    SetLyricRoman(bool),
    /// 切换界面语言。
    SetLang(Lang),
    /// 切换主题配色（下标指向 [`crate::gui::theme::THEMES`]）。
    SetTheme(usize),
    /// 清扫 `%TEMP%` 下异常退出（强制结束/崩溃）留下的播放暂存目录。
    ///
    /// 手动触发：启动时自动删别人的临时目录风险太高，时机交给用户。
    SweepTempFiles,
    /// 打开登录面板（账户行被点击）。
    OpenLogin,
}

/// 面板视图（纯只读快照；改动一律经 [`SettingsAction`] 回传）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsView {
    /// 当前生效的音质上限。
    pub quality: QualityCeiling,
    /// 桌面歌词分发是否开启。
    pub desktop_lyric: bool,
    /// 桌面歌词行内进度条样式。
    pub lyric_progress: ProgressStyle,
    /// 是否已有渲染端挂接（现阶段恒 `false`，如实显示）。
    pub hook_ready: bool,
    /// 全局媒体键 + SMTC 是否开启。
    pub media_keys: bool,
    /// 是否启用托盘图标（关闭窗口时收进托盘）。
    pub tray: bool,
    /// 歌词是否显示中文翻译。
    pub lyric_translation: bool,
    /// 歌词是否显示罗马音。
    pub lyric_roman: bool,
    /// 当前界面语言。
    pub lang: Lang,
    /// 当前主题下标（指向 [`crate::gui::theme::THEMES`]）。
    pub theme: usize,
    /// 已登录昵称；`None` = 未登录。
    pub account: Option<String>,
}

/// 设置行的 id 盐（每行一个，见 [`row`]）。
const SALT_ACCOUNT: &str = "row-account";
const SALT_LANG: &str = "row-lang";
const SALT_THEME: &str = "row-theme";
const SALT_QUALITY: &str = "row-quality";
const SALT_DESKTOP_LYRIC: &str = "row-desktop-lyric";
const SALT_MEDIA_KEYS: &str = "row-media-keys";
const SALT_TRAY: &str = "row-tray";
const SALT_LYRIC_GLOSS: &str = "row-lyric-gloss";
const SALT_TEMP_FILES: &str = "row-temp-files";

/// 一行设置：左侧固定宽标签 + 右侧控件（同一水平线，垂直居中）。
///
/// `salt` 是这行控件的 id 作用域，**必须每行不同**。
///
/// ⚠ egui 的坑（0.37 `Ui::new_child`）：子 Ui 不给 `id_salt` 时默认盐是常量
/// `"child"`，派生出的 `scope_id = parent.scope_id.with("child")`，**与
/// `next_auto_id_salt` 无关**（`unique_id` 才吃那个计数器）。于是同一父 Ui 下
/// 所有不带盐的子 Ui——包括 `ui.horizontal` 的子 Ui——共用一个 `scope_id()`，
/// 控件 id 只能靠后缀字符串区分。一旦两处后缀撞名，**一次点击会同时触发两个
/// 控件**，而在 [`settings_panel`] 里后写的动作还会覆盖先写的（切语言会吞掉
/// 开登录）。给每行显式盐，让 id 在作用域层就分开。
fn row(ui: &mut Ui, salt: &str, label: &str, body: impl FnOnce(&mut Ui)) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 14.0), Sense::hover());
    widgets::paint_spec_text(
        ui,
        rect.left_center(),
        Align2::LEFT_CENTER,
        label,
        FONT_TINY,
        TRACKING_TINY,
        pal().mid,
        true,
        false,
    );
    let mut child = ui.new_child(
        UiBuilder::new()
            .id_salt(salt)
            .max_rect(Rect::from_min_max(
                eframe::egui::pos2(rect.min.x + LABEL_W, rect.min.y),
                rect.max,
            ))
            .layout(Layout::left_to_right(eframe::egui::Align::Center)),
    );
    body(&mut child);
}

/// 账户行：未登录显示 LOGIN 按钮，已登录显示昵称（点击仍可打开面板）。
fn account_row(ui: &mut Ui, view: &SettingsView) -> bool {
    let mut clicked = false;
    row(
        ui,
        SALT_ACCOUNT,
        pick(view.lang, "账户", "ACCOUNT"),
        |ui| {
            let text = match &view.account {
                // 昵称过长按字截断（中文名 7 字足够区分账号）
                Some(name) => name.chars().take(7).collect::<String>(),
                None => pick(view.lang, "登录", "LOGIN").to_string(),
            };
            if widgets::tiny_button(
                ui,
                ui.scope_id().with("btn-account"),
                &text,
                view.account.is_some(),
            )
            .clicked()
            {
                clicked = true;
            }
            if view.account.is_some() {
                ui.add_space(8.0);
                ui.label(widgets::spec_job(
                    pick(view.lang, "已登录", "SIGNED IN"),
                    FONT_TINY,
                    TRACKING_TINY,
                    pal().mid,
                    true,
                ));
            }
        },
    );
    clicked
}

/// 语言行：`中文 / ENGLISH` 二选一。
fn lang_row(ui: &mut Ui, view: &SettingsView) -> Option<Lang> {
    let mut picked = None;
    row(ui, SALT_LANG, pick(view.lang, "语言", "LANGUAGE"), |ui| {
        for (i, lang) in Lang::ALL.iter().enumerate() {
            if i > 0 {
                ui.add_space(4.0);
            }
            if widgets::tiny_button(
                ui,
                ui.scope_id().with("btn-lang").with(i),
                lang.label(),
                *lang == view.lang,
            )
            .clicked()
                && *lang != view.lang
            {
                picked = Some(*lang);
            }
        }
    });
    picked
}

/// 主题行：一套配色一个按钮，按下去整界面换色（不需要重启）。
///
/// 名字**不随语言翻译**：「东风谷早苗」是方案名而不是控件文案，翻成
/// "Kochiya Sanae" 反而让人对不上设计稿里那套色。
fn theme_row(ui: &mut Ui, view: &SettingsView) -> Option<usize> {
    let mut picked = None;
    row(ui, SALT_THEME, pick(view.lang, "主题", "THEME"), |ui| {
        for (i, theme) in crate::gui::theme::THEMES.iter().enumerate() {
            if i > 0 {
                ui.add_space(4.0);
            }
            if widgets::tiny_button(
                ui,
                ui.scope_id().with("btn-theme").with(i),
                theme.name,
                i == view.theme,
            )
            .clicked()
                && i != view.theme
            {
                picked = Some(i);
            }
        }
    });
    picked
}

/// 音质行：`STANDARD / HIGHER / EX-HIGH / LOSSLESS` 四选一。
fn quality_row(ui: &mut Ui, view: &SettingsView) -> Option<QualityCeiling> {
    let mut picked = None;
    row(
        ui,
        SALT_QUALITY,
        pick(view.lang, "音质", "QUALITY"),
        |ui| {
            for (i, level) in QualityCeiling::ALL.iter().enumerate() {
                if i > 0 {
                    ui.add_space(4.0);
                }
                if widgets::tiny_button(
                    ui,
                    ui.scope_id().with("btn-quality").with(i),
                    level.label(view.lang),
                    *level == view.quality,
                )
                .clicked()
                    && *level != view.quality
                {
                    picked = Some(*level);
                }
            }
        },
    );
    picked
}

/// 桌面歌词行：开关 + 进度条样式（+ 接口状态，仅在未接入时显示）。
///
/// **锁定按钮不在这里**：阶段 16 已按用户要求把它移到歌词窗口自带的控件条上。
/// 那一条是**永不鼠标穿透**的小窗口，所以锁定态下依然点得到——这也顺带解决了
/// 阶段 14 的死锁（锁定后整个悬浮窗穿透，只能回这里解锁）。
///
/// 行内按钮而不是新起一行，是为了不把 [`PANEL_H`] 再撑高 20px（磁带架会跟着让位）。
fn desktop_lyric_row(ui: &mut Ui, view: &SettingsView) -> Option<SettingsAction> {
    let mut action = None;
    row(
        ui,
        SALT_DESKTOP_LYRIC,
        pick(view.lang, "桌面歌词", "DESKTOP LYRIC"),
        |ui| {
            let on_off = if view.desktop_lyric {
                pick(view.lang, "开", "ON")
            } else {
                pick(view.lang, "关", "OFF")
            };
            if widgets::tiny_button(
                ui,
                ui.scope_id().with("btn-desktop-lyric"),
                on_off,
                view.desktop_lyric,
            )
            .clicked()
            {
                action = Some(SettingsAction::SetDesktopLyric(!view.desktop_lyric));
            }
            ui.add_space(6.0);
            // 按钮上写**当前样式**（不是动作），悬停才说明它是什么
            let label = format!(
                "{}: {}",
                pick(view.lang, "进度条", "BAR"),
                view.lyric_progress.label(view.lang)
            );
            if widgets::tiny_button(
                ui,
                ui.scope_id().with("btn-desktop-lyric-bar"),
                &label,
                view.lyric_progress != ProgressStyle::Off,
            )
            .on_hover_text(view.lyric_progress.hint(view.lang))
            .clicked()
            {
                action = Some(SettingsAction::SetDesktopLyricProgress(
                    view.lyric_progress.next(),
                ));
            }
            // 接口状态只在真的没接入时才占地方（正常情况恒为已接入）
            if !view.hook_ready {
                ui.add_space(6.0);
                ui.label(widgets::spec_job(
                    pick(view.lang, "接口: 未接入", "HOOK: NONE"),
                    FONT_TINY,
                    TRACKING_TINY,
                    pal().mid,
                    true,
                ));
            }
        },
    );
    action
}

/// 全局媒体键 + SMTC 行：开关 + 说明。
///
/// 标题写「媒体键\SMTC」：两者对用户是同一件事（按播放键要能动、系统浮层要
/// 显示曲目），所以一个开关同时管；分开列会让「为什么蓝牙耳机上的键不生效」
/// 变成两个开关的排列组合问题。
fn media_keys_row(ui: &mut Ui, view: &SettingsView) -> bool {
    let mut toggled = false;
    row(
        ui,
        SALT_MEDIA_KEYS,
        pick(view.lang, "媒体键\\SMTC", "MEDIA KEYS\\SMTC"),
        |ui| {
            let on_off = if view.media_keys {
                pick(view.lang, "开", "ON")
            } else {
                pick(view.lang, "关", "OFF")
            };
            if widgets::tiny_button(
                ui,
                ui.scope_id().with("btn-media-keys"),
                on_off,
                view.media_keys,
            )
            .clicked()
            {
                toggled = true;
            }
            ui.add_space(8.0);
            ui.label(widgets::spec_job(
                pick(
                    view.lang,
                    "键盘热键 + 系统媒体浮层",
                    "HOTKEYS + SYSTEM MEDIA POPUP",
                ),
                FONT_TINY,
                TRACKING_TINY,
                pal().mid,
                true,
            ));
        },
    );
    toggled
}

/// 歌词转译的两个开关（互斥单选）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlossKind {
    /// 译文。
    Translation,
    /// 罗马音。
    Roman,
}

/// 把（可能非法的）两个开关归一到互斥：两个都为真时**保留译文**。
///
/// 用在落盘读取处：阶段 23 及以前两个开关可以同时为真（语义是"译文优先、
/// 罗马音让位"），升级后必须拉回合法状态，否则界面上会出现两个按钮同时点亮
/// 却只显示译文。优先级与 [`crate::music::lyrics::Lyrics::aux_text_for`] 一致。
pub fn normalize_gloss(translation: bool, roman: bool) -> (bool, bool) {
    if translation {
        (true, false)
    } else {
        (false, roman)
    }
}

/// 互斥切换后的新状态（纯函数）。
///
/// 语义（阶段 24 需求：两个按钮互斥，任意时刻最多一个激活）：
///
/// | 当前 | 点谁 | 结果 |
/// | --- | --- | --- |
/// | 都没开 | 译文 | 译文开 |
/// | 都没开 | 罗马音 | 罗马音开 |
/// | 译文开 | 译文（点自己） | **两个都关** |
/// | 译文开 | 罗马音 | 罗马音开、译文关 |
/// | 罗马音开 | 译文 | 译文开、罗马音关 |
///
/// "点自己 → 两个都关"是必需的：否则一旦选中任意一个，就再也没有回到
/// "不要辅助行"的路了。
///
/// 返回值恒满足 `!(translation && roman)`。
pub fn exclusive_gloss(translation: bool, roman: bool, pick: GlossKind) -> (bool, bool) {
    match pick {
        // 翻转自己；对方一律关掉（互斥）。点自己时自己变关 → 两个都关。
        GlossKind::Translation => (!translation, false),
        GlossKind::Roman => (false, !roman),
    }
}

/// 歌词转译行：「译文」「罗马音」两个开关**互斥并排**在同一行（默认都不开）。
///
/// 阶段 23 合并：原来它们是两行、各带一条"上行/下行"的说明文字。两行说的是
/// 同一件事（给歌词补一行注音 / 翻译），拆成两行既费面板高度、又让人以为它们
/// 互不相干。抬头统一叫「歌词转译」，两个按钮各自只写自己的名字。
///
/// 说明文字不再写"上行 / 下行"：阶段 23 之后主窗歌词面板与桌面歌词窗的上下
/// 顺序**不再一致**（桌面歌词按"样式回退"回了阶段 21 的版面），一句方向性
/// 文案必然说错一边。方向信息改成悬停提示里的中性描述。
///
/// 阶段 24：两个按钮改成**互斥单选**（见 [`exclusive_gloss`]）——原来可以同时开、
/// 译文优先，现在点一个就关掉另一个，再点自己则两个都关。
fn lyric_gloss_row(ui: &mut Ui, view: &SettingsView) -> Option<SettingsAction> {
    let mut action = None;
    row(
        ui,
        SALT_LYRIC_GLOSS,
        pick(view.lang, "歌词转译", "LYRIC GLOSS"),
        |ui| {
            // 两个按钮共用一个 `row` 的 id 作用域，后缀必须不同（见 `row` 的注释）
            if widgets::tiny_button(
                ui,
                ui.scope_id().with("btn-lyric-translation"),
                pick(view.lang, "译文", "TRANS"),
                view.lyric_translation,
            )
            .on_hover_text(pick(
                view.lang,
                "显示歌词译文（与罗马音互斥）",
                "Show the translation (exclusive with romaji)",
            ))
            .clicked()
            {
                action = Some(SettingsAction::SetLyricTranslation(!view.lyric_translation));
            }
            ui.add_space(6.0);
            if widgets::tiny_button(
                ui,
                ui.scope_id().with("btn-lyric-roman"),
                pick(view.lang, "罗马音", "ROMAJI"),
                view.lyric_roman,
            )
            .on_hover_text(pick(
                view.lang,
                "显示罗马音（与译文互斥）",
                "Show romaji (exclusive with translation)",
            ))
            .clicked()
            {
                action = Some(SettingsAction::SetLyricRoman(!view.lyric_roman));
            }
        },
    );
    action
}

/// 托盘行：开关 + 说明（默认关闭）。
fn tray_row(ui: &mut Ui, view: &SettingsView) -> bool {
    let mut toggled = false;
    row(ui, SALT_TRAY, pick(view.lang, "托盘", "TRAY"), |ui| {
        let on_off = if view.tray {
            pick(view.lang, "开", "ON")
        } else {
            pick(view.lang, "关", "OFF")
        };
        if widgets::tiny_button(ui, ui.scope_id().with("btn-tray"), on_off, view.tray).clicked() {
            toggled = true;
        }
        ui.add_space(8.0);
        ui.label(widgets::spec_job(
            pick(
                view.lang,
                "关闭窗口时收进托盘，不退出",
                "CLOSE HIDES TO TRAY INSTEAD OF QUITTING",
            ),
            FONT_TINY,
            TRACKING_TINY,
            pal().mid,
            true,
        ));
    });
    toggled
}

/// 临时文件行：清扫异常退出（强制结束/崩溃）留下的播放缓存。
///
/// **只在用户点「清扫」时执行**，启动时不自动跑：删除是破坏性操作，多开时
/// 还可能碰到另一个实例正在写的目录。底层只清超过 1 小时没动静、且不属于
/// 本进程的 `reimuplayer_online_<pid>` 目录（见
/// [`crate::music::online_playback::sweep_stale_scratch`]）。
fn temp_files_row(ui: &mut Ui, view: &SettingsView) -> bool {
    let mut clicked = false;
    row(
        ui,
        SALT_TEMP_FILES,
        pick(view.lang, "临时文件", "TEMP FILES"),
        |ui| {
            if widgets::tiny_button(
                ui,
                ui.scope_id().with("btn-temp-files"),
                pick(view.lang, "清扫", "CLEAN"),
                false,
            )
            .on_hover_text(pick(
                view.lang,
                "删除非本进程、超过 1 小时未写入的播放缓存目录",
                "REMOVE PLAYBACK CACHE DIRS >1H OLD (OTHER PROCESSES)",
            ))
            .clicked()
            {
                clicked = true;
            }
            ui.add_space(8.0);
            ui.label(widgets::spec_job(
                pick(view.lang, "异常退出的播放缓存", "STALE PLAYBACK CACHE"),
                FONT_TINY,
                TRACKING_TINY,
                pal().mid,
                true,
            ));
        },
    );
    clicked
}

/// 设置面板。
pub fn settings_panel(ui: &mut Ui, view: &SettingsView) -> SettingsAction {
    let mut action = SettingsAction::None;

    if account_row(ui, view) {
        action = SettingsAction::OpenLogin;
    }
    ui.add_space(6.0);
    if let Some(lang) = lang_row(ui, view) {
        action = SettingsAction::SetLang(lang);
    }
    ui.add_space(6.0);
    if let Some(index) = theme_row(ui, view) {
        action = SettingsAction::SetTheme(index);
    }
    ui.add_space(6.0);
    if let Some(level) = quality_row(ui, view) {
        action = SettingsAction::SetQuality(level);
    }
    ui.add_space(2.0);
    // 说明必须写清"上限"语义：不是绕过会员，接口按身份自己降级
    ui.label(widgets::spec_job(
        &format!(
            "{} - {}",
            view.quality.detail(),
            pick(view.lang, "按账号权限封顶", "capped by account")
        ),
        FONT_TINY,
        TRACKING_TINY,
        pal().mid,
        true,
    ));
    ui.add_space(4.0);
    if let Some(a) = desktop_lyric_row(ui, view) {
        action = a;
    }
    ui.add_space(6.0);
    if let Some(a) = lyric_gloss_row(ui, view) {
        action = a;
    }
    ui.add_space(6.0);
    if media_keys_row(ui, view) {
        action = SettingsAction::SetMediaKeys(!view.media_keys);
    }
    ui.add_space(6.0);
    if tray_row(ui, view) {
        action = SettingsAction::SetTray(!view.tray);
    }
    ui.add_space(6.0);
    if temp_files_row(ui, view) {
        action = SettingsAction::SweepTempFiles;
    }

    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_levels_are_ordered_low_to_high() {
        // 面板按 ALL 顺序排按钮，顺序即"由低到高"，错了用户会以为档位颠倒
        assert_eq!(QualityCeiling::ALL.len(), 4);
        assert_eq!(QualityCeiling::ALL[0], QualityCeiling::Standard);
        assert_eq!(QualityCeiling::ALL[3], QualityCeiling::Lossless);
        let brs: Vec<u32> = QualityCeiling::ALL
            .iter()
            .map(|q| q.br().parse::<u32>().unwrap())
            .collect();
        assert!(brs.windows(2).all(|w| w[0] < w[1]), "码率必须递增: {brs:?}");
    }

    #[test]
    fn quality_index_roundtrips_and_clamps() {
        for level in QualityCeiling::ALL {
            assert_eq!(QualityCeiling::from_index(level.index()), level);
        }
        // 越界不 panic，回落到默认档
        assert_eq!(QualityCeiling::from_index(200), QualityCeiling::default());
    }

    #[test]
    fn settings_action_is_plain_data() {
        // 面板动作可拷贝可比较（app 侧 match 分发的前提）
        let a = SettingsAction::SetQuality(QualityCeiling::ExtraHigh);
        let b = a;
        assert_eq!(a, b);
        assert_ne!(a, SettingsAction::None);
        assert_ne!(a, SettingsAction::SetDesktopLyric(true));
        assert_ne!(
            a,
            SettingsAction::SetDesktopLyricProgress(ProgressStyle::Acid)
        );
        assert_ne!(a, SettingsAction::SetLang(Lang::En));
        assert_ne!(a, SettingsAction::SweepTempFiles);
        assert_ne!(a, SettingsAction::OpenLogin);
    }

    /// 在真实 egui 上下文里跑一次面板，返回它实际占用的高度（px）。
    ///
    /// 不用肉眼估高度：`ui.cursor()` 的位移就是真实占位，
    /// 与 [`PANEL_H`] 的推算值对不上时会立刻暴露。
    fn measure_panel(view: &SettingsView) -> f32 {
        let ctx = eframe::egui::Context::default();
        let raw = eframe::egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                eframe::egui::Pos2::ZERO,
                vec2(520.0, 900.0),
            )),
            ..eframe::egui::RawInput::default()
        };
        let mut used = 0.0;
        let out = ctx.run_ui(raw, |ui| {
            ui.set_width(480.0);
            let top = ui.cursor().min.y;
            settings_panel(ui, view);
            used = ui.cursor().min.y - top;
        });
        out.drop_without_applying_deltas();
        used
    }

    fn view(lang: Lang, account: Option<String>) -> SettingsView {
        SettingsView {
            quality: QualityCeiling::Lossless,
            desktop_lyric: false,
            lyric_progress: ProgressStyle::Line,
            lyric_translation: false,
            lyric_roman: false,
            hook_ready: false,
            media_keys: true,
            tray: false,
            lang,
            theme: 0,
            account,
        }
    }

    #[test]
    fn settings_panel_fits_its_reserved_height() {
        // 中/英 + 已登录/未登录四种组合都要塞得进 PANEL_H，
        // 否则 Track Info 面板底边会切掉最后一行
        for lang in Lang::ALL {
            for account in [None, Some("耳朵".to_string())] {
                let used = measure_panel(&view(lang, account.clone()));
                assert!(
                    used <= PANEL_H,
                    "lang={lang:?} account={account:?} 占用 {used} > 预留 {PANEL_H}"
                );
            }
        }
    }

    #[test]
    fn reserved_height_leaves_no_slack_for_another_row() {
        // 反向保护：预留值不该大到离谱（每多 20px 就少吃一行磁带）
        let used = measure_panel(&view(Lang::En, None));
        assert!(
            PANEL_H - used < 24.0,
            "预留 {PANEL_H} 与实际 {used} 差得太多，磁带架白让了高度"
        );
    }

    #[test]
    fn gloss_switches_are_exclusive() {
        // 阶段 24：任意时刻最多一个为真。穷举全部合法前置态 × 两个按钮，
        // 顺便钉住"点自己 = 翻转"（否则回不到两个都关的状态）。
        for translation in [false, true] {
            for roman in [false, true] {
                if translation && roman {
                    continue; // 非法前置态，由 normalize_gloss 负责
                }
                for pick in [GlossKind::Translation, GlossKind::Roman] {
                    let (t, r) = exclusive_gloss(translation, roman, pick);
                    assert!(
                        !(t && r),
                        "互斥被破坏：({translation},{roman}) 点 {pick:?} -> ({t},{r})"
                    );
                    let (before, after) = match pick {
                        GlossKind::Translation => (translation, t),
                        GlossKind::Roman => (roman, r),
                    };
                    assert_eq!(after, !before, "点自己必须翻转：{pick:?}");
                }
            }
        }
    }

    #[test]
    fn normalize_gloss_keeps_translation_when_both_are_on() {
        // 老文件可能两个都开（那时是"译文优先、罗马音让位"）→ 归一成只有译文
        assert_eq!(normalize_gloss(true, true), (true, false));
        assert_eq!(normalize_gloss(true, false), (true, false));
        assert_eq!(normalize_gloss(false, true), (false, true));
        assert_eq!(normalize_gloss(false, false), (false, false));
    }

    #[test]
    fn every_action_variant_is_distinct() {
        // 新增动作时若不慎让两个变体相等，match 分支会互相吞掉
        let all = [
            SettingsAction::None,
            SettingsAction::SetQuality(QualityCeiling::Standard),
            SettingsAction::SetDesktopLyric(false),
            SettingsAction::SetDesktopLyricProgress(ProgressStyle::Acid),
            SettingsAction::SetLyricTranslation(true),
            SettingsAction::SetLyricRoman(true),
            SettingsAction::SetMediaKeys(false),
            SettingsAction::SetTray(false),
            SettingsAction::SetLang(Lang::Zh),
            SettingsAction::SetTheme(1),
            SettingsAction::SweepTempFiles,
            SettingsAction::OpenLogin,
        ];
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "动作 {i} 与 {j} 相同");
                }
            }
        }
    }

    /// 合成输入跑一帧面板，返回当帧动作。
    ///
    /// 用来在**没有真机输入**的前提下复现/否定"控件自己触发"类缺陷：
    /// egui 的 `Response::clicked()` 语义上只有三条来路（egui 0.37 `context.rs`）：
    /// 真实 `PointerButton` press→release 对、AccessKit 的 Click 动作请求、
    /// 或控件已获键盘焦点时按下 Space/Enter（`FAKE_PRIMARY_CLICKED`）。
    /// 三条之外再出现动作，就是我们自己的代码重新引入了自触发。
    fn panel_frame(
        ctx: &eframe::egui::Context,
        view: &SettingsView,
        t: f64,
        events: Vec<eframe::egui::Event>,
    ) -> SettingsAction {
        let raw = eframe::egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                eframe::egui::Pos2::ZERO,
                vec2(520.0, 900.0),
            )),
            time: Some(t),
            events,
            ..eframe::egui::RawInput::default()
        };
        let mut action = SettingsAction::None;
        let out = ctx.run_ui(raw, |ui| {
            ui.set_width(480.0);
            action = settings_panel(ui, view);
        });
        out.drop_without_applying_deltas();
        action
    }

    fn pointer_moved(x: f32, y: f32) -> eframe::egui::Event {
        eframe::egui::Event::PointerMoved(eframe::egui::pos2(x, y))
    }

    fn pointer_button(x: f32, y: f32, pressed: bool) -> eframe::egui::Event {
        eframe::egui::Event::PointerButton {
            pos: eframe::egui::pos2(x, y),
            button: eframe::egui::PointerButton::Primary,
            pressed,
            modifiers: eframe::egui::Modifiers::default(),
        }
    }

    #[test]
    fn pointer_hover_and_stray_keys_never_produce_an_action() {
        // 真实事故（阶段 11）：设置区开着、指针停在语言行上，没有任何按下，
        // 界面语言却自己切了。这里把「指针游走/停放、无按下、无焦点按键」
        // 整类钉死在合成输入层：任何一帧都不得返回动作。
        let ctx = eframe::egui::Context::default();
        let view = view(Lang::En, None);
        let mut t = 0.0;
        for _ in 0..2 {
            assert_eq!(panel_frame(&ctx, &view, t, vec![]), SettingsAction::None);
            t += 0.05;
        }
        for y in (0..150).step_by(6) {
            for x in (84..484).step_by(20) {
                for _ in 0..2 {
                    let a = panel_frame(
                        &ctx,
                        &view,
                        t,
                        vec![pointer_moved(x as f32 + 0.5, y as f32 + 0.5)],
                    );
                    t += 0.05;
                    assert_eq!(
                        a,
                        SettingsAction::None,
                        "指针只移动到 ({x},{y}) 就产生了动作"
                    );
                }
            }
        }
        // 无焦点时 Space/Enter/Tab 也不得触发（FAKE_PRIMARY_CLICKED 路径的反面）
        for key in [
            eframe::egui::Key::Space,
            eframe::egui::Key::Enter,
            eframe::egui::Key::Tab,
        ] {
            let a = panel_frame(
                &ctx,
                &view,
                t,
                vec![eframe::egui::Event::Key {
                    key,
                    physical_key: Some(key),
                    pressed: true,
                    repeat: false,
                    modifiers: eframe::egui::Modifiers::default(),
                }],
            );
            t += 0.05;
            assert_eq!(a, SettingsAction::None, "{key:?} 在无焦点时触发了动作");
        }
    }

    /// 机器人点击扫描：逐点 press→release，返回非 None 的 (点击点, 动作)。
    fn robot_scan(view: &SettingsView) -> Vec<(eframe::egui::Pos2, SettingsAction)> {
        let ctx = eframe::egui::Context::default();
        let mut t = 0.0;
        let mut hits = Vec::new();
        for _ in 0..2 {
            panel_frame(&ctx, view, t, vec![]);
            t += 0.05;
        }
        for y in (0..PANEL_H as i32).step_by(3) {
            for x in (84..484).step_by(4) {
                let (x, y) = (x as f32 + 0.5, y as f32 + 0.5);
                panel_frame(&ctx, view, t, vec![pointer_moved(x, y)]);
                t += 0.05;
                panel_frame(&ctx, view, t, vec![pointer_button(x, y, true)]);
                t += 0.05;
                let action = panel_frame(&ctx, view, t, vec![pointer_button(x, y, false)]);
                t += 0.05;
                if action != SettingsAction::None {
                    hits.push((eframe::egui::pos2(x, y), action));
                }
            }
        }
        hits
    }

    /// 某类动作命中点的 y 跨度。
    fn y_span(
        hits: &[(eframe::egui::Pos2, SettingsAction)],
        pred: impl Fn(&SettingsAction) -> bool,
    ) -> (f32, f32) {
        let ys: Vec<f32> = hits
            .iter()
            .filter(|(_, a)| pred(a))
            .map(|(p, _)| p.y)
            .collect();
        (
            ys.iter().cloned().fold(f32::INFINITY, f32::min),
            ys.iter().cloned().fold(f32::NEG_INFINITY, f32::max),
        )
    }

    #[test]
    fn a_click_hits_exactly_its_own_row() {
        // 机器人点击两轮（当前语言 Zh / En 各一轮）：
        // ① 五类动作都要能被真实点出来（按钮真的可点）；
        // ② 点当前语言的按钮不出动作（防误触语义）；
        // ③ 各动作的命中 y 带按行序互不相交——id 撞车会让别行的点击串到本行，
        //    表现为"点 ACCOUNT 却切了语言"，在此直接暴露。
        for (lang, other) in [(Lang::Zh, Lang::En), (Lang::En, Lang::Zh)] {
            let v = view(lang, None);
            let hits = robot_scan(&v);
            let has = |a: SettingsAction| hits.iter().any(|(_, x)| *x == a);

            assert!(
                has(SettingsAction::OpenLogin),
                "lang={lang:?} 点不出 OpenLogin"
            );
            assert!(
                has(SettingsAction::SetLang(other)),
                "lang={lang:?} 点不出 SetLang({other:?})"
            );
            assert!(
                !has(SettingsAction::SetLang(lang)),
                "lang={lang:?} 点当前语言的按钮不应出动作"
            );
            for level in QualityCeiling::ALL {
                // 与语言同语义：点"当前档"不出动作（防误触），其余档必须点得出来
                if level == v.quality {
                    assert!(
                        !has(SettingsAction::SetQuality(level)),
                        "lang={lang:?} 点当前音质档不应出动作"
                    );
                } else {
                    assert!(
                        has(SettingsAction::SetQuality(level)),
                        "lang={lang:?} 点不出 SetQuality({level:?})"
                    );
                }
            }
            assert!(
                has(SettingsAction::SetDesktopLyric(!v.desktop_lyric)),
                "lang={lang:?} 点不出 SetDesktopLyric"
            );
            // 进度条样式按钮：默认细白线，点一次循环到下一档（彩色是可选项）
            assert!(
                has(SettingsAction::SetDesktopLyricProgress(
                    v.lyric_progress.next()
                )),
                "lang={lang:?} 点不出 SetDesktopLyricProgress"
            );
            // 媒体键\SMTC 与托盘两行同样是"必须有出口"的开关
            assert!(
                has(SettingsAction::SetMediaKeys(!v.media_keys)),
                "lang={lang:?} 点不出 SetMediaKeys"
            );
            assert!(
                has(SettingsAction::SetTray(!v.tray)),
                "lang={lang:?} 点不出 SetTray"
            );
            assert!(
                has(SettingsAction::SweepTempFiles),
                "lang={lang:?} 点不出 SweepTempFiles"
            );
            // 主题行与语言同语义：点当前档不动，其余档必须点得出来
            for i in 0..crate::gui::theme::THEMES.len() {
                if i == v.theme {
                    assert!(
                        !has(SettingsAction::SetTheme(i)),
                        "lang={lang:?} 点当前主题的按钮不应出动作"
                    );
                } else {
                    assert!(
                        has(SettingsAction::SetTheme(i)),
                        "lang={lang:?} 点不出 SetTheme({i})"
                    );
                }
            }

            let (open_lo, open_hi) = y_span(&hits, |a| *a == SettingsAction::OpenLogin);
            let (lang_lo, lang_hi) = y_span(&hits, |a| matches!(a, SettingsAction::SetLang(_)));
            let (theme_lo, theme_hi) = y_span(&hits, |a| matches!(a, SettingsAction::SetTheme(_)));
            let (qual_lo, qual_hi) = y_span(&hits, |a| matches!(a, SettingsAction::SetQuality(_)));
            let (lyr_lo, lyr_hi) = y_span(&hits, |a| {
                matches!(
                    a,
                    SettingsAction::SetDesktopLyric(_) | SettingsAction::SetDesktopLyricProgress(_)
                )
            });
            let (mk_lo, mk_hi) = y_span(&hits, |a| matches!(a, SettingsAction::SetMediaKeys(_)));
            let (tray_lo, tray_hi) = y_span(&hits, |a| matches!(a, SettingsAction::SetTray(_)));
            let (sweep_lo, sweep_hi) = y_span(&hits, |a| *a == SettingsAction::SweepTempFiles);
            // egui 命中测试有 ~5px 容差（按钮矩形外一点仍算命中），相邻行的 y 带
            // 会在缝隙处相接是正常的；但**整行级**串扰（点 ACCOUNT 串到语言行）
            // 会让某一带整体上移一行（≥16px），必须抓住。
            const HIT_SLOP: f32 = 6.0;
            assert!(
                open_hi < lang_lo + HIT_SLOP
                    && lang_hi < theme_lo + HIT_SLOP
                    && theme_hi < qual_lo + HIT_SLOP
                    && qual_hi < lyr_lo + HIT_SLOP
                    && lyr_hi < mk_lo + HIT_SLOP
                    && mk_hi < tray_lo + HIT_SLOP
                    && tray_hi < sweep_lo + HIT_SLOP,
                "lang={lang:?} 动作命中带串行了：account {open_lo}..{open_hi} / \
                 lang {lang_lo}..{lang_hi} / theme {theme_lo}..{theme_hi} / quality {qual_lo}..{qual_hi} / \
                 lyric {lyr_lo}..{lyr_hi} / media {mk_lo}..{mk_hi} / tray {tray_lo}..{tray_hi} / \
                 sweep {sweep_lo}..{sweep_hi}"
            );
            assert!(
                sweep_hi <= PANEL_H + HIT_SLOP,
                "lang={lang:?} 面板之外还点出了动作（y={sweep_hi}）"
            );
        }
    }

    #[test]
    fn a_press_that_starts_off_the_button_never_clicks_it() {
        // egui 在**按下那一刻**锁定命中控件（potential_click_id），松手只做归属：
        // 在标签列空白处按下、拖到按钮上松开，不得触发按钮。
        let ctx = eframe::egui::Context::default();
        let view = view(Lang::En, None);
        let mut t = 0.0;
        for _ in 0..2 {
            panel_frame(&ctx, &view, t, vec![]);
            t += 0.05;
        }
        // x < LABEL_W 是标签列，任何一行的按钮都不在那里
        for e in [
            pointer_moved(30.0, 27.0),
            pointer_button(30.0, 27.0, true),
            pointer_moved(200.0, 27.0),
        ] {
            panel_frame(&ctx, &view, t, vec![e]);
            t += 0.05;
        }
        let a = panel_frame(&ctx, &view, t, vec![pointer_button(200.0, 27.0, false)]);
        assert_eq!(
            a,
            SettingsAction::None,
            "按下点不在按钮上、松手点在按钮上，竟然触发了动作"
        );
    }

    #[test]
    fn every_row_lives_in_its_own_id_scope() {
        // 撞 id 的后果很隐蔽：一次点击同时触发两个控件，而在 settings_panel 里
        // 后写的动作还会覆盖先写的（切语言吞掉开登录）。行 id 由 `row` 的盐派生，
        // 这里直接量 egui 给出的 scope_id，不靠"后缀看起来不一样"。
        let ctx = eframe::egui::Context::default();
        let mut ids = Vec::new();
        let mut same_salt = Vec::new();
        let out = ctx.run_ui(eframe::egui::RawInput::default(), |ui| {
            for salt in [
                SALT_ACCOUNT,
                SALT_LANG,
                SALT_THEME,
                SALT_QUALITY,
                SALT_DESKTOP_LYRIC,
                SALT_MEDIA_KEYS,
                SALT_TRAY,
                SALT_TEMP_FILES,
            ] {
                row(ui, salt, "LABEL", |ui| ids.push(ui.scope_id()));
            }
            // 同盐两次必须得同 id——证明上面的断言真在防撞 id，不是走过场
            for _ in 0..2 {
                row(ui, "same-salt", "LABEL", |ui| same_salt.push(ui.scope_id()));
            }
        });
        out.drop_without_applying_deltas();

        assert_eq!(ids.len(), 8);
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                assert_ne!(a, b, "设置行 scope_id 撞了：{a:?}");
            }
        }
        assert_eq!(
            same_salt[0], same_salt[1],
            "同盐应得同 id，否则本测试失去鉴别力"
        );
    }
}
