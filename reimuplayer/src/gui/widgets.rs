//! 设计2.md §3 组件层：spine 列表项 / 面板 / 控制按钮 / 卡带 / Toast / 错误模态。
//!
//! 绘制原则（§9 Rust 映射）：
//! - 硬投影全部经 [`theme::hard_shadow`] → `Shadow::as_shape` 手绘，**blur 恒 0**；
//! - 字距经 [`spec_job`] 逐字插 `leading_space`（egui 无 letter-spacing 属性）；
//! - 800/900 字重经 [`paint_job`] 的双描（faux-bold）近似——egui 默认字库无粗体面；
//! - 全部颜色引用 [`theme`] 令牌，本文件不出现十六进制（§10）。
//!
//! 纯逻辑辅助（[`load_dots`] / [`badge_view`] / [`tracking_px`] / [`hash_id`]）
//! 与绘制解耦，可完整单测。

use eframe::egui::text::{LayoutJob, TextFormat};
use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, FontFamily, FontId, Margin, Pos2, Rect, Response,
    Sense, Stroke, StrokeKind, TextureId, Ui, UiBuilder, Vec2,
};

use super::theme::*;
use crate::i18n::{pick, Lang};
use crate::music::load_state::LoadState;

// ---------------------------------------------------------------------------
// 排版（§1.2 / §9：LayoutJob 逐字插间距）
// ---------------------------------------------------------------------------

/// em 字距 → 像素（§1.2 表为 em 值）。
pub fn tracking_px(tracking_em: f32, size: f32) -> f32 {
    tracking_em * size
}

/// 字重档（映射到**真实字族**，不再是"双描近似"的代名词）。
///
/// 为什么要这个枚举：旧实现里 `spec_job` 恒用 `Proportional`（400），
/// 所有"加粗"都靠 [`paint_job`] 的 0.45px 双描。拉丁 400 双描 ≈ 半粗，
/// 中文 400 双描则糊成一片——这就是中英观感分裂的根因。现在按档位切到
/// 真实字面（见 [`super::theme::setup_fonts`]），中文有 700 面可用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weight {
    /// 常规（Inter 400 → Noto SC 400）。
    Regular,
    /// 粗体（Inter 700 → Noto SC 700）。
    Bold,
    /// 标题级（Inter 900 → Noto SC 700 + 双描补到 ≈900）。
    Black,
}

impl Weight {
    /// 对应的 egui 字族。
    pub fn family(self) -> FontFamily {
        match self {
            Weight::Regular => FontFamily::Proportional,
            Weight::Bold => super::theme::font_family_bold(),
            Weight::Black => super::theme::font_family_black(),
        }
    }

    /// 是否仍需要叠双描（只有 Black 档：中文没有 900 面）。
    pub fn needs_faux(self) -> bool {
        self == Weight::Black
    }
}

/// 构造带字距的单行 [`LayoutJob`]（§9：egui 无 letter-spacing，逐字插间距）。
///
/// `mono = true` 走等宽字族（数字等宽，§1.2「数字 tabular」）。
/// 图标字形（PUA 码点，[`super::icons::is_icon_text`]）自动切图标字族且无字距。
pub fn spec_job(text: &str, size: f32, tracking_em: f32, color: Color32, mono: bool) -> LayoutJob {
    spec_job_weight(text, size, tracking_em, color, mono, Weight::Regular)
}

/// 同 [`spec_job`]，但指定字重档（真实字面）。
pub fn spec_job_weight(
    text: &str,
    size: f32,
    tracking_em: f32,
    color: Color32,
    mono: bool,
    weight: Weight,
) -> LayoutJob {
    let is_icon = crate::gui::icons::is_icon_text(text);
    let family = if is_icon {
        crate::gui::icons::font_family()
    } else if mono {
        FontFamily::Monospace
    } else {
        weight.family()
    };
    let format = TextFormat {
        font_id: FontId::new(size, family),
        color,
        ..Default::default()
    };
    let spacing = if is_icon {
        0.0
    } else {
        tracking_px(tracking_em, size)
    };
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    job.wrap.max_rows = 1;
    for (i, ch) in text.chars().enumerate() {
        let leading = if i == 0 { 0.0 } else { spacing };
        job.append(&ch.to_string(), leading, format.clone());
    }
    job
}

/// 自动换行的多行 spec 文本 job（模态正文等定宽区域）。
///
/// [`spec_job`] 刻意固定 `max_rows = 1`、宽度无限——那是给列表行用的。
/// 对话框正文是自然语言，必须按可用宽度折行，否则会像早期确认框那样直接
/// 冲出纸面边界。中文没有词间空格，必须 `break_anywhere` 才能正确断行。
fn wrapped_spec_job(
    text: &str,
    size: f32,
    tracking_em: f32,
    color: Color32,
    mono: bool,
    max_width: f32,
) -> LayoutJob {
    let mut job = spec_job_weight(text, size, tracking_em, color, mono, Weight::Regular);
    job.wrap.max_width = max_width.max(1.0);
    job.wrap.max_rows = usize::MAX;
    job.wrap.break_anywhere = true;
    job
}

/// 截断模式：超宽时单行省略号截断（spine 名/副行）。
pub fn truncate_to(mut job: LayoutJob, max_width: f32) -> LayoutJob {
    job.wrap.max_width = max_width;
    job.wrap.overflow_character = Some('…');
    job.wrap.max_rows = 1;
    job
}

/// 高亮匹配：把 `query` 命中的字符段加上底色。
///
/// 匹配策略（与搜索框的直觉一致）：
/// 1. 先按**子串**匹配（不区分大小写）——"起风"命中"起风了"；
/// 2. 子串不中再按**顺序子序列**匹配——"cp" 命中 "City Pop"（各字符依次出现）；
/// 3. 都不中则不高亮（返回等价于 [`spec_job`] 的 job）。
///
/// 逐字符 append（与 [`spec_job`] 相同的字距处理），只在命中字符上换
/// `background`，字形与字距完全不变——否则高亮会让整行左右跳动。
pub fn highlight_job(
    text: &str,
    query: &str,
    size: f32,
    tracking_em: f32,
    color: Color32,
    highlight_bg: Color32,
    mono: bool,
) -> LayoutJob {
    let family = if mono {
        FontFamily::Monospace
    } else {
        FontFamily::Proportional
    };
    let base = TextFormat {
        font_id: FontId::new(size, family.clone()),
        color,
        ..Default::default()
    };
    let hot = TextFormat {
        font_id: FontId::new(size, family),
        color,
        background: highlight_bg,
        ..Default::default()
    };
    let matched = matched_chars(text, query);
    let spacing = tracking_px(tracking_em, size);
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    job.wrap.max_rows = 1;
    for (i, ch) in text.chars().enumerate() {
        let leading = if i == 0 { 0.0 } else { spacing };
        let format = if matched.get(i).copied().unwrap_or(false) {
            hot.clone()
        } else {
            base.clone()
        };
        job.append(&ch.to_string(), leading, format);
    }
    job
}

/// 每个字符是否被查询命中（长度与 `text` 的字符数一致）。
fn matched_chars(text: &str, query: &str) -> Vec<bool> {
    let hay: Vec<char> = text
        .chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect();
    let needle: Vec<char> = query
        .trim()
        .chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect();
    let mut matched = vec![false; hay.len()];
    if needle.is_empty() || hay.is_empty() {
        return matched;
    }
    // 1) 子串
    if needle.len() <= hay.len() {
        if let Some(start) = hay
            .windows(needle.len())
            .position(|window| window == needle.as_slice())
        {
            for slot in matched.iter_mut().skip(start).take(needle.len()) {
                *slot = true;
            }
            return matched;
        }
    }
    // 2) 顺序子序列
    let mut cursor = 0usize;
    let mut hits = Vec::new();
    for ch in &needle {
        match hay[cursor..].iter().position(|c| c == ch) {
            Some(offset) => {
                let index = cursor + offset;
                hits.push(index);
                cursor = index + 1;
            }
            None => return vec![false; hay.len()],
        }
    }
    for index in hits {
        matched[index] = true;
    }
    matched
}

/// 换行模式：正文段落（mini 面板等）。
pub fn wrap_to(mut job: LayoutJob, max_width: f32, max_rows: usize) -> LayoutJob {
    job.wrap.max_width = max_width;
    job.wrap.max_rows = max_rows;
    job
}

/// 双描偏移（px）：随字号缩放，`0.05em` 夹在 0.35–1.15px。
///
/// 固定 0.45px 在 9px 标签上等于 +5% 笔画（中文直接糊），在 24px 标题上
/// 又只有 +1.9%（看不出加粗）。`paint_job` 与 `paint_marquee` 必须共用它，
/// 否则同一段字在静止/滚动两种状态下笔画粗细会跳。
pub fn faux_offset(size: f32) -> f32 {
    (size * 0.05).clamp(0.35, 1.15)
}

/// 把 `job` 的全部分节改写成字影色（[`theme::pal().text_shadow`]），供双描底层用。
///
/// 为什么不再"继承原色"：底层继承黑色时，重影是一道**纯黑**的平行笔画，
/// 24px 标题下看起来像糊了一笔；改中性灰后只加深笔画边缘。
pub fn shadowed_job(mut job: LayoutJob) -> LayoutJob {
    for section in &mut job.sections {
        section.format.color = pal().text_shadow;
    }
    job
}

/// 把 [`LayoutJob`] 画到 `pos`（按 `anchor` 对齐），可选叠一层双描。
///
/// 双描 = 先画一层灰色字影（偏移右下 `faux_offset`），再画主字。
/// 现在只有 [`Weight::Black`] 档会用到它，作为「中文没有 900 面」的补偿。
///
/// 返回所占矩形（不含字距外延），方便调用方排版后续元素。
pub fn paint_job(ui: &Ui, pos: Pos2, anchor: Align2, job: LayoutJob, faux_bold: bool) -> Rect {
    let size = job
        .sections
        .first()
        .map(|s| s.format.font_id.size)
        .unwrap_or(12.0);
    // 字影必须**另排一次** galley：`Galley` 的分节颜色在 layout 时就固化了，
    // 拿到 Arc<Galley> 之后再改色是改不动的。
    let shadow = if faux_bold {
        Some(shadowed_job(job.clone()))
    } else {
        None
    };
    let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
    let rect = anchor.align_size_within_rect(galley.size(), Rect::from_min_size(pos, Vec2::ZERO));
    if let Some(shadow_job) = shadow {
        let off = faux_offset(size);
        let shadow_galley = ui.ctx().fonts_mut(|f| f.layout_job(shadow_job));
        ui.painter().galley(
            rect.min + vec2(off, off),
            shadow_galley,
            Color32::TRANSPARENT,
        );
    }
    ui.painter().galley(rect.min, galley, Color32::TRANSPARENT);
    rect
}

/// 便捷：按 §1.2 的样式段画一段字（`bold = true` → [`Weight::Bold`] 真实粗面）。
#[allow(clippy::too_many_arguments)]
pub fn paint_spec_text(
    ui: &Ui,
    pos: Pos2,
    anchor: Align2,
    text: &str,
    size: f32,
    tracking_em: f32,
    color: Color32,
    mono: bool,
    bold: bool,
) -> Rect {
    let weight = if bold { Weight::Bold } else { Weight::Regular };
    paint_spec_text_weight(
        ui,
        pos,
        anchor,
        text,
        size,
        tracking_em,
        color,
        mono,
        weight,
    )
}

/// 同 [`paint_spec_text`]，但显式指定字重档（标题级用 [`Weight::Black`]）。
#[allow(clippy::too_many_arguments)]
pub fn paint_spec_text_weight(
    ui: &Ui,
    pos: Pos2,
    anchor: Align2,
    text: &str,
    size: f32,
    tracking_em: f32,
    color: Color32,
    mono: bool,
    weight: Weight,
) -> Rect {
    let job = spec_job_weight(text, size, tracking_em, color, mono, weight);
    paint_job(ui, pos, anchor, job, weight.needs_faux())
}

/// 把 `text` 按**实际排版宽度**截断到 `max_w` 内，超出时末尾补 `…`。
///
/// 为什么不用 egui 内建的省略号：spec 文本是自绘 galley（[`spec_job`]），
/// 没有 `Label::truncate` 那条路；不自己裁就会直接溢出面板边框。
#[allow(clippy::too_many_arguments)]
pub fn fit_text(
    ui: &Ui,
    text: &str,
    size: f32,
    tracking_em: f32,
    color: Color32,
    mono: bool,
    bold: bool,
    max_w: f32,
) -> String {
    let weight = if bold { Weight::Bold } else { Weight::Regular };
    let measure = |s: &str| -> f32 {
        let job = spec_job_weight(s, size, tracking_em, color, mono, weight);
        ui.ctx().fonts_mut(|f| f.layout_job(job)).size().x
    };
    if max_w <= 0.0 {
        return String::new();
    }
    if measure(text) <= max_w {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    // 二分最长可容纳前缀（前缀 + 省略号一起量，避免刚好超一个省略号的宽度）
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let mut probe: String = chars[..mid].iter().collect();
        probe.push('…');
        if measure(&probe) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let mut out: String = chars[..lo].iter().collect();
    out.push('…');
    out
}

/// 走马灯滚动速度（px/秒）。26px/s ≈ 3 秒走完一个「晴天」标题的溢出量。
pub const MARQUEE_SPEED: f32 = 26.0;

/// 走马灯在两端各停留的时长（秒），让人有时间读完首字/末字。
pub const MARQUEE_HOLD: f32 = 1.2;

/// 走马灯的水平位移（px，恒 `0..=overflow`）。
///
/// 采用**往返式**而非循环式：标题回卷到开头时不会有"跳字"的割裂感，
/// 这也和磁带机「正转—反转」的隐喻一致。
///
/// 纯函数，时间轴完全由 `elapsed` 决定（不读时钟），因此可精确单测。
pub fn marquee_offset(text_w: f32, view_w: f32, elapsed: f32) -> f32 {
    let overflow = text_w - view_w;
    if !overflow.is_finite() || overflow <= 0.5 {
        return 0.0;
    }
    let travel = overflow / MARQUEE_SPEED;
    let cycle = 2.0 * (MARQUEE_HOLD + travel);
    let mut t = if elapsed.is_finite() && elapsed > 0.0 {
        elapsed % cycle
    } else {
        0.0
    };
    // 去程：停留 → 左移
    if t < MARQUEE_HOLD {
        return 0.0;
    }
    t -= MARQUEE_HOLD;
    if t < travel {
        return (t * MARQUEE_SPEED).min(overflow);
    }
    // 回程：末端停留 → 右移回原点
    t -= travel;
    if t < MARQUEE_HOLD {
        return overflow;
    }
    t -= MARQUEE_HOLD;
    (overflow - t * MARQUEE_SPEED).clamp(0.0, overflow)
}

/// 单行标题：**超宽时自动走马灯**，且永不越过 `area` 右边界（裁剪）。
///
/// 返回 `true` 表示当前处于滚动状态，调用方据此请求下一帧重绘
/// （静止时不必每帧重绘）。
///
/// 为什么必须裁剪而不是截断：用户要求「显示范围不应超过 LOCAL 标签」，
/// 滚动中的文字必须被硬裁在徽标左侧，否则会压到徽标上。
pub fn paint_marquee(ui: &Ui, area: Rect, job: LayoutJob, faux_bold: bool, elapsed: f32) -> bool {
    let size = job
        .sections
        .first()
        .map(|s| s.format.font_id.size)
        .unwrap_or(12.0);
    let shadow = if faux_bold {
        Some(shadowed_job(job.clone()))
    } else {
        None
    };
    let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
    let text_w = galley.size().x;
    let scrolling = text_w > area.width() + 0.5;
    let offset = marquee_offset(text_w, area.width(), elapsed);
    let painter = ui.painter().with_clip_rect(area);
    let base = area.left_top() - vec2(offset, 0.0);
    if let Some(shadow_job) = shadow {
        let off = faux_offset(size);
        let shadow_galley = ui.ctx().fonts_mut(|f| f.layout_job(shadow_job));
        painter.galley(base + vec2(off, off), shadow_galley, Color32::TRANSPARENT);
    }
    painter.galley(base, galley, Color32::TRANSPARENT);
    scrolling
}

/// 面板内「标签 + 值」信息行（TRACK / ARTIST / ALBUM / QUALITY）。
///
/// 标签固定占 [`INFO_LABEL_W`]，值在剩余宽度内按实际排版宽度截断并补 `…`。
pub fn info_row(ui: &mut Ui, label: &str, value: &str, height: f32, value_bold: bool) {
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    paint_spec_text(
        ui,
        row.left_center(),
        Align2::LEFT_CENTER,
        label,
        FONT_TINY,
        TRACKING_TINY,
        pal().mid,
        true,
        false,
    );
    let x = row.min.x + INFO_LABEL_W;
    let avail = (row.max.x - x).max(0.0);
    let text = fit_text(
        ui,
        value,
        FONT_SPINE_NAME,
        0.0,
        pal().ink,
        false,
        value_bold,
        avail,
    );
    paint_spec_text(
        ui,
        pos2(x, row.center().y),
        Align2::LEFT_CENTER,
        &text,
        FONT_SPINE_NAME,
        0.0,
        pal().ink,
        false,
        value_bold,
    );
}

/// 同 [`panel_head`]，但右侧标签可点（返回 `true` = 被点了）。
///
/// 用于「专辑曲子 / 歌手曲子」页头的「← 返回」：返回入口必须**看得见**，
/// 藏在 Esc 或右键菜单里等于没有。悬停变蓝 + 下方一条细线，明示可点。
pub fn panel_head_link(ui: &mut Ui, label: &str, right: &str) -> bool {
    let height = FONT_LABEL + 4.0;
    let rect = ui
        .allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover())
        .0;
    paint_spec_text(
        ui,
        rect.left_center() + vec2(0.0, 0.5),
        Align2::LEFT_CENTER,
        &label.to_uppercase(),
        FONT_LABEL,
        TRACKING_LABEL,
        pal().ink,
        false,
        true,
    );
    // 右侧标签单独分配一小块可点区域：整行都可点会误伤标题区
    let link_w = text_width(ui, right, FONT_TINY, TRACKING_TINY) + 8.0;
    let link_rect = Rect::from_min_max(
        pos2((rect.max.x - link_w).max(rect.min.x), rect.min.y),
        rect.max,
    );
    let resp = ui.interact(
        link_rect,
        ui.scope_id().with("panel-head-link"),
        Sense::click(),
    );
    let color = if resp.hovered() {
        pal().blue
    } else {
        pal().mid
    };
    paint_spec_text(
        ui,
        rect.right_center() + vec2(0.0, 0.5),
        Align2::RIGHT_CENTER,
        right,
        FONT_TINY,
        TRACKING_TINY,
        color,
        true,
        false,
    );
    if resp.hovered() {
        let y = rect.center().y + FONT_TINY * 0.72;
        ui.painter().line_segment(
            [pos2(link_rect.max.x, y), pos2(link_rect.min.x, y)],
            Stroke::new(STROKE_HAIRLINE, color),
        );
    }
    ui.add_space(4.0);
    let line = ui
        .allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover())
        .0;
    ui.painter()
        .line_segment([line.left_top(), line.right_top()], stroke_hairline());
    ui.add_space(GAP_PANEL_HEAD);
    resp.clicked()
}

/// 文本排版宽度（px）。仅用于给"可点标签"划热区，不参与最终绘制。
///
/// 走 `Painter::layout_job` 而不是 `Context::fonts`：0.37 的 `Fonts::layout_job`
/// 要 `&mut`，而 `Ui::fonts` 只给 `&`；`Painter` 那条路顺带吃到 egui 的排版缓存。
fn text_width(ui: &Ui, text: &str, size: f32, tracking: f32) -> f32 {
    let job = spec_job(text, size, tracking, pal().ink, true);
    ui.painter().layout_job(job).size().x
}

/// 同 [`info_row`]，但值可点（用于「点专辑名跳专辑页 / 点歌手名跳歌手页」）。
///
/// 返回 `true` = 这一行被点了。`clickable == false` 时行为与 [`info_row`] 完全一致。
pub fn info_row_link(
    ui: &mut Ui,
    label: &str,
    value: &str,
    height: f32,
    value_bold: bool,
    clickable: bool,
) -> bool {
    let sense = if clickable {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (row, resp) = ui.allocate_exact_size(vec2(ui.available_width(), height), sense);
    paint_spec_text(
        ui,
        row.left_center(),
        Align2::LEFT_CENTER,
        label,
        FONT_TINY,
        TRACKING_TINY,
        pal().mid,
        true,
        false,
    );
    let x = row.min.x + INFO_LABEL_W;
    let avail = (row.max.x - x).max(0.0);
    let text = fit_text(
        ui,
        value,
        FONT_SPINE_NAME,
        0.0,
        pal().ink,
        false,
        value_bold,
        avail,
    );
    // 可点且悬停 → 变蓝 + 下划线：不给暗示的话没人知道专辑名能点
    let hot = clickable && resp.hovered();
    let color = if hot { pal().blue } else { pal().ink };
    paint_spec_text(
        ui,
        pos2(x, row.center().y),
        Align2::LEFT_CENTER,
        &text,
        FONT_SPINE_NAME,
        0.0,
        color,
        false,
        value_bold,
    );
    if hot {
        let w = text_width(ui, &text, FONT_SPINE_NAME, 0.0);
        let y = row.center().y + FONT_SPINE_NAME * 0.68;
        ui.painter().line_segment(
            [pos2(x, y), pos2(x + w, y)],
            Stroke::new(STROKE_HAIRLINE, color),
        );
    }
    clickable && resp.clicked()
}

/// 信息行标签列宽（px）。
pub const INFO_LABEL_W: f32 = 58.0;

/// 带透明度的令牌色派生（如 spine-artist 62%）。不引入新色相。
///
/// **必须预乘**：egui 的颜色是预乘 alpha（`egui_glow` 用
/// `blend_func(ONE, ONE_MINUS_SRC_ALPHA)`），只改 alpha 而不缩放 RGB 会被当成
/// "全亮 + 低 alpha"，渲染出来接近**不透明**——白色尤其明显（`(255,255,255,26)`
/// 直接饱和成纯白）。玻璃盖板长期是一块白板、蓝色底纹的"12% 白条纹"变成纯白
/// 1px 线，根因都在这里。
pub fn with_opacity(color: Color32, opacity: f32) -> Color32 {
    let a = (opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
    let mul = |c: u8| ((c as u16 * a as u16 + 127) / 255) as u8;
    Color32::from_rgba_premultiplied(mul(color.r()), mul(color.g()), mul(color.b()), a)
}

// ---------------------------------------------------------------------------
// 纯逻辑辅助（单测覆盖）
// ---------------------------------------------------------------------------

/// `LoadState` 加载中的省略点动画帧（每 12 帧进一位）。
pub fn load_dots(frame: u64) -> &'static str {
    match (frame / 12) % 4 {
        0 => "",
        1 => "·",
        2 => "··",
        _ => "···",
    }
}

/// 加载徽标视图：文本 + 令牌色（纯函数，可测）。
pub fn badge_view<T>(state: &LoadState<T>, frame: u64, lang: Lang) -> (String, Color32) {
    match state {
        LoadState::Idle => (state.status_text(lang).to_string(), pal().mid),
        LoadState::Loading => (
            format!("{}{}", state.status_text(lang), load_dots(frame)),
            pal().blue,
        ),
        LoadState::Ready(_) => (state.status_text(lang).to_string(), pal().status_ok),
        LoadState::Failed(_) => (state.status_text(lang).to_string(), pal().error_mark),
    }
}

/// FNV-1a：曲目 ID → 封面种子（同 ID 恒同封面）。
pub fn hash_id(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

// ---------------------------------------------------------------------------
// §3.2 面板
// ---------------------------------------------------------------------------

/// 面板框（1.5px 描边 / panel 底 / 14px 内边距 / 圆角 0）。
pub fn panel_frame() -> egui::Frame {
    egui::Frame {
        fill: pal().panel,
        stroke: stroke_component(),
        inner_margin: margin_panel(),
        corner_radius: radius_none(),
        ..egui::Frame::NONE
    }
}

/// 在**指定矩形**内画面板，并返回内部 Ui 的执行结果。
///
/// 与 [`panel_frame`]`().show()` 的区别：矩形由调用方给定，面板因此可以
/// **撑满列高**——对应 CSS grid 的默认 `align-items: stretch`。
/// `panel_frame().show()` 只能得到内容高度，两栏页面里就会出现"左列比右列短"。
///
/// 内部 Ui 的 `item_spacing` 归零，间距一律由调用方显式 `add_space` 控制，
/// 避免 egui 默认 (8,3) 与手写间距叠加。
pub fn panel_at<R>(ui: &mut Ui, rect: Rect, add: impl FnOnce(&mut Ui) -> R) -> R {
    {
        let painter = ui.painter();
        painter.rect_filled(rect, radius_none(), pal().panel);
        painter.rect_stroke(rect, radius_none(), stroke_component(), StrokeKind::Inside);
    }
    // 内容裁剪到面板外框：选中态 translateX(-4) 落在内距里，不会被切
    let clip = ui.clip_rect().intersect(rect);
    let inner = rect.shrink(PAD_PANEL);
    ui.scope_builder(UiBuilder::new().max_rect(inner), |ui| {
        ui.set_clip_rect(clip);
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        add(ui)
    })
    .inner
}

/// 面板框（撑满给定高度），与 [`panel_frame`]`().show()` 同构、可原地替换。
///
/// `Frame` 自身只能得到内容高度；这里给内部 Ui 设最小高度，于是面板会长到
/// `height`——对应 CSS grid 的默认 `align-items: stretch`。
pub fn panel_in<R>(ui: &mut Ui, height: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    panel_frame()
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            ui.set_min_height((height - 2.0 * PAD_PANEL - 2.0 * STROKE_COMPONENT).max(0.0));
            add(ui)
        })
        .inner
}

/// 空态框高度：吃掉面板剩余高度（CSS grid 的 stretch），无界时退回 `min`。
///
/// 早期空态框写死 120px，窗口拉高后下方会留一大片空白（hot / playlist 页实测）。
/// `reserve` 留给空态框下方的按钮/说明块（不预留会把它们挤出面板）。
pub fn empty_height(ui: &Ui, min: f32, reserve: f32) -> f32 {
    let avail = ui.available_height();
    if avail.is_finite() {
        (avail - reserve).max(min)
    } else {
        min
    }
}

/// 面板头：`label`（§1.2 label 样式）+ 右侧 `tiny`，下压发丝线。
pub fn panel_head(ui: &mut Ui, label: &str, right: &str) {
    let height = FONT_LABEL + 4.0;
    let rect = ui
        .allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover())
        .0;
    paint_spec_text(
        ui,
        rect.left_center() + vec2(0.0, 0.5),
        Align2::LEFT_CENTER,
        &label.to_uppercase(),
        FONT_LABEL,
        TRACKING_LABEL,
        pal().ink,
        false,
        true,
    );
    paint_spec_text(
        ui,
        rect.right_center() + vec2(0.0, 0.5),
        Align2::RIGHT_CENTER,
        right,
        FONT_TINY,
        TRACKING_TINY,
        pal().mid,
        true,
        false,
    );
    ui.add_space(4.0);
    let line = ui
        .allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover())
        .0;
    ui.painter()
        .line_segment([line.left_top(), line.right_top()], stroke_hairline());
    ui.add_space(GAP_PANEL_HEAD);
}

// ---------------------------------------------------------------------------
// §3.3 spine 磁带架列表项
// ---------------------------------------------------------------------------

/// spine 列表项视图数据。
pub struct SpineView<'a> {
    /// 曲名（自动大写，§3.3 `text-transform: uppercase`）。
    pub title: &'a str,
    /// 艺人行（62% 不透明度）。
    pub artist: &'a str,
    /// 右侧徽标（如 "A"）。
    pub side: &'a str,
    /// 右侧时长（数字等宽）。
    pub length: &'a str,
    /// 左色条（accent；选中时变 acid）。
    pub accent: Color32,
    /// 是否选中（§3.3 三重编码）。
    pub selected: bool,
}

/// spine 高度（8 + 11.5×1.15 + 3 + 8 + 8）。
pub const SPINE_HEIGHT: f32 = 42.0;

/// 绘制一个 spine 列表项，返回点击响应。
///
/// 选中三重编码（§3.3）：蓝底反白 + `translateX(-4)` + 3px 投影 + 色条变 acid + 白边徽标；
/// hover：`translateX(-2)` + shelf-hover 底。
pub fn spine_item(ui: &mut Ui, _index: usize, view: &SpineView) -> Response {
    let width = ui.available_width();
    let (full_rect, response) =
        ui.allocate_exact_size(Vec2::new(width, SPINE_HEIGHT), Sense::click());

    let hovered = response.hovered() && !view.selected;
    let dx = if view.selected {
        -4.0
    } else if hovered {
        -2.0
    } else {
        0.0
    };
    let rect = full_rect.translate(vec2(dx, 0.0));
    let painter = ui.painter();

    // 硬投影（选中 3px ink / 常态 2px shelf）
    let shadow = if view.selected {
        shadow_selected()
    } else {
        shadow_shelf()
    };
    painter.add(shadow.as_shape(rect, radius_none()));

    // 底与描边
    let bg = if view.selected {
        pal().blue
    } else if hovered {
        pal().shelf_hover
    } else {
        pal().paper
    };
    painter.rect(
        rect,
        radius_none(),
        bg,
        stroke_component(),
        StrokeKind::Inside,
    );

    // 左色条 9px（选中变强调标记色）
    let chip_color = if view.selected {
        pal().selected_mark
    } else {
        view.accent
    };
    let chip = Rect::from_min_size(rect.min, vec2(9.0, rect.height()));
    painter.rect_filled(chip, radius_none(), chip_color);

    // 主体：底色是强调色/浅金 hover 时，字色必须现挑深色（暗色档的 ink 是近白）
    let text_color = if view.selected {
        fg_on(pal().blue)
    } else if hovered {
        fg_on(pal().shelf_hover)
    } else {
        pal().ink
    };
    let artist_opacity = SPINE_ARTIST_ALPHA as f32 / 255.0;
    let artist_color = with_opacity(text_color, artist_opacity);
    let body = Rect::from_min_max(
        pos2(rect.min.x + 19.0, rect.min.y + 8.0),
        pos2(rect.max.x - 62.0, rect.max.y - 8.0),
    );
    let name_job = truncate_to(
        spec_job(
            &view.title.to_uppercase(),
            FONT_SPINE_NAME,
            TRACKING_SPINE_NAME,
            text_color,
            false,
        ),
        body.width(),
    );
    paint_job(ui, body.left_top(), Align2::LEFT_TOP, name_job, true);
    let artist_job = truncate_to(
        spec_job(
            &view.artist.to_uppercase(),
            FONT_SPINE_ARTIST,
            TRACKING_SPINE_ARTIST,
            artist_color,
            false,
        ),
        body.width(),
    );
    paint_job(
        ui,
        body.left_top() + vec2(0.0, FONT_SPINE_NAME * 1.15 + 3.0),
        Align2::LEFT_TOP,
        artist_job,
        false,
    );

    // 右尾栏（徽标 + 时长）
    let end = Rect::from_min_max(pos2(rect.max.x - 62.0, rect.min.y), rect.max);
    let (end_bg, end_line) = if view.selected {
        (pal().spine_end_bg_sel, pal().spine_end_line_sel)
    } else {
        (pal().spine_end_bg, pal().spine_end_line)
    };
    painter.rect_filled(end, radius_none(), end_bg);
    painter.line_segment(
        [end.left_top(), end.left_bottom()],
        Stroke::new(1.0, end_line),
    );

    // 白边徽标（选中态反白）
    let badge_color = with_opacity(text_color, 0.75);
    let badge_text = spec_job(
        view.side,
        FONT_SPINE_ARTIST,
        TRACKING_TINY,
        badge_color,
        false,
    );
    let badge_rect = paint_job(
        ui,
        end.center() - vec2(0.0, 8.0),
        Align2::CENTER_CENTER,
        badge_text,
        true,
    )
    .expand(3.0);
    painter.rect_stroke(
        badge_rect,
        radius_none(),
        Stroke::new(1.0, badge_color),
        StrokeKind::Outside,
    );

    let len_text = spec_job(
        view.length,
        FONT_SPINE_ARTIST + 0.5,
        TRACKING_TINY,
        text_color,
        true,
    );
    paint_job(
        ui,
        end.center() + vec2(0.0, 9.0),
        Align2::CENTER_CENTER,
        len_text,
        false,
    );

    response
}

// ---------------------------------------------------------------------------
// §3.4 控制按钮
// ---------------------------------------------------------------------------

/// 按钮规格（§3.4：37×34 / 主键 54×42 acid）。
pub struct ControlSpec {
    /// 字形（▶/❚❚ 等）。
    pub glyph: &'static str,
    /// 是否主键（acid 底 54×42）。
    pub primary: bool,
    /// 持续按下态（toggle 类按钮）。
    pub toggled: bool,
    /// **强调态**：主键在「正在播放」时为 `true`；模式切换键在当前模式生效时为 `true`。
    ///
    /// 与 `toggled` 分开是有意的：`toggled` 会让按钮下沉 2px（看起来像被按住），
    /// 而强调态只是**状态标注**，按钮不该有位移，只换底色。
    pub active: bool,
}

/// 控制键底色：**强调态（`active`）用 acid，其余与模块面板同色（PANEL）**。
///
/// 用户要求：播放中主键为绿色强调色，暂停时与面板背景同色；模式键（顺序/随机、
/// 列表/单曲循环）用**同一种** acid 底标注当前生效的那一态。
/// 之前只看 `primary && active`，于是暂停时按钮仍是亮绿，视觉上无法区分"在播/没播"。
pub fn control_fill(spec: &ControlSpec) -> Color32 {
    if spec.active {
        pal().acid
    } else {
        pal().panel
    }
}

/// 控制键的按钮尺寸（主键 54×42，普通键 37×34）。
pub fn control_size(spec: &ControlSpec) -> Vec2 {
    if spec.primary {
        vec2(54.0, 42.0)
    } else {
        vec2(37.0, 34.0)
    }
}

/// 控制键的**视觉**高度：按钮高度 + 2px 硬投影。
///
/// 共轴对齐要用它算中心：投影只往下偏，视觉重心比几何中心低 1px。
pub fn control_visual_height(spec: &ControlSpec) -> f32 {
    control_size(spec).y + 2.0
}

/// 一行控制键的**统一中心线布局**。
///
/// 返回每个键的中心点。所有 `y` 完全相同——这是本设计里唯一不能靠肉眼保证的
/// 约束：普通键 34px、主键 42px 混排时，若逐个 `ui.horizontal` 放置，
/// egui 会把它们**顶边对齐**（主键多出的 8px 全堆在下方，中心低 4px）。
///
/// `y` 取 `row.min.y + (row.height() - 2) / 2`：硬投影只向右下偏 2px，
/// 所以「按钮矩形中心」比「视觉中心」高 1px，两种高度的键因此得到同一个视觉中心。
///
/// 水平方向按实际宽度累加并整体居中，末键投影右伸 2px 故左移 1px 做光学补偿。
pub fn control_row_layout(row: Rect, specs: &[&ControlSpec], gap: f32) -> Vec<Pos2> {
    let total_w: f32 = specs.iter().map(|s| control_size(s).x).sum::<f32>()
        + gap * (specs.len().saturating_sub(1)) as f32;
    let cy = row.min.y + (row.height() - 2.0) / 2.0;
    let mut x = row.min.x + ((row.width() - total_w) / 2.0 - 1.0).max(0.0);
    let mut centers = Vec::with_capacity(specs.len());
    for spec in specs {
        let w = control_size(spec).x;
        centers.push(pos2(x + w / 2.0, cy));
        x += w + gap;
    }
    centers
}

/// 绘制控制按钮。按下时 `translate(2,2)` 且投影消失（§3.4，0ms）。
pub fn control_button(ui: &mut Ui, id: egui::Id, spec: &ControlSpec) -> Response {
    let size = control_size(spec);
    let (rect, _) = ui.allocate_exact_size(size, Sense::click());
    let response = ui.interact(rect, id, Sense::click());
    let pressed = response.is_pointer_button_down_on() || spec.toggled;
    paint_control_button(ui, rect, spec, pressed);
    response
}

/// 在**指定中心**绘制控制键，供需要严格共轴的多键行使用。
///
/// 为什么需要它：`ui.horizontal` 里普通键（34px）与主键（42px）混排时，
/// egui 按「放置当时的行高」决定垂直位置，**不会回头重新居中已放下的控件**，
/// 于是五个键的中心落在不同水平线上（差 4px 肉眼可见）。
/// 调用方先整行 `allocate_exact_size`，再用同一 `center.y` 逐个放置即可严格共轴。
///
/// 不自己推进光标：整行高度由调用方的 `allocate_exact_size` 负责。
pub fn control_button_at(ui: &mut Ui, id: egui::Id, spec: &ControlSpec, center: Pos2) -> Response {
    let rect = Rect::from_center_size(center, control_size(spec));
    let response = ui.interact(rect, id, Sense::click());
    let pressed = response.is_pointer_button_down_on() || spec.toggled;
    paint_control_button(ui, rect, spec, pressed);
    response
}

fn paint_control_button(ui: &Ui, rect: Rect, spec: &ControlSpec, pressed: bool) {
    let rect = rect.translate(if pressed { vec2(2.0, 2.0) } else { Vec2::ZERO });
    let painter = ui.painter();

    if !pressed {
        painter.add(shadow_button().as_shape(rect, radius_none()));
    }
    let fill = control_fill(spec);
    painter.rect(
        rect,
        radius_none(),
        fill,
        stroke_component(),
        StrokeKind::Inside,
    );

    let glyph_size = if spec.primary { 18.0 } else { 14.0 };
    paint_spec_text(
        ui,
        rect.center(),
        Align2::CENTER_CENTER,
        spec.glyph,
        glyph_size,
        0.0,
        fg_on(fill),
        false,
        true,
    );
}

/// 文本按钮的绘制宽度（与 [`text_button`] 内部算法一致，供调用方预留空间）。
pub fn text_button_width(ui: &Ui, text: &str) -> f32 {
    let job = spec_job(text, FONT_LABEL, TRACKING_LABEL, pal().ink, false);
    let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
    galley.size().x + 22.0
}

/// 紧凑文本按钮（14px 高，面板内的折叠开关/次要动作）。
///
/// 与 [`text_button`]（34px 工具栏高度 + 2px 硬投影）区分：信息面板里塞一个
/// 工具栏按钮会把整块撑高，这里只保留 1px 描边与 tiny 字距。
pub fn tiny_button(ui: &mut Ui, id: egui::Id, text: &str, active: bool) -> Response {
    let fill = if active { pal().acid } else { pal().panel_alt };
    let job = spec_job(text, FONT_TINY, TRACKING_TINY, fg_on(fill), true);
    let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
    let size = vec2(galley.size().x + 12.0, 14.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::click());
    let response = ui.interact(rect, id, Sense::click());
    ui.painter().rect(
        rect,
        radius_none(),
        fill,
        Stroke::new(1.0, pal().line),
        StrokeKind::Inside,
    );
    ui.painter().galley(
        rect.center() - galley.size() / 2.0,
        galley,
        Color32::TRANSPARENT,
    );
    response
}

/// 文本按钮（工具栏动作：SEARCH / LOAD / APPLY）。
///
/// 沿用 §3.4 按钮语言：1.5px 描边 + 2px 硬投影，按下 `translate(2,2)` 投影消失。
pub fn text_button(
    ui: &mut Ui,
    id: egui::Id,
    text: &str,
    primary: bool,
    toggled: bool,
) -> Response {
    let fill = if primary { pal().acid } else { pal().panel };
    let job = spec_job(text, FONT_LABEL, TRACKING_LABEL, fg_on(fill), false);
    let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
    let size = vec2(galley.size().x + 22.0, 34.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::click());
    let response = ui.interact(rect, id, Sense::click());
    let pressed = response.is_pointer_button_down_on() || toggled;
    let rect = rect.translate(if pressed { vec2(2.0, 2.0) } else { Vec2::ZERO });
    let painter = ui.painter();
    if !pressed {
        painter.add(shadow_button().as_shape(rect, radius_none()));
    }
    painter.rect(
        rect,
        radius_none(),
        fill,
        stroke_component(),
        StrokeKind::Inside,
    );
    painter.galley(
        rect.center() - galley.size() / 2.0,
        galley,
        Color32::TRANSPARENT,
    );
    response
}

// ---------------------------------------------------------------------------
// §3.4b 自绘滑杆（取代 egui::Slider 的默认灰底与数值框）
// ---------------------------------------------------------------------------

/// 滑杆占位高度（轨道 6px + 滑块 14px + 上下留白）。
pub const RANGE_HEIGHT: f32 = 18.0;

/// 自绘滑杆：`code (3).html` 的 `input[type=range]` + `accent-color:blue`。
///
/// 轨道 6px（`PANEL_ALT` 底 + 1px ink），已填充段 `BLUE`，滑块 8×14
/// （`PAPER` 底 + 1.5px ink）。`logarithmic` 为真时按对数映射（超时滑杆用）。
/// 返回 `(是否发生变化, 响应)`；响应供调用方判断 `drag_stopped`（松手再落盘）。
pub fn range_slider(
    ui: &mut Ui,
    id: egui::Id,
    value: &mut f32,
    min: f32,
    max: f32,
    width: f32,
    logarithmic: bool,
) -> (bool, Response) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, RANGE_HEIGHT), Sense::click_and_drag());
    if ui.rect_contains_pointer(rect) {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let response = ui.interact(rect, id, Sense::click_and_drag());

    let span = (max - min).max(f32::EPSILON);
    let ratio_of = |v: f32| -> f32 {
        if logarithmic {
            ((v.max(min).ln() - min.ln()) / (max.ln() - min.ln())).clamp(0.0, 1.0)
        } else {
            ((v - min) / span).clamp(0.0, 1.0)
        }
    };
    let value_of = |t: f32| -> f32 {
        if logarithmic {
            (min.ln() + t * (max.ln() - min.ln())).exp()
        } else {
            min + t * span
        }
    };

    let mut changed = false;
    if response.dragged() || response.clicked() {
        if let Some(p) = response.interact_pointer_pos() {
            let t = ((p.x - rect.min.x) / rect.width().max(1.0)).clamp(0.0, 1.0);
            *value = value_of(t);
            changed = true;
        }
    }

    let painter = ui.painter();
    let track = Rect::from_center_size(rect.center(), vec2(rect.width(), 6.0));
    painter.rect_filled(track, radius_none(), pal().panel_alt);
    painter.rect_stroke(track, radius_none(), stroke_hairline(), StrokeKind::Inside);

    let t = ratio_of(*value);
    let fill_w = track.width() * t;
    if fill_w > 0.5 {
        painter.rect_filled(
            Rect::from_min_size(track.min, vec2(fill_w, track.height())),
            radius_none(),
            pal().blue,
        );
    }
    // 滑块中心夹在轨道内，避免 t=0/1 时半个滑块出框
    let cx = (track.min.x + fill_w).clamp(track.min.x + 4.0, track.max.x - 4.0);
    let thumb = Rect::from_center_size(pos2(cx, rect.center().y), vec2(8.0, 14.0));
    painter.rect_filled(thumb, radius_none(), pal().thumb);
    painter.rect_stroke(thumb, radius_none(), stroke_component(), StrokeKind::Inside);

    (changed, response)
}

// ---------------------------------------------------------------------------
// §3.5 卡带（走带窗 + 印刷封面）
// ---------------------------------------------------------------------------

/// 封面的 UV 子矩形：**等比缩放 + 居中裁切**（CSS `object-fit: cover`）。
///
/// 为什么不直接铺 `0..1`：封面纹理是正方形（[`super::halftone`] 烘焙时已居中裁过），
/// 而容器常常是长方形（卡带封面的高随面板变化）。把正方形纹理拉进长方形，
/// 画面会被横向或纵向压扁——专辑封面一旦变形，用户第一眼就能看出来。
///
/// 做法：比较纹理与容器的宽高比，**多出来的那一维裁掉一半**（左右各一半或
/// 上下各一半），保留的那一维铺满。容器尺寸不变，只是取样范围变了，
/// 所以布局与既有像素栅格完全不动。
pub fn cover_uv(tex: Vec2, dest: Vec2) -> Rect {
    // 退化尺寸（0 宽/0 高）时回退整幅，绝不除零
    if tex.x <= 0.0 || tex.y <= 0.0 || dest.x <= 0.0 || dest.y <= 0.0 {
        return Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    }
    let tex_aspect = tex.x / tex.y;
    let dest_aspect = dest.x / dest.y;
    if tex_aspect > dest_aspect {
        // 纹理更宽 → 保留高度，左右各裁掉一半多余宽度
        let keep = dest_aspect / tex_aspect;
        let du = (1.0 - keep) * 0.5;
        Rect::from_min_max(pos2(du, 0.0), pos2(1.0 - du, 1.0))
    } else {
        // 纹理更高（或相等）→ 保留宽度，上下各裁掉一半多余高度
        let keep = tex_aspect / dest_aspect;
        let dv = (1.0 - keep) * 0.5;
        Rect::from_min_max(pos2(0.0, dv), pos2(1.0, 1.0 - dv))
    }
}

/// 卡带视图数据。
pub struct CassetteView<'a> {
    /// 标签主标题（大写 14/900）。
    pub title: &'a str,
    /// 副行（7.5/500，72% 透明）。
    pub artist: &'a str,
    /// 印刷封面纹理（§3.8 网纹已烘焙；未加载时留灰）。
    pub cover: Option<TextureId>,
    /// 轮毂旋转角（度）。播放中由走带时钟驱动，静止时固定 0。
    pub spin_deg: f32,
}

/// 卡带宽高比（§3.5 `aspect-ratio: 1.56/1`）。
pub const CASSETTE_RATIO: f32 = 1.56;

/// 卡带占舱体宽度的比例。
///
/// 0.96 时磁带几乎铺满舱体，舱盖（玻璃）只剩 2% 边缘，透明之后完全看不出
/// 盖板的存在；0.90 留出一圈玻璃边，玻璃+描边才有地方"立"起来。
pub const CASSETTE_FILL: f32 = 0.90;

/// 在给定矩形内绘制卡带（art 区内嵌用）。
pub fn cassette_at(ui: &Ui, rect: Rect, view: &CassetteView) -> Rect {
    let painter = ui.painter();

    // 壳 + 6px 硬投影
    painter.add(shadow_tape().as_shape(rect, radius_none()));
    painter.rect(
        rect,
        radius_none(),
        pal().paper,
        stroke_structural(),
        StrokeKind::Inside,
    );

    // 四角螺丝（§1.4 唯一圆角例外：5px 螺丝）
    for corner in [
        rect.min + vec2(6.5, 6.5),
        pos2(rect.max.x - 6.5, rect.min.y + 6.5),
        pos2(rect.min.x + 6.5, rect.max.y - 6.5),
        rect.max - vec2(6.5, 6.5),
    ] {
        painter.circle(corner, 2.5, pal().silver, Stroke::new(1.0, pal().ink));
    }

    // 内区（11 / 11 / 11 / 7）
    let inner = Rect::from_min_max(rect.min + vec2(11.0, 11.0), rect.max - vec2(11.0, 7.0));

    // 三段：标签（flex）+ 走带窗（34%）+ foot（11px），段距 6
    let foot_h = 11.0;
    let window_h = (inner.height() * 0.34).max(44.0);
    let label_h = inner.height() - window_h - foot_h - 12.0;

    let label_rect = Rect::from_min_size(inner.min, vec2(inner.width(), label_h));
    let window_rect = Rect::from_min_size(
        inner.min + vec2(0.0, label_h + 6.0),
        vec2(inner.width(), window_h),
    );
    let foot_rect = Rect::from_min_size(
        inner.min + vec2(0.0, label_h + window_h + 12.0),
        vec2(inner.width(), foot_h),
    );

    paint_cass_label(ui, label_rect, view);
    paint_tape_window(ui.painter(), window_rect, view.spin_deg);
    paint_cass_foot(ui.painter(), foot_rect);
    rect
}

/// 标签纸（6px 亮 / 1px 暗横纹）+ 顶行 / 封面 + 文本 / 底行。
fn paint_cass_label(ui: &Ui, rect: Rect, view: &CassetteView) {
    let painter = ui.painter();
    painter.rect_filled(rect, radius_none(), pal().label_light);
    // 横纹（7px 周期：6px 亮 + 1px 暗）
    let mut y = rect.min.y + 6.0;
    while y < rect.max.y {
        painter.rect_filled(
            Rect::from_min_max(
                pos2(rect.min.x, y),
                pos2(rect.max.x, (y + 1.0).min(rect.max.y)),
            ),
            radius_none(),
            pal().panel_alt,
        );
        y += 7.0;
    }
    painter.rect_stroke(rect, radius_none(), stroke_component(), StrokeKind::Inside);

    // 顶行：Reimu Player / TYPE II · C-60
    paint_spec_text(
        ui,
        pos2(rect.min.x + 8.0, rect.min.y + 6.0),
        Align2::LEFT_TOP,
        "Reimu Player",
        FONT_CASS_LABEL,
        TRACKING_CASS_LABEL,
        pal().ink,
        false,
        true,
    );
    paint_spec_text(
        ui,
        pos2(rect.max.x - 8.0, rect.min.y + 6.0),
        Align2::RIGHT_TOP,
        "TYPE II · C-60",
        FONT_CASS_LABEL,
        TRACKING_CASS_LABEL,
        pal().ink,
        false,
        true,
    );

    // 底行：SIDE A / STEREO / DOLBY NR（上压发丝线）
    let bottom_text_y = rect.max.y - 6.0 - FONT_CASS_LABEL;
    painter.line_segment(
        [
            pos2(rect.min.x + 8.0, bottom_text_y - 5.0),
            pos2(rect.max.x - 8.0, bottom_text_y - 5.0),
        ],
        stroke_hairline(),
    );
    for (text, x, anchor) in [
        ("SIDE A", rect.min.x + 8.0, Align2::LEFT_TOP),
        ("STEREO", rect.center().x, Align2::CENTER_TOP),
        ("DOLBY NR", rect.max.x - 8.0, Align2::RIGHT_TOP),
    ] {
        paint_spec_text(
            ui,
            pos2(x, bottom_text_y),
            anchor,
            text,
            FONT_CASS_LABEL,
            TRACKING_CASS_LABEL,
            pal().ink,
            false,
            true,
        );
    }

    // 中段：封面 + 标题/副行
    let mid = Rect::from_min_max(
        pos2(rect.min.x + 8.0, rect.min.y + 22.0),
        pos2(rect.max.x - 8.0, bottom_text_y - 10.0),
    );
    let cover_w = (mid.width() * 0.24).clamp(52.0, 78.0);
    let cover_rect = Rect::from_min_size(mid.min, vec2(cover_w, mid.height()));
    if let Some(texture) = view.cover {
        painter.image(
            texture,
            cover_rect,
            // 封面纹理恒为正方形（halftone 烘焙时居中裁过）；容器是长方形，
            // 这里按 cover 语义居中裁切，绝不拉伸
            cover_uv(Vec2::splat(1.0), cover_rect.size()),
            // 纹理乘色：必须是纯白（tint 语义），不随主题变化
            Color32::WHITE,
        );
    } else {
        painter.rect_filled(cover_rect, radius_none(), pal().mid);
    }
    painter.rect_stroke(
        cover_rect,
        radius_none(),
        stroke_component(),
        StrokeKind::Inside,
    );

    let text_left = cover_rect.max.x + 8.0;
    let text_width = rect.max.x - 8.0 - text_left;
    let title_job = truncate_to(
        spec_job(
            &view.title.to_uppercase(),
            FONT_CASS_TITLE,
            TRACKING_CASS_TITLE,
            pal().ink,
            false,
        ),
        text_width,
    );
    paint_job(
        ui,
        pos2(text_left, mid.center().y - 8.0),
        Align2::LEFT_CENTER,
        title_job,
        true,
    );
    let artist_job = truncate_to(
        spec_job(
            &view.artist.to_uppercase(),
            FONT_CASS_ARTIST,
            TRACKING_CASS_ARTIST,
            with_opacity(pal().ink, 0.72),
            false,
        ),
        text_width,
    );
    paint_job(
        ui,
        pos2(text_left, mid.center().y + 8.0),
        Align2::LEFT_CENTER,
        artist_job,
        false,
    );
}

/// 走带窗：黑底 + 双带盘 + 走带槽。`spin_deg` 驱动轮毂齿旋转（播放走带）。
fn paint_tape_window(painter: &egui::Painter, rect: Rect, spin_deg: f32) {
    painter.rect(
        rect,
        radius_none(),
        pal().tape_black,
        stroke_component(),
        StrokeKind::Inside,
    );

    let reel_size = (rect.height() - 8.0).min(52.0);
    for cx in [
        rect.min.x + 12.0 + reel_size / 2.0,
        rect.max.x - 12.0 - reel_size / 2.0,
    ] {
        let center = pos2(cx, rect.center().y);
        // 磁带环（同心交替）
        painter.circle_filled(center, reel_size / 2.0, pal().reel_bg);
        for (k, scale) in [0.94_f32, 0.82, 0.70, 0.58].into_iter().enumerate() {
            let color = if k % 2 == 0 {
                pal().reel_pack
            } else {
                pal().reel_pack_dark
            };
            painter.circle_filled(center, reel_size / 2.0 * scale, color);
        }
        // 轮毂（§1.1 acid 轮毂齿）
        let hub_r = reel_size / 2.0 * 0.24;
        painter.circle_filled(center, hub_r, pal().hub_dark);
        for spoke in 0..8 {
            let angle = spoke as f32 * std::f32::consts::TAU / 8.0 + spin_deg.to_radians();
            let dir = vec2(angle.cos(), angle.sin());
            painter.line_segment(
                [center, center + dir * hub_r * 0.92],
                Stroke::new(1.6, pal().ornament),
            );
        }
        painter.circle(
            center,
            reel_size / 2.0,
            Color32::TRANSPARENT,
            Stroke::new(1.0, pal().slot_black),
        );
    }

    // 走带槽
    let slot = Rect::from_center_size(
        rect.center(),
        vec2(rect.width() * 0.32, rect.height() * 0.44),
    );
    painter.rect_filled(slot, radius_none(), pal().slot_black);
    painter.rect_stroke(
        slot,
        radius_none(),
        Stroke::new(1.0, pal().slot_inner),
        StrokeKind::Inside,
    );
}

/// foot：两点 + 滑槽。
fn paint_cass_foot(painter: &egui::Painter, rect: Rect) {
    for cx in [rect.min.x + 12.0, rect.max.x - 12.0] {
        painter.circle(
            pos2(cx, rect.center().y),
            3.5,
            pal().silver,
            Stroke::new(1.0, pal().ink),
        );
    }
    let slot = Rect::from_center_size(rect.center(), vec2(rect.width() * 0.34, 6.0));
    painter.rect(
        slot,
        radius_none(),
        pal().fslot,
        stroke_hairline(),
        StrokeKind::Inside,
    );
}

// ---------------------------------------------------------------------------
// §3.8 状态徽标
// ---------------------------------------------------------------------------

/// 绘制加载徽标小胶囊（`LoadState` 状态 + 动画点）。
pub fn load_badge(ui: &mut Ui, text: &str, color: Color32) {
    let job = spec_job(text, FONT_TINY, TRACKING_TINY, color, false);
    let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
    let size = galley.size() + vec2(12.0, 6.0);
    let rect = ui.allocate_exact_size(size, Sense::hover()).0;
    ui.painter().rect(
        rect,
        radius_none(),
        pal().paper,
        Stroke::new(1.0, color),
        StrokeKind::Inside,
    );
    ui.painter().galley(
        rect.center() - galley.size() / 2.0,
        galley,
        Color32::TRANSPARENT,
    );
}

// ---------------------------------------------------------------------------
// §3.9 Toast 与错误模态
// ---------------------------------------------------------------------------

/// Toast（acid 底 / 2px ink / 3px 硬投影），底部居中。
/// Toast 最小内容宽度（px）。
///
/// 之前宽度完全由文案决定，`SHUFFLE ON` 这类短句会缩成一颗窄胶囊，
/// 在 1100px 窗口底部看起来像掉了个碎片。给一个下限后提示条有了"横幅"的体量。
pub const TOAST_MIN_W: f32 = 232.0;

pub fn toast(ctx: &egui::Context, message: &str) {
    let job = spec_job(message, FONT_SPINE_NAME, 0.0, fg_on(pal().acid), false);
    let text_w = ctx.fonts_mut(|f| f.layout_job(job.clone())).size().x;
    // 内容区宽度：至少 TOAST_MIN_W，至多不撑出窗口（左右各留 24px）
    let content_w = text_w
        .max(TOAST_MIN_W)
        .min((ctx.content_rect().width() - 76.0).max(TOAST_MIN_W));
    egui::Area::new(egui::Id::unique("reimu-player-toast"))
        .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -24.0))
        .order(egui::Order::Foreground)
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame {
                fill: pal().acid,
                stroke: stroke_structural(),
                shadow: shadow_toast(),
                inner_margin: Margin::symmetric(14, 9),
                corner_radius: radius_none(),
                ..egui::Frame::NONE
            }
            .show(ui, |ui| {
                // 文案在提示条内居中（定宽后靠左会显得空半边）
                ui.set_min_width(content_w);
                ui.vertical_centered(|ui| {
                    ui.label(job);
                });
            });
        });
}

/// 错误模态（规范 8.2：错误用模态，不用 toast）。
///
/// 返回 `true` 表示本帧点击了确认（调用方应清除模态）。
pub fn error_modal(ctx: &egui::Context, title: &str, message: &str, ok_label: &str) -> bool {
    let mut dismissed = false;
    let screen = ctx.content_rect();
    egui::Area::new(egui::Id::unique("reimu-player-error-modal"))
        .fixed_pos(screen.min)
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            // 压暗层
            ui.painter()
                .rect_filled(screen, radius_none(), pal().modal_dim);

            let modal_w = 380.0_f32.min((screen.width() - 40.0).max(240.0));
            let text_w = (modal_w - 32.0).max(80.0);
            let msg_galley = ui.ctx().fonts_mut(|f| {
                f.layout_job(wrapped_spec_job(
                    message,
                    FONT_SPINE_NAME,
                    0.0,
                    pal().ink,
                    false,
                    text_w,
                ))
            });
            // 高度跟着正文实际行数走：长文案折行后纸面自动长高，不压住按钮
            let modal_h = (52.0 + msg_galley.size().y + 12.0 + 42.0 + 16.0).max(168.0);
            let modal_h = modal_h.min((screen.height() - 40.0).max(140.0));
            let modal = Rect::from_center_size(screen.center(), vec2(modal_w, modal_h));
            let painter = ui.painter();

            painter.add(shadow_window().as_shape(modal, radius_none()));
            painter.rect(
                modal,
                radius_none(),
                pal().panel,
                stroke_structural(),
                StrokeKind::Inside,
            );

            // 头部：错误标记 + 标题
            let head = Rect::from_min_size(modal.min, vec2(modal_w, 30.0));
            painter.rect_filled(
                Rect::from_min_size(head.min, vec2(6.0, head.height())),
                radius_none(),
                pal().error_mark,
            );
            paint_spec_text(
                ui,
                head.left_center() + vec2(14.0, 0.0),
                Align2::LEFT_CENTER,
                title,
                FONT_LABEL,
                TRACKING_LABEL,
                pal().ink,
                false,
                true,
            );
            painter.line_segment([head.left_bottom(), head.right_bottom()], stroke_hairline());

            // 正文（人话提示，不暴露 JSON/堆栈；按纸面宽度折行）
            painter.galley(
                modal.min + vec2(16.0, 52.0),
                msg_galley,
                Color32::TRANSPARENT,
            );

            // 确认按钮（控制按钮风格，主键）
            let btn_rect = Rect::from_min_size(
                pos2(modal.max.x - 16.0 - 54.0, modal.max.y - 16.0 - 42.0),
                vec2(54.0, 42.0),
            );
            painter.add(shadow_button().as_shape(btn_rect, radius_none()));
            painter.rect(
                btn_rect,
                radius_none(),
                pal().acid,
                stroke_component(),
                StrokeKind::Inside,
            );
            paint_spec_text(
                ui,
                btn_rect.center(),
                Align2::CENTER_CENTER,
                ok_label,
                FONT_LABEL,
                TRACKING_LABEL,
                fg_on(pal().acid),
                false,
                true,
            );
            if ui
                .interact(btn_rect, ui.scope_id().with("modal-ok"), Sense::click())
                .clicked()
            {
                dismissed = true;
            }
        });
    dismissed
}

/// 二次确认模态（删除类动作）。
///
/// 返回 `None` = 仍在等待；`Some(true)` = 确认；`Some(false)` = 取消/Esc。
/// 与 [`error_modal`] 同一套框架（压暗层 + 纸面 + 错误色左条），两个按钮
/// 并排，不依赖任何计时器。
pub fn confirm_modal(
    ctx: &egui::Context,
    title: &str,
    message: &str,
    confirm_label: &str,
    cancel_label: &str,
) -> Option<bool> {
    let mut result: Option<bool> = None;
    let screen = ctx.content_rect();
    egui::Area::new(egui::Id::unique("reimu-player-confirm-modal"))
        .fixed_pos(screen.min)
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            ui.painter()
                .rect_filled(screen, radius_none(), pal().modal_dim);
            // 挡住模态之外的点击：压暗层必须同时是一层"吸收命中"的背板
            ui.interact(screen, ui.scope_id().with("confirm-block"), Sense::click());

            let modal_w = 420.0_f32.min((screen.width() - 40.0).max(260.0));
            let text_w = (modal_w - 32.0).max(80.0);
            let msg_galley = ui.ctx().fonts_mut(|f| {
                f.layout_job(wrapped_spec_job(
                    message,
                    FONT_SPINE_NAME,
                    0.0,
                    pal().ink,
                    false,
                    text_w,
                ))
            });
            // 长文案（歌单名 + 说明）必须折行，纸面高度随行数增长
            let modal_h = (50.0 + msg_galley.size().y + 14.0 + 34.0 + 16.0).max(172.0);
            let modal_h = modal_h.min((screen.height() - 40.0).max(140.0));
            let modal = Rect::from_center_size(screen.center(), vec2(modal_w, modal_h));
            let painter = ui.painter();

            painter.add(shadow_window().as_shape(modal, radius_none()));
            painter.rect(
                modal,
                radius_none(),
                pal().panel,
                stroke_structural(),
                StrokeKind::Inside,
            );

            let head = Rect::from_min_size(modal.min, vec2(modal_w, 30.0));
            painter.rect_filled(
                Rect::from_min_size(head.min, vec2(6.0, head.height())),
                radius_none(),
                pal().error_mark,
            );
            paint_spec_text(
                ui,
                head.left_center() + vec2(14.0, 0.0),
                Align2::LEFT_CENTER,
                title,
                FONT_LABEL,
                TRACKING_LABEL,
                pal().ink,
                false,
                true,
            );
            painter.line_segment([head.left_bottom(), head.right_bottom()], stroke_hairline());

            painter.galley(
                modal.min + vec2(16.0, 50.0),
                msg_galley,
                Color32::TRANSPARENT,
            );

            let btn = vec2(72.0, 34.0);
            let confirm_rect = Rect::from_min_size(
                pos2(modal.max.x - 16.0 - 72.0, modal.max.y - 16.0 - 34.0),
                btn,
            );
            let cancel_rect = confirm_rect.translate(vec2(-72.0 - 10.0, 0.0));

            // 取消：面板底普通键
            painter.rect(
                cancel_rect,
                radius_none(),
                pal().panel,
                stroke_component(),
                StrokeKind::Inside,
            );
            paint_spec_text(
                ui,
                cancel_rect.center(),
                Align2::CENTER_CENTER,
                cancel_label,
                FONT_LABEL,
                TRACKING_LABEL,
                pal().ink,
                false,
                true,
            );
            // 确认：错误色（危险动作）
            painter.rect(
                confirm_rect,
                radius_none(),
                pal().error_mark,
                stroke_component(),
                StrokeKind::Inside,
            );
            paint_spec_text(
                ui,
                confirm_rect.center(),
                Align2::CENTER_CENTER,
                confirm_label,
                FONT_LABEL,
                TRACKING_LABEL,
                fg_on(pal().error_mark),
                false,
                true,
            );

            if ui
                .interact(
                    cancel_rect,
                    ui.scope_id().with("confirm-cancel"),
                    Sense::click(),
                )
                .clicked()
            {
                result = Some(false);
            }
            if ui
                .interact(
                    confirm_rect,
                    ui.scope_id().with("confirm-ok"),
                    Sense::click(),
                )
                .clicked()
            {
                result = Some(true);
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                result = Some(false);
            }
        });
    result
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// §3.10 页面导航（code (3).html .page-nav / .nav-btn）
// ---------------------------------------------------------------------------

/// 导航按钮横向内边距（字形槽 26 + 右侧留白 16）。
pub const NAV_BUTTON_PAD: f32 = 42.0;

/// 导航按钮高度。
pub const NAV_BUTTON_HEIGHT: f32 = 30.0;

/// 页面导航按钮宽度（与 [`nav_button`] 同源计算，供居中排版累加）。
///
/// 抽出来的原因：导航行靠"总宽 → 两侧留白"居中，早期调用方用近似公式
/// 估算宽度，页签一多误差累积，整排会明显偏左。
pub fn nav_button_width(ui: &Ui, label: &str) -> f32 {
    let job = spec_job(&label.to_uppercase(), FONT_TINY, 0.12, pal().ink, false);
    let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
    galley.size().x + NAV_BUTTON_PAD
}

/// 页面导航按钮（1.5px ink + 2px 硬投影；选中蓝底反白、字形 acid）。
pub fn nav_button(ui: &mut Ui, id: egui::Id, glyph: &str, label: &str, selected: bool) -> Response {
    let job = spec_job(&label.to_uppercase(), FONT_TINY, 0.12, pal().ink, false);
    let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
    let size = vec2(galley.size().x + NAV_BUTTON_PAD, NAV_BUTTON_HEIGHT);
    let (rect, _) = ui.allocate_exact_size(size, Sense::click());
    let response = ui.interact(rect, id, Sense::click());
    let pressed = response.is_pointer_button_down_on();
    let rect = rect.translate(if pressed { vec2(2.0, 2.0) } else { Vec2::ZERO });
    let painter = ui.painter();
    if !pressed {
        painter.add(shadow_button().as_shape(rect, radius_none()));
    }
    painter.rect(
        rect,
        radius_none(),
        if selected { pal().blue } else { pal().panel },
        stroke_component(),
        StrokeKind::Inside,
    );
    paint_spec_text(
        ui,
        rect.left_center() + vec2(10.0, 0.0),
        Align2::LEFT_CENTER,
        glyph,
        11.0,
        0.0,
        if selected {
            pal().selected_mark
        } else {
            pal().ink
        },
        false,
        true,
    );
    paint_spec_text(
        ui,
        rect.left_center() + vec2(26.0, 0.5),
        Align2::LEFT_CENTER,
        &label.to_uppercase(),
        FONT_TINY,
        0.12,
        if selected {
            fg_on(pal().blue)
        } else {
            pal().ink
        },
        false,
        false,
    );
    response
}

// ---------------------------------------------------------------------------
// §3.11 波形走带 / 时间行（code (3).html .wave / .time-row）
// ---------------------------------------------------------------------------

/// 波形条数量（code (3).html：62 条）。
pub const WAVE_BARS: usize = 62;

/// 波形条高百分比（10–98，确定性：同 i 恒同高）。
pub fn wave_height(i: usize) -> f32 {
    let x = i as f32;
    10.0 + ((x * 1.7).sin() * (x * 0.29).cos()).abs() * 88.0
}

/// 波形拖动回传：拖动中只预览，松手才提交 seek。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WaveSeek {
    /// 目标进度（0..=1）。
    pub ratio: f32,
    /// `true` = 点击/松手，调用方应真正 seek；`false` = 拖动预览。
    pub commit: bool,
}

/// 播放进度波形（39px 高，2px 间隔）。
///
/// 可点可拖：拖动过程只回传 `commit = false` 的预览值，松手（或单击）回传
/// `commit = true`——避免每帧 seek 造成音频卡顿。
pub fn waveform(ui: &mut Ui, id: egui::Id, ratio: f32) -> (Response, Option<WaveSeek>) {
    let (rect, _) =
        ui.allocate_exact_size(vec2(ui.available_width(), 39.0), Sense::click_and_drag());
    if ui.rect_contains_pointer(rect) {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let response = ui.interact(rect, id, Sense::click_and_drag());
    let painter = ui.painter();
    let gap = 2.0;
    let bar_w = ((rect.width() - gap * (WAVE_BARS as f32 - 1.0)) / WAVE_BARS as f32).max(2.0);
    let ratio = ratio.clamp(0.0, 1.0);
    for i in 0..WAVE_BARS {
        let h = (wave_height(i) / 100.0 * rect.height()).clamp(2.0, rect.height());
        let x = rect.min.x + i as f32 * (bar_w + gap);
        let bar = Rect::from_min_max(
            pos2(x, rect.center().y - h / 2.0),
            pos2(x + bar_w, rect.center().y + h / 2.0),
        );
        let active = (i as f32 + 0.5) / WAVE_BARS as f32 <= ratio;
        painter.rect_filled(
            bar,
            radius_none(),
            if active { pal().blue } else { pal().wave_bar },
        );
    }
    // 底压发丝线（.wave border-bottom）
    painter.line_segment([rect.left_bottom(), rect.right_bottom()], stroke_hairline());
    // 拖动预览光标：一条 2px 竖线
    if response.dragged() {
        if let Some(p) = response.interact_pointer_pos() {
            painter.line_segment(
                [pos2(p.x, rect.min.y), pos2(p.x, rect.max.y)],
                Stroke::new(2.0, with_opacity(pal().ink, 0.55)),
            );
        }
    }
    let at_pointer = |r: &Response| {
        r.interact_pointer_pos()
            .map(|p| ((p.x - rect.min.x) / rect.width()).clamp(0.0, 1.0))
    };
    let seek = if response.clicked() || response.drag_stopped() {
        at_pointer(&response).map(|ratio| WaveSeek {
            ratio,
            commit: true,
        })
    } else if response.dragged() {
        at_pointer(&response).map(|ratio| WaveSeek {
            ratio,
            commit: false,
        })
    } else {
        None
    };
    (response, seek)
}

/// 时间行文本（`mm:ss` / `mm:ss`，数字等宽）。点击切换「已播 / 剩余」。
pub fn time_row(ui: &mut Ui, elapsed: &str, total: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 12.0), Sense::click());
    if ui.rect_contains_pointer(rect) {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    paint_spec_text(
        ui,
        rect.left_center(),
        Align2::LEFT_CENTER,
        elapsed,
        FONT_TINY,
        0.0,
        pal().ink,
        true,
        false,
    );
    paint_spec_text(
        ui,
        rect.right_center(),
        Align2::RIGHT_CENTER,
        total,
        FONT_TINY,
        0.0,
        pal().ink,
        true,
        false,
    );
    response
}

// ---------------------------------------------------------------------------
// §3.12 歌词面板（code (3).html .lyric-wrap / .lyrics）
// ---------------------------------------------------------------------------

/// 歌词行视图。
pub struct LyricRow<'a> {
    /// 左侧时间戳文本（`mm:ss`）。
    pub time_text: &'a str,
    /// 歌词文本（**上行**）。
    pub text: &'a str,
    /// 辅助行文本（译文或罗马音，**下行**）；无则 `None`。
    ///
    /// 阶段 22 起改为「上行原文、下行译文」——与桌面歌词窗完全一致。
    /// 辅助行字号更小、透明度更低。`None` 时行高与排版跟没有这个字段时一模一样。
    pub aux: Option<&'a str>,
}

/// 歌词正文的字号。
///
/// 阶段 22 由 11 提到 14：歌词区是主窗口里"最该读得清"的一块，
/// 原字号在高分屏上几乎是注释级别。
const LYRIC_TEXT_SIZE: f32 = 14.0;

/// 辅助行（译文 / 罗马音）的字号。
///
/// 比正文小一号：它是「原文的注脚」，不能和原文抢视觉层级。
const LYRIC_AUX_SIZE: f32 = 12.0;

/// 辅助行与原文之间的行距。
const LYRIC_AUX_GAP: f32 = 3.0;

/// 歌词行的高度。
///
/// `aux_h` 是辅助行 galley 的高度，**没有辅助行时传 0**——此时公式退化成
/// `text_h + 10`，与加辅助行之前完全一致（关闭态的像素回归基线）。
fn lyric_row_height(aux_h: f32, text_h: f32) -> f32 {
    let aux = if aux_h > 0.0 {
        aux_h + LYRIC_AUX_GAP
    } else {
        0.0
    };
    (aux + text_h + 10.0).max(24.0)
}

/// 歌词面板。返回被点击行的下标（→ seek 到该行时间戳）。
///
/// `active` 为当前播放行（蓝底反白 + acid 左条）；
/// `focus` 为需要滚到可见区中心的行（仅在换行帧传 `Some`）。
pub fn lyric_panel(
    ui: &mut Ui,
    lang: Lang,
    id: egui::Id,
    rows: &[LyricRow],
    active: Option<usize>,
    focus: Option<usize>,
    body_h: f32,
) -> Option<usize> {
    let mut clicked = None;

    // 外框 + 头条（条纹底）
    egui::Frame {
        fill: pal().label_light,
        stroke: stroke_component(),
        inner_margin: Margin::ZERO,
        corner_radius: radius_none(),
        ..egui::Frame::NONE
    }
    .show(ui, |ui| {
        let width = ui.available_width();
        let head = ui.allocate_exact_size(vec2(width, 24.0), Sense::hover()).0;
        ui.painter()
            .rect_filled(head, radius_none(), pal().panel_alt);
        let mut y = head.min.y + 6.0;
        while y < head.max.y {
            ui.painter().rect_filled(
                Rect::from_min_max(
                    pos2(head.min.x, y),
                    pos2(head.max.x, (y + 1.0).min(head.max.y)),
                ),
                radius_none(),
                with_opacity(pal().ink, 0.12),
            );
            y += 7.0;
        }
        paint_spec_text(
            ui,
            head.left_center() + vec2(22.0, 0.0),
            Align2::LEFT_CENTER,
            pick(lang, "Lyric Sheet / 歌词", "Lyric Sheet"),
            FONT_LABEL,
            TRACKING_LABEL,
            pal().ink,
            false,
            true,
        );
        paint_spec_text(
            ui,
            head.right_center() + vec2(-8.0, 0.0),
            Align2::RIGHT_CENTER,
            pick(lang, "点歌词行可跳转", "CLICK A LINE TO SEEK"),
            FONT_TINY,
            TRACKING_TINY,
            pal().mid,
            true,
            false,
        );
        ui.painter()
            .line_segment([head.left_bottom(), head.right_bottom()], stroke_hairline());

        // 左侧标尺槽（16px 横纹 + 虚线右缘）
        let body_top = head.max.y;
        let body_h = body_h.max(24.0);
        let body = Rect::from_min_size(pos2(ui.cursor().min.x, body_top), vec2(width, body_h));
        let mut ry = body.min.y + 16.0;
        while ry < body.max.y {
            ui.painter().line_segment(
                [pos2(body.min.x, ry), pos2(body.min.x + 16.0, ry)],
                Stroke::new(1.0, with_opacity(pal().ink, 0.10)),
            );
            ry += 17.0;
        }
        // 标尺右缘虚线
        let mut dy = body.min.y;
        while dy < body.max.y {
            ui.painter().line_segment(
                [
                    pos2(body.min.x + 16.0, dy),
                    pos2(body.min.x + 16.0, (dy + 6.0).min(body.max.y)),
                ],
                Stroke::new(1.0, with_opacity(pal().ink, 0.28)),
            );
            dy += 10.0;
        }

        ui.scope_builder(UiBuilder::new().max_rect(body), |ui| {
            egui::ScrollArea::vertical()
                .id_salt(id)
                .max_height(body_h)
                // 纵向**不许收缩**：歌词区的高度必须只由布局决定，与内容多少无关。
                // 旧值 `[false, true]` 会让无歌词（1 行占位）时 Frame 只包住那一行，
                // 面板塌成一条，载入歌词后又弹开——既破坏占位稳定性，也是明显的布局跳动。
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add_space(6.0);
                    for (i, row) in rows.iter().enumerate() {
                        let is_active = active == Some(i);
                        let row_w = ui.available_width() - 22.0;
                        // 歌词行允许换行（HTML `.lyric-line` 无 nowrap），行高随行数增长
                        let text_w = (row_w - 52.0 - 6.0).max(60.0);
                        // 三种底色（当前行 / 悬停 / 常态）各有自己的前景色；
                        // 先按"常态"排一次拿行高——颜色不影响度量，所以悬停
                        // 只换色重排、行高不抖。
                        let line_color = |hovered: bool| {
                            if is_active {
                                fg_on(pal().blue)
                            } else if hovered {
                                fg_on(pal().shelf_hover)
                            } else {
                                pal().ink
                            }
                        };
                        let layout = |ui: &Ui, text: &str, size: f32, color: Color32| {
                            let job = wrap_to(spec_job(text, size, 0.01, color, false), text_w, 3);
                            ui.ctx().fonts_mut(|f| f.layout_job(job))
                        };
                        let aux_text = row.aux.filter(|s| !s.trim().is_empty());
                        let mut galley = layout(ui, row.text, LYRIC_TEXT_SIZE, line_color(false));
                        let mut aux_galley = aux_text.map(|aux| {
                            layout(
                                ui,
                                aux,
                                LYRIC_AUX_SIZE,
                                with_opacity(line_color(false), 0.72),
                            )
                        });
                        let aux_h = aux_galley.as_ref().map_or(0.0, |g| g.size().y);
                        let row_h = lyric_row_height(aux_h, galley.size().y);
                        let (rect, _) = ui.allocate_exact_size(vec2(row_w, row_h), Sense::click());
                        let response = ui.interact(rect, id.with(i), Sense::click());
                        let hovered = !is_active && response.hovered();
                        // 悬停底色是浅金光：深色主题的浅色文字会糊掉，按底色重排一次
                        if hovered {
                            galley = layout(ui, row.text, LYRIC_TEXT_SIZE, line_color(true));
                            aux_galley = aux_text.map(|aux| {
                                layout(
                                    ui,
                                    aux,
                                    LYRIC_AUX_SIZE,
                                    with_opacity(line_color(true), 0.72),
                                )
                            });
                        }
                        let painter = ui.painter();
                        if is_active {
                            painter.rect_filled(rect, radius_none(), pal().blue);
                            painter.rect_filled(
                                Rect::from_min_size(rect.min, vec2(3.0, rect.height())),
                                radius_none(),
                                pal().selected_mark,
                            );
                        } else if hovered {
                            painter.rect_filled(rect, radius_none(), pal().shelf_hover);
                        }
                        paint_spec_text(
                            ui,
                            rect.left_center() + vec2(8.0, 0.0),
                            Align2::LEFT_CENTER,
                            row.time_text,
                            9.5,
                            0.1,
                            if is_active {
                                with_opacity(fg_on(pal().blue), 0.85)
                            } else if hovered {
                                with_opacity(fg_on(pal().shelf_hover), 0.7)
                            } else {
                                with_opacity(pal().ink, 0.6)
                            },
                            true,
                            false,
                        );
                        // 上行原文、下行辅助行（阶段 22 反转）
                        let text_pos = rect.left_top() + vec2(52.0, 5.0);
                        if is_active {
                            // faux-bold：双描近似 700（CJK 无粗体面）
                            painter.galley(
                                text_pos + vec2(0.45, 0.45),
                                galley.clone(),
                                Color32::TRANSPARENT,
                            );
                        }
                        painter.galley(text_pos, galley.clone(), Color32::TRANSPARENT);
                        if let Some(aux) = &aux_galley {
                            let aux_pos =
                                text_pos + vec2(0.0, galley.size().y + LYRIC_AUX_GAP);
                            painter.galley(aux_pos, aux.clone(), Color32::TRANSPARENT);
                        }
                        if response.clicked() {
                            clicked = Some(i);
                        }
                        if focus == Some(i) {
                            ui.scroll_to_rect(rect, Some(Align::Center));
                        }
                    }
                    ui.add_space(6.0);
                });
        });
    });

    clicked
}

// ---------------------------------------------------------------------------
// §3.6 EQ 条（code (3).html .eq）
// ---------------------------------------------------------------------------

/// EQ 条数量。**唯一事实源**在 [`crate::audio::spectrum::BANDS`]，
/// 这里只是给 UI 侧一个语义化别名。
pub const EQ_BARS: usize = crate::audio::spectrum::BANDS;

/// EQ 面板（黑底 acid 条），绘制在指定矩形内。
///
/// `levels` 是 [`crate::audio::spectrum::Bands`]——每段 `0.0..=1.0` 的实时电平。
/// 未播放时传全零即可（面板会自动落到 3px 底线），**不再有假波形**。
pub fn eq_at(ui: &mut Ui, rect: Rect, levels: &[f32; EQ_BARS], right: &str, lang: Lang) {
    // 频谱柱渐变：低端取 `spectrum_low`，满幅取主题主金。
    // 两套浅色主题里二者相等，渐变退化为原来的单色；深色主题才有金棕→金光过渡。
    fn mix_rgb(a: Color32, b: Color32, t: f32) -> Color32 {
        let t = t.clamp(0.0, 1.0);
        let ch = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
        Color32::from_rgb(ch(a.r(), b.r()), ch(a.g(), b.g()), ch(a.b(), b.b()))
    }
    {
        let painter = ui.painter();
        painter.rect_filled(rect, radius_none(), pal().monitor_bg);
        painter.rect_stroke(
            rect,
            radius_none(),
            Stroke::new(1.0, pal().hub_dark),
            StrokeKind::Inside,
        );
    }
    let clip = ui.clip_rect().intersect(rect);
    ui.scope_builder(UiBuilder::new().max_rect(rect.shrink(12.0)), |ui| {
        ui.set_clip_rect(clip);
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        let head = ui
            .allocate_exact_size(vec2(ui.available_width(), 12.0), Sense::hover())
            .0;
        paint_spec_text(
            ui,
            head.left_center(),
            Align2::LEFT_CENTER,
            pick(lang, "▣ 实时频谱", "▣ SPECTRUM"),
            FONT_TINY,
            TRACKING_TINY,
            pal().acid,
            false,
            true,
        );
        paint_spec_text(
            ui,
            head.right_center(),
            Align2::RIGHT_CENTER,
            right,
            FONT_TINY,
            TRACKING_TINY,
            pal().acid,
            true,
            false,
        );
        ui.add_space(12.0);

        let (bars, _) = ui.allocate_exact_size(vec2(ui.available_width(), 60.0), Sense::hover());
        let painter = ui.painter();
        let gap = 4.0;
        let bar_w = (bars.width() - gap * (EQ_BARS as f32 - 1.0)) / EQ_BARS as f32;
        for (i, level) in levels.iter().enumerate().take(EQ_BARS) {
            let lv = level.clamp(0.0, 1.0);
            let h = (lv * (bars.height() - 4.0)).max(3.0);
            let x = bars.min.x + i as f32 * (bar_w + gap);
            let bar = Rect::from_min_max(pos2(x, bars.max.y - h), pos2(x + bar_w, bars.max.y));
            // 电平越高越亮：静音时是暗金棕，满幅是主题主金
            let alpha = 0.35 + 0.55 * lv;
            let color = mix_rgb(pal().spectrum_low, pal().acid, lv);
            painter.rect_filled(bar, radius_none(), with_opacity(color, alpha));
        }
        // 底线 #73766b
        painter.line_segment(
            [bars.left_bottom(), bars.right_bottom()],
            Stroke::new(1.0, pal().well_rail),
        );
        ui.add_space(7.0);
        let (labels, _) = ui.allocate_exact_size(vec2(ui.available_width(), 10.0), Sense::hover());
        for (text, anchor, x) in [
            ("60Hz", Align2::LEFT_TOP, labels.min.x),
            (
                "250",
                Align2::LEFT_TOP,
                labels.min.x + labels.width() * 0.25,
            ),
            ("1k", Align2::CENTER_TOP, labels.center().x),
            ("4k", Align2::LEFT_TOP, labels.min.x + labels.width() * 0.75),
            ("16kHz", Align2::RIGHT_TOP, labels.max.x),
        ] {
            paint_spec_text(
                ui,
                pos2(x, labels.min.y),
                anchor,
                text,
                8.0,
                0.08,
                with_opacity(pal().acid, 0.8),
                true,
                false,
            );
        }
    });
}

// ---------------------------------------------------------------------------
// 检索结果行 / 热榜行 / 歌单卡（code (3).html .res / .hot-row / .pl-card）
// ---------------------------------------------------------------------------

/// 检索结果行动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAction {
    None,
    /// 点击行：仅选中（不换曲播放）。
    Select,
    /// 点击行内 ▶：立即播放。
    Play,
    /// 双击行：立即播放（与 ▶ 同义，符合桌面列表惯例）。
    PlayNow,
    /// 点击行内红心：切换收藏。
    ToggleFavorite,
    /// 右键点击行：请求打开上下文菜单。
    ///
    /// 用动作变体而不是额外返回值：`res_row` 的调用点有七八处，改签名会连带
    /// 改一遍所有调用方；右键本质就是"这一行被另一种方式点了"。
    ContextMenu,
}

/// 检索结果行（列 28px / 1.25fr / 1.25fr / 26px / 52px / 30px，gap 9）。
pub struct ResRow<'a> {
    pub index: usize,
    pub title: &'a str,
    pub artist: &'a str,
    pub time: &'a str,
    /// 当前播放曲目（蓝底反白）。
    pub current: bool,
    /// 当前选中行（浅蓝底 + 左侧蓝条，与"正在播放"区分）。
    pub selected: bool,
    /// 是否已收藏（红心实心）。
    pub favorite: bool,
    /// 高亮关键词（空串 = 不高亮）。
    pub highlight: &'a str,
}

/// 检索结果行的固定行高。
///
/// 行高**必须**是常量：列表用 [`egui::ScrollArea::show_rows`] 做虚拟化，
/// 它按固定行高反推可视区间；行高与实际绘制高度不一致会让行错位或跳动。
pub const RES_ROW_HEIGHT: f32 = 34.0;

/// 绘制检索结果行。
pub fn res_row(ui: &mut Ui, id: egui::Id, view: &ResRow) -> RowAction {
    let width = ui.available_width();
    let gap = 9.0;
    // 28 序号 + 26 红心 + 52 时长 + 30 ▶ + 5 个 gap
    let fixed = 28.0 + 26.0 + 52.0 + 30.0 + gap * 5.0;
    let fr = ((width - fixed) / 2.5).max(40.0);
    let (rect, _) = ui.allocate_exact_size(vec2(width, RES_ROW_HEIGHT), Sense::click());
    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    let response = ui.interact(rect, id, Sense::click());
    let play_rect = Rect::from_min_size(
        pos2(rect.max.x - 30.0, rect.center().y - 11.0),
        vec2(30.0, 22.0),
    );
    let play_resp = ui.interact(play_rect, id.with("play"), Sense::click());
    let fav_rect = Rect::from_min_size(
        pos2(rect.max.x - 30.0 - gap - 26.0, rect.center().y - 11.0),
        vec2(26.0, 22.0),
    );
    let fav_resp = ui.interact(fav_rect, id.with("fav"), Sense::click());
    let painter = ui.painter();

    let hovered = response.hovered() && !view.current;
    if view.current {
        painter.rect_filled(rect, radius_none(), pal().blue);
    } else if view.selected {
        painter.rect_filled(rect, radius_none(), with_opacity(pal().blue, 0.10));
        painter.rect_filled(
            Rect::from_min_size(rect.min, vec2(3.0, rect.height())),
            radius_none(),
            pal().blue,
        );
    } else if hovered {
        painter.rect_filled(rect, radius_none(), pal().shelf_hover);
    }
    painter.line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0, with_opacity(pal().ink, 0.24)),
    );

    // 悬停底色是浅金光，白字会糊掉：整行前景跟着底色走
    let fg = if view.current {
        fg_on(pal().blue)
    } else if hovered {
        fg_on(pal().shelf_hover)
    } else {
        pal().ink
    };
    paint_spec_text(
        ui,
        rect.left_center() + vec2(8.0, 0.0),
        Align2::LEFT_CENTER,
        &format!("{:02}", view.index + 1),
        FONT_TINY,
        0.0,
        if view.current {
            with_opacity(fg_on(pal().blue), 0.7)
        } else if hovered {
            with_opacity(fg_on(pal().shelf_hover), 0.65)
        } else {
            with_opacity(pal().ink, 0.55)
        },
        true,
        false,
    );
    let title_x = rect.min.x + 28.0 + gap;
    let title_job = if view.highlight.trim().is_empty() {
        spec_job(view.title, 10.5, 0.01, fg, true)
    } else {
        highlight_job(
            view.title,
            view.highlight,
            10.5,
            0.01,
            fg,
            if view.current {
                with_opacity(fg_on(pal().blue), 0.28)
            } else {
                with_opacity(pal().blue, 0.18)
            },
            true,
        )
    };
    let title_job = truncate_to(title_job, (fr - gap).max(24.0));
    paint_job(
        ui,
        pos2(title_x, rect.center().y),
        Align2::LEFT_CENTER,
        title_job,
        false,
    );
    let artist_x = title_x + fr + gap;
    let artist_job = truncate_to(
        spec_job(
            view.artist,
            FONT_TINY,
            0.08,
            if view.current {
                with_opacity(fg_on(pal().blue), 0.7)
            } else if hovered {
                with_opacity(fg_on(pal().shelf_hover), 0.7)
            } else {
                with_opacity(pal().ink, 0.7)
            },
            true,
        ),
        (fr - gap).max(24.0),
    );
    paint_job(
        ui,
        pos2(artist_x, rect.center().y),
        Align2::LEFT_CENTER,
        artist_job,
        false,
    );
    let time_x = artist_x + fr + gap;
    paint_spec_text(
        ui,
        pos2(time_x, rect.center().y),
        Align2::LEFT_CENTER,
        view.time,
        FONT_TINY,
        0.0,
        if view.current {
            with_opacity(fg_on(pal().blue), 0.7)
        } else if hovered {
            with_opacity(fg_on(pal().shelf_hover), 0.7)
        } else {
            with_opacity(pal().ink, 0.7)
        },
        true,
        false,
    );
    // 行内红心（26px 列）：已收藏用 favorite 实心，未收藏用低透明度同字形。
    // 当前行/悬停行的底色本身就是亮金，金色爱心会消失，改用底色反色。
    paint_spec_text(
        ui,
        fav_rect.center(),
        Align2::CENTER_CENTER,
        super::icons::HEART,
        12.0,
        0.0,
        if view.favorite {
            if view.current {
                fg_on(pal().blue)
            } else if hovered {
                fg_on(pal().shelf_hover)
            } else {
                pal().favorite
            }
        } else if view.current {
            with_opacity(fg_on(pal().blue), 0.45)
        } else if hovered {
            with_opacity(fg_on(pal().shelf_hover), 0.45)
        } else {
            with_opacity(pal().mid, 0.7)
        },
        false,
        false,
    );
    // 行内 ▶（30px 列）
    paint_spec_text(
        ui,
        play_rect.center(),
        Align2::CENTER_CENTER,
        "▶",
        11.0,
        0.0,
        fg,
        false,
        true,
    );

    if fav_resp.clicked() {
        RowAction::ToggleFavorite
    } else if play_resp.clicked() {
        RowAction::Play
    } else if response.secondary_clicked() {
        // 右键优先级最高：它不该顺带触发选中/播放
        RowAction::ContextMenu
    } else if response.double_clicked() {
        // 双击 = 播放；单击只选中（P0-1：避免误触打断正在播放的曲目）
        RowAction::PlayNow
    } else if response.clicked() {
        RowAction::Select
    } else {
        RowAction::None
    }
}

/// 涨跌颜色（涨红 #C62828 / 跌绿 #1B7F3B / 平 MID）。
pub fn rise_color(rise: i32) -> Color32 {
    match rise {
        r if r > 0 => pal().hot_up,
        r if r < 0 => pal().hot_down,
        _ => pal().mid,
    }
}

/// 涨跌文本（`▲ +3` / `▼ −2` / `—`）。
pub fn rise_text(rise: i32) -> String {
    match rise {
        r if r > 0 => format!("▲ +{}", r),
        r if r < 0 => format!("▼ −{}", -r),
        _ => "—".to_string(),
    }
}

/// 热榜行视图。
pub struct HotRow<'a> {
    pub rank: usize,
    /// 名次变化（正=上升）。
    pub rise: i32,
    pub title: &'a str,
    pub artist: &'a str,
    /// 热度 0..=1（热度条）。
    pub heat: f32,
    /// 播放量文本（真实统计）。
    pub plays: &'a str,
    pub current: bool,
    /// 单击选中（与「正在播放」是两种不同状态）。
    pub selected: bool,
    /// 该榜单正在载入：行尾显示忙碌标记并拒绝重复触发。
    pub loading: bool,
}

/// 热榜列几何（相对行左边界）。
///
/// 表头与数据行**必须同源**计算：早期表头用 `head.max.x - 220` 硬编码，
/// 与 `hot_row` 的 fr 分配不同源，实测 rec 页的 `Update` 表头直接压在热度条上。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HotColumns {
    /// 名次列左边界。
    pub rank_x: f32,
    /// 标题列左边界。
    pub title_x: f32,
    /// 标题列可用宽度。
    pub title_w: f32,
    /// 副标题（更新信息）列左边界。
    pub artist_x: f32,
    /// 副标题列可用宽度。
    pub artist_w: f32,
    /// 热度条左边界。
    pub meter_x: f32,
    /// 播放量列左边界。
    pub plays_x: f32,
}

/// 列宽常量：40 rank / 1.3fr title / 1.05fr artist / 104 meter / 46 plays / 28 play。
const HOT_RANK_W: f32 = 40.0;
/// 热度条列宽。
const HOT_METER_W: f32 = 104.0;
/// 播放量列宽。
const HOT_PLAYS_W: f32 = 46.0;
/// 行尾播放按钮列宽。
const HOT_PLAY_W: f32 = 28.0;
/// 列间距。
const HOT_GAP: f32 = 9.0;
/// 单行高度（滚动区估算内容高度也用它）。
pub const HOT_ROW_HEIGHT: f32 = 42.0;

/// 计算 [`hot_row`] 的列几何（纯函数，可测）。
pub fn hot_columns(width: f32) -> HotColumns {
    let fixed = HOT_RANK_W + HOT_METER_W + HOT_PLAYS_W + HOT_PLAY_W + HOT_GAP * 5.0;
    let fr_total = (width - fixed).max(80.0);
    let title_w = fr_total * (1.3 / 2.35);
    let artist_w = fr_total * (1.05 / 2.35);
    let title_x = HOT_RANK_W + HOT_GAP;
    let artist_x = title_x + title_w + HOT_GAP;
    let meter_x = artist_x + artist_w + HOT_GAP;
    HotColumns {
        rank_x: 8.0,
        title_x,
        title_w,
        artist_x,
        artist_w,
        meter_x,
        plays_x: meter_x + HOT_METER_W + HOT_GAP,
    }
}

/// 热榜行（列 40px / 1.3fr / 1.05fr / 104px / 46px / 28px，gap 9）。
pub fn hot_row(ui: &mut Ui, id: egui::Id, view: &HotRow) -> RowAction {
    let width = ui.available_width();
    let cols = hot_columns(width);
    let gap = HOT_GAP;
    let (rect, _) = ui.allocate_exact_size(vec2(width, HOT_ROW_HEIGHT), Sense::click());
    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    let response = ui.interact(rect, id, Sense::click());
    let play_rect = Rect::from_min_size(
        pos2(rect.max.x - 28.0, rect.center().y - 11.0),
        vec2(28.0, 22.0),
    );
    let play_resp = ui.interact(play_rect, id.with("play"), Sense::click());
    let painter = ui.painter();

    let hovered = response.hovered() && !view.current;
    if view.current {
        painter.rect_filled(rect, radius_none(), pal().blue);
    } else if view.selected {
        painter.rect_filled(rect, radius_none(), with_opacity(pal().blue, 0.10));
        painter.rect_filled(
            Rect::from_min_size(rect.min, vec2(3.0, rect.height())),
            radius_none(),
            pal().blue,
        );
    } else if hovered {
        painter.rect_filled(rect, radius_none(), pal().shelf_hover);
    }

    let fg = if view.current {
        fg_on(pal().blue)
    } else if hovered {
        fg_on(pal().shelf_hover)
    } else {
        pal().ink
    };
    // 名次 + 涨跌
    paint_spec_text(
        ui,
        rect.left_center() + vec2(cols.rank_x, -6.0),
        Align2::LEFT_CENTER,
        &format!("{:02}", view.rank),
        17.0,
        -0.05,
        fg,
        true,
        false,
    );
    paint_spec_text(
        ui,
        rect.left_center() + vec2(cols.rank_x, 12.0),
        Align2::LEFT_CENTER,
        &rise_text(view.rise),
        8.0,
        0.06,
        if view.current {
            with_opacity(fg_on(pal().blue), 0.85)
        } else {
            rise_color(view.rise)
        },
        true,
        false,
    );

    let title_x = rect.min.x + cols.title_x;
    let title_job = truncate_to(
        spec_job(view.title, 10.5, 0.01, fg, true),
        (cols.title_w - gap).max(24.0),
    );
    paint_job(
        ui,
        pos2(title_x, rect.center().y),
        Align2::LEFT_CENTER,
        title_job,
        false,
    );
    let artist_x = rect.min.x + cols.artist_x;
    let artist_job = truncate_to(
        spec_job(
            view.artist,
            FONT_TINY,
            0.08,
            if view.current {
                with_opacity(fg_on(pal().blue), 0.7)
            } else if hovered {
                with_opacity(fg_on(pal().shelf_hover), 0.7)
            } else {
                with_opacity(pal().ink, 0.7)
            },
            true,
        ),
        (cols.artist_w - gap).max(24.0),
    );
    paint_job(
        ui,
        pos2(artist_x, rect.center().y),
        Align2::LEFT_CENTER,
        artist_job,
        false,
    );

    // 热度条（104px 列）
    let meter_x = rect.min.x + cols.meter_x;
    let meter = Rect::from_min_size(pos2(meter_x, rect.center().y - 4.0), vec2(HOT_METER_W, 8.0));
    painter.rect_filled(
        meter,
        radius_none(),
        if view.current {
            with_opacity(Color32::WHITE, 0.25)
        } else {
            pal().panel_alt
        },
    );
    painter.rect_stroke(meter, radius_none(), stroke_hairline(), StrokeKind::Inside);
    let fill_w = HOT_METER_W * view.heat.clamp(0.0, 1.0);
    if fill_w > 0.5 {
        painter.rect_filled(
            Rect::from_min_size(meter.min, vec2(fill_w, 8.0)),
            radius_none(),
            if view.current {
                pal().selected_mark
            } else {
                pal().blue
            },
        );
    }

    let plays_x = rect.min.x + cols.plays_x;
    paint_spec_text(
        ui,
        pos2(plays_x, rect.center().y),
        Align2::LEFT_CENTER,
        view.plays,
        FONT_TINY,
        0.06,
        if view.current {
            with_opacity(fg_on(pal().blue), 0.75)
        } else if hovered {
            with_opacity(fg_on(pal().shelf_hover), 0.75)
        } else {
            with_opacity(pal().ink, 0.75)
        },
        true,
        false,
    );
    paint_spec_text(
        ui,
        play_rect.center(),
        Align2::CENTER_CENTER,
        if view.loading { "…" } else { "▶" },
        11.0,
        0.0,
        if view.loading {
            with_opacity(fg, 0.5)
        } else {
            fg
        },
        false,
        true,
    );

    // 载入中该行整体不再响应，避免连点叠加请求
    if view.loading {
        return RowAction::None;
    }
    if play_resp.clicked() {
        RowAction::Play
    } else if response.secondary_clicked() {
        RowAction::ContextMenu
    } else if response.double_clicked() {
        RowAction::PlayNow
    } else if response.clicked() {
        RowAction::Select
    } else {
        RowAction::None
    }
}

/// 歌单卡动作（单击打开 / 右键菜单 / hover 删除图标）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistCardAction {
    None,
    /// 单击卡片：打开/载入。
    Open,
    /// 右键卡片：打开上下文菜单。
    ContextMenu,
    /// 点击 hover 时浮出的 ✕（仅 `deletable` 卡片）。
    Delete,
}

impl PlaylistCardAction {
    /// 兼容旧调用点的 `clicked()` 语义。
    pub fn clicked(self) -> bool {
        self == Self::Open
    }

    /// 兼容旧调用点的 `secondary_clicked()` 语义。
    pub fn secondary_clicked(self) -> bool {
        self == Self::ContextMenu
    }
}

/// 歌单卡（左侧 accent 条 + 名称/副行 + 右侧 56×56 封面位）。
///
/// 三态：常态 / `loading`（灰化 + 忙碌点，拒绝重复触发）/ `deletable`
/// （hover 时右上角浮出 ✕，点击回传 [`PlaylistCardAction::Delete`]）。
pub fn playlist_card(
    ui: &mut Ui,
    id: egui::Id,
    accent: Color32,
    name: &str,
    sub: &str,
    cover: Option<TextureId>,
    selected: bool,
    loading: bool,
    deletable: bool,
) -> PlaylistCardAction {
    let width = ui.available_width();
    // .pl-card：padding 11 + 封面 56 → 78 高
    let (rect, _) = ui.allocate_exact_size(vec2(width, 78.0), Sense::click());
    if ui.rect_contains_pointer(rect) {
        ui.ctx().set_cursor_icon(if loading {
            egui::CursorIcon::Progress
        } else {
            egui::CursorIcon::PointingHand
        });
    }
    let response = ui.interact(rect, id, Sense::click());
    // 选中 translateX(-3) / hover translateX(-2)，按下再叠 2px 位移
    let dx = if selected {
        -3.0
    } else if response.hovered() {
        -2.0
    } else {
        0.0
    };
    let dy = if response.is_pointer_button_down_on() {
        2.0
    } else {
        0.0
    };
    let rect = rect.translate(vec2(dx, dy));
    let painter = ui.painter();
    painter.add(shadow_shelf().as_shape(rect, radius_none()));
    painter.rect(
        rect,
        radius_none(),
        if selected { pal().blue } else { pal().panel },
        stroke_component(),
        StrokeKind::Inside,
    );
    // accent 条：贴边、全高（HTML grid 第一列 9px，无内缩）
    painter.rect_filled(
        Rect::from_min_size(rect.min, vec2(9.0, rect.height())),
        radius_none(),
        if selected {
            pal().selected_mark
        } else {
            accent
        },
    );
    let fg = if selected {
        fg_on(pal().blue)
    } else {
        pal().ink
    };
    let text_w = (rect.width() - 20.0 - 68.0).max(40.0);
    let name_job = truncate_to(spec_job(name, 12.0, 0.01, fg, true), text_w);
    paint_job(
        ui,
        rect.left_top() + vec2(20.0, 15.0),
        Align2::LEFT_TOP,
        name_job,
        true,
    );
    let sub_job = truncate_to(
        spec_job(
            sub,
            8.0,
            0.14,
            if selected {
                with_opacity(fg_on(pal().blue), 0.72)
            } else {
                with_opacity(pal().ink, 0.65)
            },
            true,
        ),
        text_w,
    );
    paint_job(
        ui,
        rect.left_top() + vec2(20.0, 37.0),
        Align2::LEFT_TOP,
        sub_job,
        false,
    );
    // 封面位（56×56）
    let cover_rect = Rect::from_min_size(
        pos2(rect.max.x - 64.0, rect.center().y - 28.0),
        vec2(56.0, 56.0),
    );
    if let Some(texture) = cover {
        painter.image(
            texture,
            cover_rect,
            // 同上：cover 语义居中裁切（容器当前是正方形，与纹理一致时等于整幅）
            cover_uv(Vec2::splat(1.0), cover_rect.size()),
            // 纹理乘色：必须是纯白（tint 语义），不随主题变化
            Color32::WHITE,
        );
    } else {
        painter.rect_filled(
            cover_rect,
            radius_none(),
            if selected {
                with_opacity(Color32::WHITE, 0.18)
            } else {
                pal().mid
            },
        );
    }
    painter.rect_stroke(
        cover_rect,
        radius_none(),
        stroke_component(),
        StrokeKind::Inside,
    );

    // 载入中：整卡灰化 + 三个忙碌点，期间不响应点击（防连点叠加请求）
    if loading {
        painter.rect_filled(rect, radius_none(), with_opacity(pal().paper, 0.62));
        let phase = (ui.input(|i| i.time) * 6.0) as usize % 3;
        for i in 0..3 {
            let cx = rect.center().x + (i as f32 - 1.0) * 12.0;
            painter.rect_filled(
                Rect::from_center_size(pos2(cx, rect.center().y), vec2(6.0, 6.0)),
                radius_none(),
                with_opacity(pal().ink, if i == phase { 0.9 } else { 0.25 }),
            );
        }
    }

    // hover 删除图标（可发现性：不必猜"右键")
    let del_rect = Rect::from_min_size(pos2(rect.max.x - 24.0, rect.min.y + 4.0), vec2(18.0, 18.0));
    let del_response = if deletable && !loading && response.hovered() {
        painter.rect_filled(del_rect, radius_none(), pal().paper);
        painter.rect_stroke(
            del_rect,
            radius_none(),
            Stroke::new(1.5, pal().error_mark),
            StrokeKind::Inside,
        );
        paint_spec_text(
            ui,
            del_rect.center(),
            Align2::CENTER_CENTER,
            super::icons::CLOSE,
            11.0,
            0.0,
            pal().error_mark,
            false,
            false,
        );
        Some(ui.interact(del_rect, id.with("delete"), Sense::click()))
    } else {
        None
    };

    if loading {
        return PlaylistCardAction::None;
    }
    if del_response.map(|r| r.clicked()).unwrap_or(false) {
        PlaylistCardAction::Delete
    } else if response.secondary_clicked() {
        PlaylistCardAction::ContextMenu
    } else if response.clicked() {
        PlaylistCardAction::Open
    } else {
        PlaylistCardAction::None
    }
}

// ---------------------------------------------------------------------------
// §3.5b 磁带舱（code (3).html .deck-well / .deck-door）
// ---------------------------------------------------------------------------

/// 舱体视图数据。
pub struct DeckView<'a> {
    pub cassette: CassetteView<'a>,
    /// 舱盖开度 0（合）..=1（开 -66°）。
    pub open: f32,
    /// 装带进度 0（未入舱）..=1（落座）。
    pub insert: f32,
    /// 落座轻震 y 偏移（px，[`super::deck_seq::seat_kick`]）。
    pub seat_dy: f32,
    /// 合舱咔哒 y 偏移（px，[`super::deck_seq::clunk_kick`]，作用于舱体）。
    pub well_dy: f32,
}

/// 舱盖投影高度比：rotateX(-66°) 的余弦投影（cos 66° ≈ 0.407）。
pub fn door_height_ratio(open: f32) -> f32 {
    let open = open.clamp(0.0, 1.0);
    1.0 - 0.593 * open
}

/// 舱盖玻璃底色不透明度（合盖 0.10 → 全开 0.20）。
///
/// 合盖时玻璃正对镜头，透光最多（舱内磁带要能看清）；越接近侧视
/// 视线穿过玻璃的路径越长、叠层越厚，所以不透明度随开度线性上升。
/// 上限压到 0.20，保证**任何开度下磁带都透得出来**——这正是"透明玻璃"
/// 与"半透明白板"的区别。
pub fn door_glass_opacity(open: f32) -> f32 {
    0.10 + 0.10 * open.clamp(0.0, 1.0)
}

/// 舱盖玻璃底色（乳白薄层，靠下面的描边与高光立起"玻璃"语义）。
pub fn door_glass_tint(open: f32) -> Color32 {
    with_opacity(Color32::WHITE, door_glass_opacity(open))
}

/// 在给定矩形内绘制磁带舱：腔体 / 导轨 / 走带口 / 卡带（装带动画）/ 舱盖（开合）。
pub fn deck_at(ui: &Ui, rect: Rect, view: &DeckView) {
    let painter = ui.painter();
    // 合舱咔哒：舱体整体 y 偏移（HTML .deck.clunk .deck-well）
    let rect = rect.translate(vec2(0.0, view.well_dy));

    // 腔体（inset -2px + 内阴影环）
    let cavity = rect.expand(2.0);
    painter.rect_filled(cavity, radius_none(), pal().well_cavity);
    painter.rect(
        cavity,
        radius_none(),
        Color32::TRANSPARENT,
        stroke_structural(),
        StrokeKind::Inside,
    );
    painter.rect_stroke(
        cavity.shrink(3.0),
        radius_none(),
        Stroke::new(2.0, pal().hub_dark),
        StrokeKind::Inside,
    );

    // 导轨（左右 7%，top/bottom 8%，3px）
    let rail_w = rect.width() * 0.86;
    for (y, alpha) in [
        (rect.min.y + rect.height() * 0.08, 1.0),
        (rect.max.y - rect.height() * 0.08 - 3.0, 1.0),
    ] {
        let rail =
            Rect::from_min_size(pos2(rect.min.x + rect.width() * 0.07, y), vec2(rail_w, 3.0));
        painter.rect_filled(rail, radius_none(), pal().well_rail);
        painter.rect(
            rail,
            radius_none(),
            Color32::TRANSPARENT,
            Stroke::new(1.0, pal().slot_black),
            StrokeKind::Inside,
        );
        let _ = alpha;
    }

    // 走带口（左右 24%，bottom 3.5%，8px）
    let slot = Rect::from_min_size(
        pos2(
            rect.min.x + rect.width() * 0.24,
            rect.max.y - rect.height() * 0.035 - 8.0,
        ),
        vec2(rect.width() * 0.52, 8.0),
    );
    painter.rect_filled(slot, radius_none(), pal().slot_black);
    painter.rect(
        slot,
        radius_none(),
        Color32::TRANSPARENT,
        Stroke::new(1.0, Color32::BLACK),
        StrokeKind::Inside,
    );

    // 卡带（装带动画：insert 0→1 落座，透明度 .22→1；落座时 seat 轻震）
    let insert = view.insert.clamp(0.0, 1.0);
    if insert > 0.0 {
        let lift = (1.0 - insert) * rect.height() * 0.10;
        // 0.90 而不是 0.96：舱盖改成透明玻璃后，磁带与舱壁之间必须留出一圈
        // 可见的玻璃边（含描边），否则玻璃会被磁带完全盖住、看不出是"盖板"。
        let cass_rect = Rect::from_center_size(
            rect.center() + vec2(0.0, -lift + view.seat_dy),
            vec2(
                rect.width() * CASSETTE_FILL,
                rect.width() * CASSETTE_FILL / CASSETTE_RATIO,
            ),
        );
        let alpha = (0.22 + 0.78 * insert).clamp(0.0, 1.0);
        cassette_at(ui, cass_rect, &view.cassette);
        if alpha < 0.99 {
            // 透明度近似：未落座时叠暗罩（egui 无整体 alpha 形状组）
            painter.rect_filled(
                cass_rect,
                radius_none(),
                Color32::from_black_alpha(((1.0 - alpha) * 150.0) as u8),
            );
        }
    }

    // 舱盖（顶铰链，开合近似：高度收缩到投影比）
    //
    // 玻璃语义 = 「几乎全透的薄层 + 窄而亮的高光 + 清晰的双层描边」。
    // 早期版本用整块 3px 偏移硬投影 + 30% 白斜带，看起来是一块**不透明的白板**
    // 压在磁带上；现在把投影收成一条 2px 厚度暗带（只暗示门有厚度），
    // 本体降到 10–20% 乳白，磁带在任何开度下都透得出来。
    let open = view.open.clamp(0.0, 1.0);
    let door_h = rect.height() * door_height_ratio(open);
    let door = Rect::from_min_size(rect.min, vec2(rect.width(), door_h));

    // 门厚度：右/下各 2px 的暗带（不是整块偏移投影）
    painter.rect_filled(
        Rect::from_min_max(
            pos2(door.min.x + 2.0, door.max.y),
            pos2(door.max.x + 2.0, door.max.y + 2.0),
        ),
        radius_none(),
        with_opacity(Color32::BLACK, 0.22),
    );
    painter.rect_filled(
        Rect::from_min_max(
            pos2(door.max.x, door.min.y + 2.0),
            pos2(door.max.x + 2.0, door.max.y + 2.0),
        ),
        radius_none(),
        with_opacity(Color32::BLACK, 0.16),
    );

    // 玻璃本体（乳白薄层）
    painter.rect_filled(door, radius_none(), door_glass_tint(open));

    // 斜向高光：主光带 10% 宽 + 侧反光 + 细边光（比实心板时代更窄更淡）
    for (from, to, alpha) in [
        (0.10_f32, 0.155, 0.20_f32),
        (0.16, 0.30, 0.05),
        (0.305, 0.345, 0.13),
    ] {
        painter.rect_filled(
            Rect::from_min_max(
                pos2(door.min.x + door.width() * from, door.min.y),
                pos2(door.min.x + door.width() * to, door.max.y),
            ),
            radius_none(),
            with_opacity(Color32::WHITE, alpha),
        );
    }

    // 顶铰链处的玻璃厚度反光（3px，最亮的一条）
    painter.rect_filled(
        Rect::from_min_max(door.min, pos2(door.max.x, door.min.y + 3.0)),
        radius_none(),
        with_opacity(Color32::WHITE, 0.26),
    );

    // 描边（本轮要求的重点）：外亮内暗的双层玻璃边
    // - 2px 亮边在框内 → 玻璃折射出的高光边
    // - 1px 发丝暗边在框外 → 与舱腔分离，透明之后仍能读出边界
    painter.rect_stroke(
        door,
        radius_none(),
        Stroke::new(STROKE_STRUCTURAL, with_opacity(Color32::WHITE, 0.62)),
        StrokeKind::Inside,
    );
    painter.rect_stroke(
        door.expand(1.0),
        radius_none(),
        Stroke::new(STROKE_HAIRLINE, with_opacity(pal().hub_dark, 0.45)),
        StrokeKind::Outside,
    );
    // 门内 6px 内框（双层玻璃的内片边）
    painter.rect_stroke(
        door.shrink(6.0),
        radius_none(),
        Stroke::new(STROKE_HAIRLINE, with_opacity(Color32::WHITE, 0.24)),
        StrokeKind::Inside,
    );
    // 两条横向反光线（玻璃内侧的反射条纹）
    for frac in [0.26_f32, 0.74] {
        let y = door.min.y + door.height() * frac;
        painter.line_segment(
            [pos2(door.min.x + 9.0, y), pos2(door.max.x - 9.0, y)],
            Stroke::new(STROKE_HAIRLINE, with_opacity(Color32::WHITE, 0.10)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::music::provider::ProviderError;

    #[test]
    fn cover_uv_crops_the_long_side_and_never_stretches() {
        // 正方形纹理铺进 1:2 的竖长容器 → 只能裁左右，上下必须满幅
        let uv = cover_uv(Vec2::new(1.0, 1.0), Vec2::new(50.0, 100.0));
        assert!((uv.min.y - 0.0).abs() < 1e-6, "上下不该被裁：{uv:?}");
        assert!((uv.max.y - 1.0).abs() < 1e-6, "上下不该被裁：{uv:?}");
        assert!((uv.min.x - 0.25).abs() < 1e-6, "左右应各裁 1/4：{uv:?}");
        assert!((uv.max.x - 0.75).abs() < 1e-6, "左右应各裁 1/4：{uv:?}");

        // 正方形纹理铺进 2:1 的横长容器 → 只能裁上下
        let uv = cover_uv(Vec2::new(1.0, 1.0), Vec2::new(200.0, 100.0));
        assert!((uv.min.x - 0.0).abs() < 1e-6, "左右不该被裁：{uv:?}");
        assert!((uv.max.x - 1.0).abs() < 1e-6, "左右不该被裁：{uv:?}");
        assert!((uv.min.y - 0.25).abs() < 1e-6, "上下应各裁 1/4：{uv:?}");
        assert!((uv.max.y - 0.75).abs() < 1e-6, "上下应各裁 1/4：{uv:?}");
    }

    #[test]
    fn cover_uv_keeps_full_frame_when_aspects_match() {
        let uv = cover_uv(Vec2::new(1.0, 1.0), Vec2::new(56.0, 56.0));
        assert!((uv.min.x).abs() < 1e-6 && (uv.min.y).abs() < 1e-6);
        assert!((uv.max.x - 1.0).abs() < 1e-6 && (uv.max.y - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cover_uv_survives_degenerate_sizes() {
        // 容器 0 高（布局塌缩）时不能除零、不能产生 NaN
        let uv = cover_uv(Vec2::new(1.0, 1.0), Vec2::new(10.0, 0.0));
        assert!(uv.min.x.is_finite() && uv.max.y.is_finite(), "{uv:?}");
    }

    #[test]
    fn wrapped_spec_job_keeps_long_cjk_message_inside_width() {
        // 回归：确认框正文曾用单行 job 直绘，长歌单名会直接冲出纸面右边缘。
        let ctx = egui::Context::default();
        crate::gui::theme::setup_fonts(&ctx);
        let text =
            "「【东方vocal】自选神曲篇」将从列表隐藏，直到下次刷新账号歌单。云端数据不受影响。";
        let mut dims = egui::Vec2::ZERO;
        let mut rows = 0;
        let out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let job = wrapped_spec_job(text, FONT_SPINE_NAME, 0.0, pal().ink, false, 300.0);
            let galley = ui.fonts_mut(|f| f.layout_job(job));
            dims = galley.size();
            rows = galley.rows.len();
        });
        out.drop_without_applying_deltas();
        assert!(dims.x <= 300.5, "折行后仍超宽：{} > 300", dims.x);
        assert!(rows >= 2, "长文案没有被折成多行");
        assert!(dims.y > FONT_SPINE_NAME * 1.5, "多行高度异常：{}", dims.y);
    }

    #[test]
    fn hot_columns_are_monotonic_and_fit() {
        // 表头与 hot_row 同源：列必须严格从左到右、互不重叠，且尾部留出播放按钮
        for width in [420.0_f32, 700.0, 1100.0] {
            let c = hot_columns(width);
            assert!(c.rank_x < c.title_x, "w={width}");
            assert!(c.title_x + c.title_w <= c.artist_x + 1e-3, "w={width}");
            assert!(c.artist_x + c.artist_w <= c.meter_x + 1e-3, "w={width}");
            assert!(c.meter_x + HOT_METER_W <= c.plays_x + 1e-3, "w={width}");
            assert!(
                c.plays_x + HOT_PLAYS_W <= width - HOT_PLAY_W + 1e-3,
                "播放量列吃掉了播放按钮：w={width}"
            );
        }
    }

    #[test]
    fn tracking_px_converts_em_to_pixels() {
        // §1.2：window-title +0.12em @13px = 1.56px
        assert!((tracking_px(TRACKING_WINDOW_TITLE, FONT_WINDOW_TITLE) - 1.56).abs() < 1e-6);
        // label +0.16em @11px = 1.76px
        assert!((tracking_px(TRACKING_LABEL, FONT_LABEL) - 1.76).abs() < 1e-6);
    }

    #[test]
    fn load_dots_cycles_four_phases() {
        assert_eq!(load_dots(0), "");
        assert_eq!(load_dots(12), "·");
        assert_eq!(load_dots(24), "··");
        assert_eq!(load_dots(36), "···");
        assert_eq!(load_dots(48), "");
    }

    #[test]
    fn badge_view_colors_follow_state() {
        let idle: LoadState<u32> = LoadState::Idle;
        assert_eq!(badge_view(&idle, 0, Lang::Zh).1, pal().mid);

        let loading = LoadState::<u32>::Loading;
        let (text, color) = badge_view(&loading, 12, Lang::Zh);
        assert_eq!(color, pal().blue);
        assert!(text.contains('·'), "加载中应带省略点: {}", text);

        let ready = LoadState::Ready(1u32);
        assert_eq!(badge_view(&ready, 0, Lang::Zh).1, pal().status_ok);

        let failed = LoadState::<u32>::Failed(ProviderError::RequestTimeout);
        assert_eq!(badge_view(&failed, 0, Lang::Zh).1, pal().error_mark);
    }

    #[test]
    fn eq_bars_follow_spectrum_band_count() {
        // EQ 柱数必须与音频侧频带数同源，否则频带数据会被静默截断/浪费
        assert_eq!(EQ_BARS, crate::audio::spectrum::BANDS);
        assert!(EQ_BARS > 0);
    }

    #[test]
    fn hash_id_is_stable_and_distinguishing() {
        assert_eq!(hash_id("186016"), hash_id("186016"));
        assert_ne!(hash_id("186016"), hash_id("186017"));
    }

    #[test]
    fn spec_job_inserts_per_char_spacing() {
        // §9：字距经逐字 leading_space；n 个字符 → n 个 section
        let job = spec_job("Reimu Player", FONT_LABEL, TRACKING_LABEL, pal().ink, false);
        assert_eq!(job.sections.len(), "Reimu Player".chars().count());
        // 首字符无前距，其余每字带 spacing
        assert_eq!(job.sections[0].leading_space, 0.0);
        let expected = tracking_px(TRACKING_LABEL, FONT_LABEL);
        assert!((job.sections[1].leading_space - expected).abs() < 1e-6);
    }

    #[test]
    fn with_opacity_premultiplies_rgb_and_sets_alpha() {
        let ink = pal().ink;
        let c = with_opacity(ink, 0.62);
        assert_eq!(c.a(), SPINE_ARTIST_ALPHA);
        // 预乘：RGB 必须按 alpha 缩放，否则 egui 的 ONE/ONE_MINUS_SRC_ALPHA
        // 混合会把"低 alpha"当成"全亮叠加"，白色直接饱和成纯白。
        // 逐通道比较：主题的 ink 不一定是中性灰（灵梦档就是深红黑），不能用 r 推 g/b
        let scale = |v: u8| ((v as u16 * SPINE_ARTIST_ALPHA as u16 + 127) / 255) as u8;
        assert_eq!(c.r(), scale(ink.r()));
        assert_eq!(c.g(), scale(ink.g()));
        assert_eq!(c.b(), scale(ink.b()));
    }

    #[test]
    fn with_opacity_keeps_white_translucent_instead_of_saturating() {
        // 回归：玻璃盖板曾因未预乘渲染成一块白板
        let glass = with_opacity(Color32::WHITE, 0.10);
        assert_eq!(glass.a(), 26);
        assert!(
            glass.r() < 40,
            "10% 白必须预乘成低亮度（实际 {}），否则盖板会盖住磁带",
            glass.r()
        );
        // 全透明/全不透明是两端恒等
        assert_eq!(with_opacity(Color32::WHITE, 0.0).a(), 0);
        assert_eq!(with_opacity(Color32::WHITE, 1.0), Color32::WHITE);
    }

    #[test]
    fn truncate_and_wrap_preserve_text() {
        let job = truncate_to(
            spec_job("PLASTIC LOVE", FONT_SPINE_NAME, 0.0, pal().ink, false),
            60.0,
        );
        assert_eq!(job.text, "PLASTIC LOVE");
        assert_eq!(job.wrap.max_width, 60.0);
        assert_eq!(job.wrap.overflow_character, Some('…'));

        let job = wrap_to(spec_job("body", FONT_TINY, 0.0, pal().mid, false), 80.0, 3);
        assert_eq!(job.wrap.max_rows, 3);
    }

    #[test]
    fn wave_height_is_deterministic_and_bounded() {
        for i in 0..WAVE_BARS {
            let h = wave_height(i);
            assert!((10.0..=98.0).contains(&h), "条高越界: {}", h);
            assert_eq!(h, wave_height(i), "同序号必须同高");
        }
    }

    #[test]
    fn rise_color_follows_market_convention() {
        // 涨红跌绿（A 股习惯）
        assert_eq!(rise_color(3), pal().hot_up);
        assert_eq!(rise_color(-2), pal().hot_down);
        assert_eq!(rise_color(0), pal().mid);
    }

    #[test]
    fn rise_text_formats_arrows() {
        assert_eq!(rise_text(3), "▲ +3");
        assert_eq!(rise_text(-2), "▼ −2");
        assert_eq!(rise_text(0), "—");
    }

    #[test]
    fn door_height_ratio_projects_rotation() {
        assert!((door_height_ratio(0.0) - 1.0).abs() < 1e-6);
        assert!(
            (door_height_ratio(1.0) - 0.407).abs() < 1e-3,
            "cos 66° ≈ 0.407"
        );
        assert!(door_height_ratio(0.5) > door_height_ratio(1.0));
        assert!(door_height_ratio(-3.0) >= door_height_ratio(1.0));
    }

    #[test]
    fn door_glass_stays_transparent_at_every_angle() {
        // 「透明玻璃」的硬约束：任何开度下都要能看清舱内磁带，
        // 所以不透明度必须远低于 50%，且随开度单调上升（侧视叠层更厚）。
        assert!((door_glass_opacity(0.0) - 0.10).abs() < 1e-6);
        assert!((door_glass_opacity(1.0) - 0.20).abs() < 1e-6);
        assert!(door_glass_opacity(0.5) > door_glass_opacity(0.0));
        assert!(door_glass_opacity(0.5) < door_glass_opacity(1.0));
        for i in 0..=10 {
            let a = door_glass_opacity(i as f32 / 10.0);
            assert!(a < 0.5, "开度 {} 的玻璃不透明度 {} 会挡住磁带", i, a);
        }
        // 越界开度被夹紧，不产生负透明度
        assert!((door_glass_opacity(-1.0) - 0.10).abs() < 1e-6);
        assert!((door_glass_opacity(3.0) - 0.20).abs() < 1e-6);
    }

    #[test]
    fn door_glass_tint_is_a_premultiplied_white_veil() {
        let c = door_glass_tint(0.0);
        assert_eq!(c.a(), 26, "合盖时 10% 白");
        // 预乘后 RGB = alpha（纯白 × 10%），不是 255——否则盖板会盖住磁带
        assert_eq!((c.r(), c.g(), c.b()), (26, 26, 26));
        assert!(c.a() < 128, "玻璃必须始终是薄层");
    }

    // ---- 控制键共轴布局 -------------------------------------------------

    fn specs_5() -> (ControlSpec, ControlSpec) {
        (
            ControlSpec {
                glyph: "x",
                primary: false,
                toggled: false,
                active: false,
            },
            ControlSpec {
                glyph: "x",
                primary: true,
                toggled: false,
                active: true,
            },
        )
    }

    #[test]
    fn primary_key_is_acid_only_while_active() {
        // 用户要求：播放中主键才是绿色强调色，暂停时与背景同色
        let (normal, primary) = specs_5();
        assert_eq!(control_fill(&normal), pal().paper);
        assert_eq!(control_fill(&primary), pal().acid);
        let paused = ControlSpec {
            active: false,
            ..primary
        };
        assert_eq!(control_fill(&paused), pal().paper, "暂停时主键不该是 acid");
    }

    #[test]
    fn mode_keys_use_the_same_acid_highlight_without_sinking() {
        // 模式切换键（顺序/随机、列表/单曲循环）：高亮与播放主键同色，
        // 但不能走 `toggled` 的下沉位移——它是"当前是哪一态"的标注，不是"被按住"。
        let mode_key = ControlSpec {
            glyph: "x",
            primary: false,
            toggled: false,
            active: true,
        };
        assert_eq!(
            control_fill(&mode_key),
            pal().acid,
            "活动态必须与播放键同款高亮"
        );
        assert!(
            control_size(&mode_key).y
                < control_size(&ControlSpec {
                    primary: true,
                    ..mode_key
                })
                .y,
            "模式键仍是普通键尺寸，不因高亮而变形"
        );
    }

    #[test]
    fn control_row_centers_share_one_horizontal_axis() {
        // 回归：主键 42px 与普通键 34px 混排时若顶边对齐，主键中心会低 4px。
        let (n, p) = specs_5();
        let row = Rect::from_min_size(pos2(100.0, 50.0), vec2(400.0, 44.0));
        let centers = control_row_layout(row, &[&n, &n, &p, &n, &n], 8.0);
        let y0 = centers[0].y;
        for (i, c) in centers.iter().enumerate() {
            assert!(
                (c.y - y0).abs() < 1e-6,
                "第 {i} 个键的中心 y={} 与首个键 {y0} 不同轴",
                c.y
            );
        }
    }

    #[test]
    fn control_row_visual_centres_coincide_despite_different_heights() {
        // 硬投影只往下偏 2px，两种高度的键视觉中心都应落在同一水平线
        let (n, p) = specs_5();
        let row = Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 44.0));
        let centers = control_row_layout(row, &[&n, &p], 8.0);
        let visual_center = |spec: &ControlSpec, cy: f32| {
            let h = control_size(spec).y;
            let top = cy - h / 2.0;
            let bottom = top + h + 2.0; // 硬投影
            (top + bottom) / 2.0
        };
        let a = visual_center(&n, centers[0].y);
        let b = visual_center(&p, centers[1].y);
        assert!((a - b).abs() < 1e-6, "普通键视觉中心 {a} != 主键 {b}");
    }

    #[test]
    fn control_row_is_horizontally_centred() {
        let (n, p) = specs_5();
        let row = Rect::from_min_size(pos2(100.0, 0.0), vec2(400.0, 44.0));
        let specs: [&ControlSpec; 5] = [&n, &n, &p, &n, &n];
        let centers = control_row_layout(row, &specs, 8.0);
        // 组左缘 / 右缘（含末键 2px 投影）
        let left = centers[0].x - control_size(&n).x / 2.0;
        let right = centers[4].x + control_size(&n).x / 2.0 + 2.0;
        let group_center = (left + right) / 2.0;
        assert!(
            (group_center - row.center().x).abs() <= 1.0,
            "组中心 {group_center} 偏离行中心 {}（>1px）",
            row.center().x
        );
    }

    #[test]
    fn control_row_gaps_are_uniform() {
        let (n, p) = specs_5();
        let row = Rect::from_min_size(pos2(0.0, 0.0), vec2(500.0, 44.0));
        let specs: [&ControlSpec; 5] = [&n, &n, &p, &n, &n];
        let centers = control_row_layout(row, &specs, 8.0);
        let mut gaps = Vec::new();
        for i in 0..4 {
            let a = centers[i].x + control_size(&specs[i]).x / 2.0;
            let b = centers[i + 1].x - control_size(&specs[i + 1]).x / 2.0;
            gaps.push(b - a);
        }
        for (i, g) in gaps.iter().enumerate() {
            assert!((g - 8.0).abs() < 1e-6, "第 {i} 个间隙 {g} != 8");
        }
    }

    // ---- 歌词区尺寸稳定 ------------------------------------------------

    /// 在给定行数下渲染一次歌词面板，返回它占用的高度。
    fn lyric_panel_height(rows: &[LyricRow<'_>], body_h: f32) -> f32 {
        let ctx = egui::Context::default();
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, vec2(600.0, 600.0)));
        let mut used = 0.0;
        let out = ctx.run_ui(raw, |ui| {
            ui.set_width(400.0);
            let top = ui.cursor().min.y;
            lyric_panel(
                ui,
                Lang::Zh,
                egui::Id::unique("lyrics-test"),
                rows,
                None,
                None,
                body_h,
            );
            used = ui.cursor().min.y - top;
        });
        out.drop_without_applying_deltas();
        used
    }

    #[test]
    fn lyric_panel_keeps_its_height_when_empty() {
        // 回归：`auto_shrink` 纵向为 true 时，空态（1 行占位）会让面板塌成一条，
        // 歌词载入后再弹开——既是占位变化，也是明显的布局跳动。
        let empty = [LyricRow {
            time_text: "",
            text: "装载后显示歌词",
            aux: None,
        }];
        let loaded: Vec<LyricRow> = (0..6)
            .map(|i| LyricRow {
                time_text: "00:0",
                text: if i % 2 == 0 {
                    "a line of lyrics"
                } else {
                    "另一行歌词"
                },
                aux: None,
            })
            .collect();

        let body_h = 200.0;
        let h_empty = lyric_panel_height(&empty, body_h);
        let h_loaded = lyric_panel_height(&loaded, body_h);
        assert!(
            (h_empty - h_loaded).abs() < 1.0,
            "空态高 {h_empty} 与有歌词高 {h_loaded} 不一致（差 {}）",
            (h_empty - h_loaded).abs()
        );
        // 高度还必须真的吃满 body（含 24px 头条）
        assert!(
            h_empty >= body_h + 24.0 - 1.0,
            "面板高 {h_empty} 未撑满 body_h={body_h} + 头条 24"
        );
    }

    #[test]
    fn aux_line_only_makes_the_row_taller_when_it_exists() {
        // 关闭态是回归基线：`aux_h = 0` 必须与加辅助行之前的公式逐像素相同
        assert_eq!(lyric_row_height(0.0, 13.0), 24.0, "13 + 10 被 24 兜住");
        assert_eq!(lyric_row_height(0.0, 20.0), 30.0, "20 + 10，无辅助行加成");
        // 有辅助行：多出"辅助行高 + 行距"，且严格更高
        let with_aux = lyric_row_height(11.0, 13.0);
        assert_eq!(with_aux, 11.0 + LYRIC_AUX_GAP + 13.0 + 10.0);
        assert!(with_aux > lyric_row_height(0.0, 13.0));
        // 空文本的辅助行由调用方过滤成 `aux_h = 0`，这里再兜一层
        assert_eq!(lyric_row_height(0.0, 5.0), 24.0, "矮行也不许低于 24px");
    }

    // ---- 标题走马灯 -----------------------------------------------------

    #[test]
    fn marquee_stays_put_when_text_fits() {
        // 放得下就一个像素都不许动（否则短标题会自己抖）
        for t in [0.0, 0.5, 3.0, 17.0] {
            assert_eq!(marquee_offset(200.0, 200.0, t), 0.0);
            assert_eq!(marquee_offset(120.0, 200.0, t), 0.0);
        }
    }

    #[test]
    fn marquee_starts_with_a_hold_then_moves_left() {
        let overflow = 100.0;
        let (text_w, view_w) = (300.0, 200.0);
        assert_eq!(marquee_offset(text_w, view_w, 0.0), 0.0);
        // 停留期内不动
        assert_eq!(marquee_offset(text_w, view_w, MARQUEE_HOLD * 0.5), 0.0);
        // 停留结束后开始匀速左移
        let moved = marquee_offset(text_w, view_w, MARQUEE_HOLD + 1.0);
        assert!(
            (moved - MARQUEE_SPEED).abs() < 1e-3,
            "停留 1 秒后应左移 {MARQUEE_SPEED}px，实际 {moved}"
        );
        assert!(moved > 0.0 && moved < overflow);
    }

    #[test]
    fn marquee_never_exceeds_overflow_and_returns() {
        let (text_w, view_w) = (300.0, 200.0);
        let overflow = text_w - view_w;
        let travel = overflow / MARQUEE_SPEED;
        let cycle = 2.0 * (MARQUEE_HOLD + travel);
        // 扫两个完整周期，位移必须恒在 [0, overflow] 内
        let mut max_seen = 0.0_f32;
        let mut steps = 0;
        let mut t = 0.0;
        while t < cycle * 2.0 {
            let off = marquee_offset(text_w, view_w, t);
            assert!(
                (-1e-3..=overflow + 1e-3).contains(&off),
                "t={t} 位移 {off} 越界 (overflow={overflow})"
            );
            max_seen = max_seen.max(off);
            steps += 1;
            t += 0.05;
        }
        assert!(steps > 100);
        // 必须真的走到末端（否则"滚动"只是意思意思）
        assert!(
            (max_seen - overflow).abs() < 1.0,
            "最大位移 {max_seen} 未到达末端 {overflow}"
        );
        // 周期末尾回到原点，循环不跳字
        assert!(marquee_offset(text_w, view_w, cycle) < 0.05);
    }
    /// 把 `LayoutJob` 还原成「每个字符是否高亮」。
    ///
    /// 不能直接数 `sections`：egui 的 `LayoutJob::append` 会把**格式与字距都相同**
    /// 的相邻段合并（`leading_space == 0.0` 时逐字追加必然合并），所以段数不等于
    /// 字符数。按 `byte_range` 展开才是稳定判据。
    fn hot_chars(job: &LayoutJob, text: &str) -> Vec<bool> {
        let mut out = Vec::new();
        for section in &job.sections {
            let hot = section.format.background != Color32::TRANSPARENT;
            let range = section.byte_range.start.0..section.byte_range.end.0;
            let slice = &text[range];
            for _ in slice.chars() {
                out.push(hot);
            }
        }
        out
    }

    #[test]
    fn highlight_marks_substring_case_insensitively() {
        // "起风" 命中 "起风了"：前两字带底色，其余不带
        let job = highlight_job("起风了", "起风", 10.5, 0.01, pal().ink, pal().blue, true);
        assert_eq!(hot_chars(&job, "起风了"), vec![true, true, false]);
        // 大小写不敏感
        let job = highlight_job("City Pop", "city", 10.0, 0.0, pal().ink, pal().blue, true);
        let hot = hot_chars(&job, "City Pop");
        assert_eq!(
            hot.iter().filter(|h| **h).count(),
            4,
            "CITY 四个字母都应高亮"
        );
        assert_eq!(&hot[..4], &[true, true, true, true]);
    }

    #[test]
    fn highlight_falls_back_to_subsequence() {
        // "cp" 不是 "City Pop" 的子串，但 c→p 顺序出现，应逐字高亮
        let job = highlight_job("City Pop", "cp", 10.0, 0.0, pal().ink, pal().blue, true);
        let hot = hot_chars(&job, "City Pop");
        assert_eq!(hot.iter().filter(|h| **h).count(), 2);
        assert!(hot[0], "C 应高亮");
        assert!(hot[5], "P 应高亮");
    }

    #[test]
    fn highlight_without_match_or_query_is_plain() {
        for query in ["", "   ", "zzz"] {
            let job = highlight_job("City Pop", query, 10.0, 0.0, pal().ink, pal().blue, true);
            assert!(
                job.sections
                    .iter()
                    .all(|s| s.format.background == Color32::TRANSPARENT),
                "query={query:?} 不该有高亮"
            );
        }
    }

    #[test]
    fn highlight_preserves_character_count_and_tracking() {
        // 逐字 append 且字距只在首字之后——高亮不应改变排版
        let job = highlight_job("abc", "b", 10.0, 0.1, pal().ink, pal().blue, true);
        assert_eq!(job.sections.len(), 3);
        assert_eq!(job.sections[0].leading_space, 0.0);
        assert!(job.sections[1].leading_space > 0.0);
        assert!(job.sections[2].leading_space > 0.0);
    }
}
