//! 统一右键菜单（全项目**唯一**的右键菜单实现）。
//!
//! 为什么单独抽一个模块：右键菜单最容易各写各的——曲目行一套、歌单卡一套、
//! 磁带架一套，字号/描边/高亮/关闭时机全不一样，用户点哪儿都要重新学一遍。
//! 这里把「长什么样」和「怎么关」钉死一次，各处只负责提供条目列表。
//!
//! 实现要点（踩过的坑都写在这里）：
//!
//! 1. **必须画在 `Order::Foreground` 的 [`egui::Area`] 里**。菜单如果跟着行一起
//!    画在 `ScrollArea` 内，会被裁剪区切掉一半，还会跟着列表一起滚走。
//! 2. **打开那一帧不响应点击**。右键的 press 与 release 可能落在同一帧（触摸板/
//!    高回报率鼠标），若不做 [`ContextMenuState::opened_frame`] 判断，菜单会
//!    「开了立刻关」，表现为右键没反应。
//! 3. **关闭只有三条路**：选中某一项、按 Esc、点在菜单矩形之外。不额外监听
//!    「焦点丢失」——egui 的 Area 没有真正的窗口焦点，那条路在 immediate-mode
//!    里不可靠，容易出现「点按钮时菜单还挂着」。
//! 4. 菜单是**纯数据 + 回传动作**：本模块不认识 `Track`、不认识歌单，返回
//!    [`MenuAction`] 由 [`crate::gui::app::ReimuApp`] 解释。这样菜单样式改动
//!    不会牵连业务逻辑。

use eframe::egui::{Align2, Id, Pos2, Rect, Sense, Stroke, Ui, Vec2};

use super::theme::*;
use super::widgets;

/// 菜单宽度（px）。
pub const MENU_W: f32 = 158.0;
/// 菜单项行高（px）。
const ITEM_H: f32 = 22.0;
/// 分组标题行高（px）。
const HEAD_H: f32 = 18.0;
/// 分隔线占位高度（px）。
const SEP_H: f32 = 7.0;
/// 菜单左右内距（px）。
const PAD_X: f32 = 10.0;

/// 菜单项被点击后回传的动作。
///
/// 只放**通用**动作：具体是「哪首歌」「哪个歌单」由调用方在打开菜单时就绑定好
/// （闭包捕获），菜单本身不持有业务数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuAction {
    /// 立即播放。
    Play,
    /// 下一首播放（插入到当前曲目之后）。
    PlayNext,
    /// 加入播放队列末尾。
    AddToQueue,
    /// 加入某个自建歌单（`u64` = 歌单 id）。
    AddToPlaylist(u64),
    /// 从当前自建歌单里移除这一首。
    RemoveFromPlaylist,
    /// 打开某个自建歌单（`u64` = 歌单 id）。
    OpenLocalPlaylist(u64),
    /// 删除某个自建歌单（`u64` = 歌单 id）。
    DeleteLocalPlaylist(u64),
    /// 打开某张已载入歌单（`String` = 歌单 id）。
    OpenLoadedPlaylist(String),
    /// 把某张已载入歌单移出历史列表。
    RemoveLoadedPlaylist(String),
    /// 打开某张账号歌单。
    OpenAccountPlaylist(String),
    /// 从账号歌单列表隐藏（仅本地，不动云端）。
    HideAccountPlaylist(String),
    /// 跳到专辑页。
    GoToAlbum,
    /// 跳到歌手页。
    GoToArtist,
    /// 收藏 / 取消收藏。
    ToggleFavorite,
}

/// 菜单里的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    /// 分组标题（不可点，如「加入歌单」）。
    Heading(String),
    /// 可点条目。
    Item {
        /// 点击后回传的动作。
        action: MenuAction,
        /// 显示文案。
        label: String,
        /// 是否可点（`false` 时灰显，点了不产生动作）。
        enabled: bool,
        /// 危险动作（删除类）用错误色显示。
        danger: bool,
    },
    /// 分隔线。
    Separator,
}

impl MenuEntry {
    /// 普通可点条目。
    pub fn item(action: MenuAction, label: impl Into<String>) -> Self {
        Self::Item {
            action,
            label: label.into(),
            enabled: true,
            danger: false,
        }
    }

    /// 灰显条目（条件不满足时也要露出来，让人知道"有这功能，只是现在不能用"）。
    pub fn disabled(action: MenuAction, label: impl Into<String>) -> Self {
        Self::Item {
            action,
            label: label.into(),
            enabled: false,
            danger: false,
        }
    }

    /// 危险条目（删除类）。
    pub fn danger(action: MenuAction, label: impl Into<String>) -> Self {
        Self::Item {
            action,
            label: label.into(),
            enabled: true,
            danger: true,
        }
    }

    /// 分组标题。
    pub fn heading(label: impl Into<String>) -> Self {
        Self::Heading(label.into())
    }

    /// 分隔线。
    pub fn separator() -> Self {
        Self::Separator
    }
}

/// 菜单状态（打开时创建，关闭时置 `None`）。
#[derive(Debug, Clone)]
pub struct ContextMenuState {
    /// 弹出位置（屏幕坐标 = 右键点）。
    pos: Pos2,
    /// 条目列表。
    entries: Vec<MenuEntry>,
    /// 打开的帧号（该帧内不响应点击，见模块注释第 2 条）。
    opened_frame: u64,
}

impl ContextMenuState {
    /// 在 `pos` 处打开一个菜单。
    ///
    /// `frame` 传 [`eframe::egui::Context::cumulative_frame_nr`]，用于识别"打开这一帧"。
    /// `entries` 为空时返回 `None`——空菜单没有意义，还会让右键看起来"没反应"。
    pub fn open(pos: Pos2, entries: Vec<MenuEntry>, frame: u64) -> Option<Self> {
        if entries.is_empty() {
            return None;
        }
        Some(Self {
            pos,
            entries,
            opened_frame: frame,
        })
    }

    /// 弹出位置（测试用）。
    pub fn pos(&self) -> Pos2 {
        self.pos
    }

    /// 条目数量（测试用）。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否没有任何条目（`open` 保证不会出现，保留给断言用）。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 渲染并处理当前菜单；返回被选中的动作。
///
/// 返回 `Some(action)` 的同一帧菜单即关闭（`state` 被置 `None`），调用方直接
/// 执行动作即可，不需要自己清理。
pub fn show(
    ctx: &eframe::egui::Context,
    state: &mut Option<ContextMenuState>,
) -> Option<MenuAction> {
    let menu = state.as_ref()?;
    let opening = menu.opened_frame == ctx.cumulative_frame_nr();
    let mut chosen: Option<MenuAction> = None;

    let area = eframe::egui::Area::new(Id::unique("reimu-player-context-menu"))
        .order(eframe::egui::Order::Foreground)
        .fixed_pos(menu.pos)
        .constrain_to(ctx.content_rect())
        .show(ctx, |ui| {
            eframe::egui::Frame::NONE
                .fill(pal().panel)
                .stroke(Stroke::new(STROKE_STRUCTURAL, pal().line))
                .inner_margin(eframe::egui::Margin::symmetric(0, 4))
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing = Vec2::ZERO;
                    ui.set_min_width(MENU_W);
                    for entry in &menu.entries {
                        match entry {
                            MenuEntry::Heading(text) => {
                                let (rect, _) = ui
                                    .allocate_exact_size(Vec2::new(MENU_W, HEAD_H), Sense::hover());
                                widgets::paint_spec_text(
                                    ui,
                                    pos2_left(rect, 4.0),
                                    Align2::LEFT_CENTER,
                                    text,
                                    FONT_TINY,
                                    TRACKING_LABEL,
                                    pal().mid,
                                    true,
                                    false,
                                );
                            }
                            MenuEntry::Separator => {
                                let (rect, _) = ui
                                    .allocate_exact_size(Vec2::new(MENU_W, SEP_H), Sense::hover());
                                let y = rect.center().y;
                                ui.painter().line_segment(
                                    [
                                        eframe::egui::pos2(rect.min.x + PAD_X * 0.5, y),
                                        eframe::egui::pos2(rect.max.x - PAD_X * 0.5, y),
                                    ],
                                    Stroke::new(
                                        STROKE_HAIRLINE,
                                        widgets::with_opacity(pal().ink, 0.28),
                                    ),
                                );
                            }
                            MenuEntry::Item {
                                action,
                                label,
                                enabled,
                                danger,
                            } => {
                                if item_row(ui, label, *enabled, *danger) {
                                    chosen = Some(action.clone());
                                }
                            }
                        }
                    }
                });
        });

    let rect = area.response.rect;
    if chosen.is_some() {
        *state = None;
        return chosen;
    }
    let esc = ctx.input(|i| i.key_pressed(eframe::egui::Key::Escape));
    let clicked_outside = !opening
        && ctx.input(|i| {
            i.pointer.any_click()
                && i.pointer
                    .interact_pos()
                    .map(|p| !rect.contains(p))
                    .unwrap_or(false)
        });
    if esc || clicked_outside {
        *state = None;
    }
    None
}

/// 行内左对齐基准点。
fn pos2_left(rect: Rect, dx: f32) -> Pos2 {
    eframe::egui::pos2(rect.min.x + dx, rect.center().y)
}

/// 画一行可点菜单条目，返回「本帧被点了」。
///
/// 单独抽出来是因为文本输入框的「粘贴」菜单
/// （[`super::clipboard::attach_paste_menu`]）要用**同一份**绘制代码：
/// 两处外观一旦分叉，用户会看到两种菜单。
///
/// `enabled = false` 时既不高亮也不可点（灰显）；`danger` 只影响常态文字色。
pub(crate) fn item_row(ui: &mut Ui, label: &str, enabled: bool, danger: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(MENU_W, ITEM_H), Sense::click());
    let hot = enabled && resp.hovered();
    if hot {
        ui.painter().rect_filled(rect, radius_none(), pal().blue);
    }
    let color = if !enabled {
        widgets::with_opacity(pal().mid, 0.55)
    } else if hot {
        fg_on(pal().blue)
    } else if danger {
        pal().error_mark
    } else {
        pal().ink
    };
    widgets::paint_spec_text(
        ui,
        pos2_left(rect, PAD_X),
        Align2::LEFT_CENTER,
        label,
        FONT_TINY,
        TRACKING_TINY,
        color,
        false,
        true,
    );
    hot && resp.clicked()
}

/// 给「加入歌单」这一组生成条目（多处共用，避免文案与顺序漂移）。
///
/// `targets` 是 `(歌单 id, 歌单名)`；空列表时给一条灰显提示而不是什么都不画——
/// 用户点右键发现"没有加入歌单"会以为功能坏了，实际是还没建歌单。
pub fn playlist_entries(targets: &[(u64, String)], lang: crate::i18n::Lang) -> Vec<MenuEntry> {
    use crate::i18n::pick;
    let mut out = vec![MenuEntry::separator()];
    out.push(MenuEntry::heading(pick(
        lang,
        "加入自建歌单",
        "ADD TO PLAYLIST",
    )));
    if targets.is_empty() {
        out.push(MenuEntry::disabled(
            MenuAction::Play,
            pick(
                lang,
                "（先到「歌单」页新建一个）",
                "(create one on the PLAYLIST page)",
            ),
        ));
        return out;
    }
    for (id, name) in targets {
        // 名字过长会撑爆菜单：按字截断（中英都按字符数）
        let label = if name.chars().count() > 14 {
            format!("{}…", name.chars().take(14).collect::<String>())
        } else {
            name.clone()
        };
        out.push(MenuEntry::item(MenuAction::AddToPlaylist(*id), label));
    }
    out
}

/// 当前指针位置（供调用方在 `secondary_clicked` 时取弹出点）。
pub fn pointer_pos(ui: &Ui) -> Option<Pos2> {
    ui.ctx().input(|i| i.pointer.interact_pos())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_entry_list_refuses_to_open() {
        // 空菜单会让右键"看起来没反应"，不如明确不开
        assert!(ContextMenuState::open(Pos2::ZERO, Vec::new(), 0).is_none());
    }

    #[test]
    fn open_keeps_entries_and_position() {
        let entries = vec![
            MenuEntry::item(MenuAction::Play, "PLAY"),
            MenuEntry::separator(),
        ];
        let menu = ContextMenuState::open(Pos2::new(12.0, 34.0), entries, 7).expect("应当能打开");
        assert_eq!(menu.len(), 2);
        assert!(!menu.is_empty());
        assert_eq!(menu.pos(), Pos2::new(12.0, 34.0));
        assert_eq!(menu.opened_frame, 7);
    }

    #[test]
    fn playlist_entries_never_produce_a_dead_menu() {
        // 没有自建歌单时也必须有条目：一条灰显提示 + 标题 + 分隔线
        let entries = playlist_entries(&[], crate::i18n::Lang::Zh);
        assert!(!entries.is_empty());
        assert!(
            entries
                .iter()
                .any(|e| matches!(e, MenuEntry::Item { enabled: false, .. })),
            "空歌单列表要给灰显提示"
        );
        let with = playlist_entries(&[(1, "开车".to_string())], crate::i18n::Lang::En);
        assert!(with.iter().any(|e| matches!(
            e,
            MenuEntry::Item {
                action: MenuAction::AddToPlaylist(1),
                enabled: true,
                ..
            }
        )));
    }

    #[test]
    fn long_playlist_names_are_truncated() {
        let long = "这是一个非常非常非常长的歌单名字啊".to_string();
        let entries = playlist_entries(&[(9, long)], crate::i18n::Lang::Zh);
        let label = entries
            .iter()
            .find_map(|e| match e {
                MenuEntry::Item {
                    action: MenuAction::AddToPlaylist(9),
                    label,
                    ..
                } => Some(label.clone()),
                _ => None,
            })
            .expect("应当有加入歌单条目");
        assert!(label.ends_with('…'), "超长名字要截断: {label}");
        assert!(label.chars().count() <= 15);
    }

    /// 跑一帧菜单，返回 (是否还开着, 动作)。
    fn frame(
        ctx: &eframe::egui::Context,
        state: &mut Option<ContextMenuState>,
        t: f64,
        events: Vec<eframe::egui::Event>,
    ) -> Option<MenuAction> {
        let raw = eframe::egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(600.0, 400.0))),
            time: Some(t),
            events,
            ..eframe::egui::RawInput::default()
        };
        let mut action = None;
        let out = ctx.run_ui(raw, |ui| {
            // 菜单之外先铺一块背景，模拟"点空白处"
            ui.allocate_exact_size(Vec2::new(600.0, 400.0), Sense::hover());
            action = show(ctx, state);
        });
        out.drop_without_applying_deltas();
        action
    }

    fn entries() -> Vec<MenuEntry> {
        vec![
            MenuEntry::item(MenuAction::Play, "PLAY"),
            MenuEntry::item(MenuAction::GoToAlbum, "ALBUM"),
        ]
    }

    #[test]
    fn opening_frame_never_closes_itself() {
        // 关键回归：右键 press/release 同帧时，菜单不得"开了立刻关"
        let ctx = eframe::egui::Context::default();
        let mut state = ContextMenuState::open(Pos2::new(40.0, 40.0), entries(), 0);
        // 第 0 帧打开（frame_nr 从 0 起）
        let a = frame(&ctx, &mut state, 0.0, vec![]);
        assert!(a.is_none());
        assert!(state.is_some(), "打开那一帧就自己关掉了");
    }

    #[test]
    fn escape_closes_the_menu() {
        let ctx = eframe::egui::Context::default();
        let mut state = ContextMenuState::open(Pos2::new(40.0, 40.0), entries(), 0);
        frame(&ctx, &mut state, 0.0, vec![]);
        frame(
            &ctx,
            &mut state,
            0.05,
            vec![eframe::egui::Event::Key {
                key: eframe::egui::Key::Escape,
                physical_key: Some(eframe::egui::Key::Escape),
                pressed: true,
                repeat: false,
                modifiers: eframe::egui::Modifiers::default(),
            }],
        );
        assert!(state.is_none(), "Esc 应当关掉菜单");
    }

    #[test]
    fn clicking_far_away_closes_the_menu() {
        let ctx = eframe::egui::Context::default();
        let mut state = ContextMenuState::open(Pos2::new(40.0, 40.0), entries(), 0);
        frame(&ctx, &mut state, 0.0, vec![]);
        // 菜单在左上角 158×~60 的范围里，点 (500,300) 明显在外面。
        // 注意 `any_click()` 只在**松手**那一帧为真（egui 的 click 语义），
        // 所以必须发 press→release 一对，只发 press 永远关不掉。
        for e in [
            eframe::egui::Event::PointerMoved(Pos2::new(500.0, 300.0)),
            eframe::egui::Event::PointerButton {
                pos: Pos2::new(500.0, 300.0),
                button: eframe::egui::PointerButton::Primary,
                pressed: true,
                modifiers: eframe::egui::Modifiers::default(),
            },
            eframe::egui::Event::PointerButton {
                pos: Pos2::new(500.0, 300.0),
                button: eframe::egui::PointerButton::Primary,
                pressed: false,
                modifiers: eframe::egui::Modifiers::default(),
            },
        ] {
            frame(&ctx, &mut state, 0.05, vec![e]);
        }
        assert!(state.is_none(), "点菜单外面应当关闭");
    }

    #[test]
    fn clicking_an_item_returns_its_action_and_closes() {
        let ctx = eframe::egui::Context::default();
        let mut state = ContextMenuState::open(Pos2::new(40.0, 40.0), entries(), 0);
        frame(&ctx, &mut state, 0.0, vec![]);
        // 第一项：y = 40(顶) + 4(内距) + ITEM_H/2
        let y = 40.0 + 4.0 + ITEM_H * 0.5;
        let x = 40.0 + PAD_X;
        let mut got = None;
        for e in [
            eframe::egui::Event::PointerMoved(Pos2::new(x, y)),
            eframe::egui::Event::PointerButton {
                pos: Pos2::new(x, y),
                button: eframe::egui::PointerButton::Primary,
                pressed: true,
                modifiers: eframe::egui::Modifiers::default(),
            },
            eframe::egui::Event::PointerButton {
                pos: Pos2::new(x, y),
                button: eframe::egui::PointerButton::Primary,
                pressed: false,
                modifiers: eframe::egui::Modifiers::default(),
            },
        ] {
            let a = frame(&ctx, &mut state, 0.05, vec![e]);
            if a.is_some() {
                got = a;
            }
        }
        assert_eq!(got, Some(MenuAction::Play), "点第一项应当回传 Play");
        assert!(state.is_none(), "选中后菜单应当关闭");
    }
}
