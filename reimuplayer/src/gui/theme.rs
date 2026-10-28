//! 设计令牌（`设计2.md` §1）的 Rust 定义处。
//!
//! 依据 §10「颜色一律引用令牌，**禁止在业务代码里写十六进制**」——本模块是 GUI
//! 侧唯一出现颜色字面量的地方；其余 GUI 文件一律经 [`pal()`] 取色。
//!
//! 颜色是 [`Palette`] 结构体的字段而不是 `const`，这样「换配色」才是运行时的事：
//! [`THEMES`] 列出可选方案（首次启动默认「博丽灵梦」，见 [`DEFAULT_THEME`]），
//! 设置面板改下标，下一帧全部控件跟着变。尺寸/字距/线宽仍是不变的 `const`——
//! 它们不属于"配色方案"。
//!
//! §9 映射到 egui 0.37：`Color32` 令牌表 + `Shadow { offset: [i8; 2], blur: u8,
//! spread: u8, color }`；硬投影 blur **必须为 0**（§1.4「硬投影是这个设计的骨相，
//! 模糊 = 失去风格」）。

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, Margin, Shadow, Stroke,
};
use std::sync::atomic::{AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// §1.1 色彩
// ---------------------------------------------------------------------------

/// 一整套配色令牌。
///
/// 为什么是结构体而不是一组 `pub const`：`const` 在编译期就钉死了，换配色只能
/// 重新编译。egui 是即时模式、界面每帧重画，所以取值只要走 [`pal()`]，切主题
/// 就是下一次重绘的事——不需要重启，也不需要给每个控件加失效逻辑。
///
/// 字段命名与原令牌一一对应（`PAPER` → `paper`），注释即原 §1.1/§3.x 的设计依据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// 窗口/页面底（窗口内最底层，铺满视口）。
    pub paper: Color32,
    /// 模块面板底（磁带架、歌单框、歌词区、频谱这类"卡片"）。
    ///
    /// 与 `paper` 分开之后，暗色档才有「页面底 → 面板 → 子面」的层级：
    /// 早苗/灵梦两档两者同值，观感不变。
    pub panel: Color32,
    /// 主文字、投影。
    pub ink: Color32,
    /// 硬投影色（窗口 / 按钮 / 选中项 / toast）。
    ///
    /// 亮色档等于 `ink`（黑投影）；暗色档必须另配近黑——反相后 `ink` 是近白，
    /// 继续拿它当投影会在深底上画出一圈白光。
    pub shadow: Color32,
    /// 结构描边、分隔线。
    pub line: Color32,
    /// 主强调色：仅「当前正在活动的东西」（选中项、进度、当前歌词）。
    pub blue: Color32,
    /// art 区（卡带展台）底色。
    ///
    /// 亮色档等于 `blue`（大面积强调块）；暗色档换成辅助藏青——主金铺满
    /// 整块展台会把金色用成背景色，反而压掉真正需要强调的选中行。
    pub art_bg: Color32,
    /// 点睛色：面积 ≤ 5%（主按键、贴纸、轮毂齿）。
    pub acid: Color32,
    /// 压在强调面上的标记色（选中项色条、导航字形、当前歌词左条）。
    ///
    /// 亮色档等于 `acid`；暗色档要换成深色——金色标记压在金色选中行上会消失。
    pub selected_mark: Color32,
    /// 次级金色装饰（卡带轮毂齿、小标记）。
    pub ornament: Color32,
    /// 印刷套印偏色、次要点缀（侧边标签、色块）。
    pub pink: Color32,
    /// 收藏红心实心色。
    ///
    /// 单独一档而不是借 `pink`：暗色档的 `pink` 是辅助藏青，而红心要跟主强调
    /// 金光走；亮色档仍等于 `pink`，红心观感不变。
    pub favorite: Color32,
    /// 金属件：螺丝、旋钮孔。
    pub silver: Color32,
    /// 次级图标、失活元素。
    pub mid: Color32,
    /// 桌面底（窗口外）。仅设计稿桌面底，原生窗口不使用（R8 已去网页外壳；
    /// 保留供设置页的预览块引用）。
    pub desktop: Color32,
    /// 底部 mini 面板底 / 凹陷子面（搜索条、滑杆槽、歌词头条、标签条纹）。
    pub panel_alt: Color32,
    /// 仪器面板底（实时频谱、搜索指引这类"黑底酸绿字"的机器窗）。
    ///
    /// 这些位置早期直接借用了 `ink`，于是反相主题里"黑底 LCD"变成了米白底，
    /// 酸绿条几乎看不见。仪器是机身的一部分，不跟纸面反相。
    pub monitor_bg: Color32,
    /// 列表项 hover 底。
    pub shelf_hover: Color32,
    /// 列表项硬投影。
    pub shelf_shadow: Color32,
    /// 卡带走带窗底。
    pub tape_black: Color32,
    /// 系统状态点（正常）。
    pub status_ok: Color32,
    /// 标题栏条纹（与 paper 交替）。
    pub titlebar_stripe: Color32,
    /// 错误模态强调（规范 8.2 错误用模态，不用 toast）。
    pub error_mark: Color32,
    /// 卡带壳硬投影 rgba(0,0,0,.32)（§1.4 投影梯度 tape 6px）。
    pub tape_shadow: Color32,
    /// 卡带标签纸亮条纹（也是文本框底色）。
    pub label_light: Color32,
    /// 带盘底。
    pub reel_bg: Color32,
    /// 带盘磁带环（亮）。
    pub reel_pack: Color32,
    /// 带盘磁带环（暗）。
    pub reel_pack_dark: Color32,
    /// 轮毂底。
    pub hub_dark: Color32,
    /// 走带槽。
    pub slot_black: Color32,
    /// 走带槽内框。
    pub slot_inner: Color32,
    /// 滑块槽（foot）。
    pub fslot: Color32,
    /// 滑杆把手。
    pub thumb: Color32,
    /// art 区白色描线（inset 1px rgba(255,255,255,.28)）。
    ///
    /// 注意 **RGB 也要预乘**：egui 的颜色是预乘 alpha，写成 `(255,255,255,71)`
    /// 会被渲染成接近纯白（蓝底上的白条纹因此一直"过曝"）。
    pub art_inset: Color32,
    /// art 区横条纹 rgba(255,255,255,.12)。
    pub art_stripe: Color32,
    /// spine-end 左分隔线 rgba(0,0,0,.16)。
    pub spine_end_line: Color32,
    /// spine-end 底 rgba(0,0,0,.035)。
    pub spine_end_bg: Color32,
    /// spine 选中态 end 底 rgba(255,255,255,.12)。
    pub spine_end_bg_sel: Color32,
    /// spine 选中态 end 分隔线 rgba(255,255,255,.3)。
    pub spine_end_line_sel: Color32,
    /// 模态背景压暗层。
    pub modal_dim: Color32,
    /// 波形条默认色。
    pub wave_bar: Color32,
    /// 频谱低电平柱色（随电平向 `acid` 渐变）。
    ///
    /// 亮色档等于 `acid`（整条恒定 acid，与旧观感一致）；暗色档单独配暗调
    /// 金棕，柱子从底部往峰值自然提亮。
    pub spectrum_low: Color32,
    /// 热榜上涨（红）。
    pub hot_up: Color32,
    /// 热榜下跌（绿）。
    pub hot_down: Color32,
    /// 磁带舱腔体底。
    pub well_cavity: Color32,
    /// 舱体导轨/深边。
    pub well_rail: Color32,
    /// 字影（`paint_job` 双描的底层色）。
    ///
    /// 这一层过去继承原色（黑），于是 24px 标题右下会拖出一道**纯黑**重影，
    /// 看起来像糊了一笔而不是"印刷加粗"。改成中性灰后，重影只加深笔画边缘，
    /// 不再抢主字的对比度。
    pub text_shadow: Color32,
}

/// 一个可选主题：设置面板里看得到的名字 + 它的配色表。
pub struct Theme {
    /// 方案名（直接显示在「设置 → 主题」）。
    pub name: &'static str,
    /// 配色表。
    pub palette: Palette,
}

/// 「东风谷早苗」——本设计最初落地的那套配色。
///
/// 名字来自这套色实际长出来的样子：纸白 + 油墨黑 + 一点青绿巫女色和酸绿点睛，
/// 干净、偏冷、带印刷感。
pub const SANA: Palette = Palette {
    paper: Color32::from_rgb(0xF2, 0xF0, 0xE7),
    panel: Color32::from_rgb(0xF2, 0xF0, 0xE7),
    ink: Color32::from_rgb(0x17, 0x17, 0x17),
    shadow: Color32::from_rgb(0x17, 0x17, 0x17),
    line: Color32::from_rgb(0x17, 0x17, 0x17),
    blue: Color32::from_rgb(0x17, 0x4D, 0xCE),
    art_bg: Color32::from_rgb(0x17, 0x4D, 0xCE),
    acid: Color32::from_rgb(0xD7, 0xFF, 0x35),
    selected_mark: Color32::from_rgb(0xD7, 0xFF, 0x35),
    ornament: Color32::from_rgb(0xD7, 0xFF, 0x35),
    pink: Color32::from_rgb(0xFF, 0x70, 0xB8),
    favorite: Color32::from_rgb(0xFF, 0x70, 0xB8),
    silver: Color32::from_rgb(0xC9, 0xC9, 0xC3),
    mid: Color32::from_rgb(0xA5, 0xA5, 0x9E),
    desktop: Color32::from_rgb(0xD8, 0xD7, 0xCF),
    panel_alt: Color32::from_rgb(0xE4, 0xE2, 0xD9),
    monitor_bg: Color32::from_rgb(0x17, 0x17, 0x17),
    shelf_hover: Color32::from_rgb(0xE9, 0xE7, 0xDD),
    shelf_shadow: Color32::from_rgb(0xC4, 0xC2, 0xB8),
    tape_black: Color32::from_rgb(0x19, 0x19, 0x19),
    status_ok: Color32::from_rgb(0x40, 0xA8, 0x48),
    titlebar_stripe: Color32::from_rgb(0xD4, 0xD2, 0xC8),
    error_mark: Color32::from_rgb(0xB0, 0x1B, 0x1B),
    tape_shadow: Color32::from_rgba_premultiplied(0, 0, 0, 82), // 255 × 0.32
    label_light: Color32::from_rgb(0xFB, 0xFA, 0xF4),
    reel_bg: Color32::from_rgb(0x0D, 0x0D, 0x0D),
    reel_pack: Color32::from_rgb(0x3C, 0x3C, 0x3C),
    reel_pack_dark: Color32::from_rgb(0x1C, 0x1C, 0x1C),
    hub_dark: Color32::from_rgb(0x11, 0x11, 0x11),
    slot_black: Color32::from_rgb(0x0B, 0x0B, 0x0B),
    slot_inner: Color32::from_rgb(0x4A, 0x4A, 0x4A),
    fslot: Color32::from_rgb(0xD9, 0xD7, 0xCD),
    thumb: Color32::from_rgb(0xF2, 0xF0, 0xE7),
    art_inset: Color32::from_rgba_premultiplied(71, 71, 71, 71), // 255 × 0.28
    art_stripe: Color32::from_rgba_premultiplied(31, 31, 31, 31), // 255 × 0.12
    spine_end_line: Color32::from_rgba_premultiplied(0, 0, 0, 41), // 255 × 0.16
    spine_end_bg: Color32::from_rgba_premultiplied(0, 0, 0, 9),  // 255 × 0.035
    spine_end_bg_sel: Color32::from_rgba_premultiplied(31, 31, 31, 31),
    spine_end_line_sel: Color32::from_rgba_premultiplied(76, 76, 76, 76),
    modal_dim: Color32::from_black_alpha(110),
    wave_bar: Color32::from_rgb(0x8D, 0x8D, 0x86),
    spectrum_low: Color32::from_rgb(0xD7, 0xFF, 0x35),
    hot_up: Color32::from_rgb(0xC6, 0x28, 0x28),
    hot_down: Color32::from_rgb(0x1B, 0x7F, 0x3B),
    well_cavity: Color32::from_rgb(0x23, 0x23, 0x20),
    well_rail: Color32::from_rgb(0x4C, 0x4C, 0x45),
    text_shadow: Color32::from_rgb(0x9C, 0x9C, 0x95),
};

/// 「博丽灵梦」——同一套骨相换一层巫女色：纸面偏暖，强调走朱红，点睛用金。
///
/// 只动"带颜色倾向"的令牌（纸/墨/强调/点缀/状态），结构性的深件（带盘、走带槽、
/// 轮毂）保持原值——卡带是黑色塑料做的，跟界面配色无关。
pub const REIMU: Palette = Palette {
    paper: Color32::from_rgb(0xF6, 0xF1, 0xEC),
    panel: Color32::from_rgb(0xF6, 0xF1, 0xEC),
    ink: Color32::from_rgb(0x1A, 0x14, 0x14),
    shadow: Color32::from_rgb(0x1A, 0x14, 0x14),
    line: Color32::from_rgb(0x1A, 0x14, 0x14),
    blue: Color32::from_rgb(0xC7, 0x3E, 0x3A),
    art_bg: Color32::from_rgb(0xC7, 0x3E, 0x3A),
    acid: Color32::from_rgb(0xFF, 0xB1, 0x1B),
    selected_mark: Color32::from_rgb(0xFF, 0xB1, 0x1B),
    ornament: Color32::from_rgb(0xFF, 0xB1, 0x1B),
    pink: Color32::from_rgb(0xE8, 0x55, 0x6B),
    favorite: Color32::from_rgb(0xE8, 0x55, 0x6B),
    silver: Color32::from_rgb(0xCF, 0xC7, 0xBF),
    mid: Color32::from_rgb(0xA9, 0x9E, 0x96),
    desktop: Color32::from_rgb(0xE3, 0xDA, 0xD2),
    panel_alt: Color32::from_rgb(0xED, 0xE4, 0xDC),
    monitor_bg: Color32::from_rgb(0x1A, 0x14, 0x14),
    shelf_hover: Color32::from_rgb(0xF1, 0xE7, 0xDF),
    shelf_shadow: Color32::from_rgb(0xCF, 0xC2, 0xB8),
    tape_black: Color32::from_rgb(0x1C, 0x16, 0x16),
    status_ok: Color32::from_rgb(0x3F, 0x9E, 0x4D),
    titlebar_stripe: Color32::from_rgb(0xE0, 0xD5, 0xCC),
    error_mark: Color32::from_rgb(0xA8, 0x1E, 0x1E),
    label_light: Color32::from_rgb(0xFD, 0xFA, 0xF5),
    fslot: Color32::from_rgb(0xE2, 0xD8, 0xCF),
    thumb: Color32::from_rgb(0xF6, 0xF1, 0xEC),
    wave_bar: Color32::from_rgb(0x9A, 0x8F, 0x88),
    spectrum_low: Color32::from_rgb(0xFF, 0xB1, 0x1B),
    hot_up: Color32::from_rgb(0xC6, 0x28, 0x28),
    hot_down: Color32::from_rgb(0x1B, 0x7F, 0x3B),
    well_cavity: Color32::from_rgb(0x24, 0x1E, 0x1C),
    well_rail: Color32::from_rgb(0x4E, 0x46, 0x42),
    text_shadow: Color32::from_rgb(0xA2, 0x97, 0x8F),
    ..SANA
};

/// 「雾雨魔理沙」——暗色档：带藏青调的炭黑纸面 + 星光金强调 + 藏青辅色。
///
/// 色表来自用户给定的一套交接值（页面 / 面板 / 描边 / 主金 / hover / 暗金 /
/// 辅助藏青 / 正文 / 次要文字 / 频谱底 / 频谱低柱），逐项落到令牌：
///
/// - 页面底 `#1b1c25` → [`Palette::paper`]；模块面板 `#272933` → [`Palette::panel`]。
///   两者分开后才有"页面 → 面板 → 凹陷子面"三级层次。
/// - 分割线、窗口边框、模块轮廓 `#393b49` → [`Palette::line`]（描边只经
///   [`stroke_structural`] 一族取色，所以整个界面的边框会一起从纯白褪成灰蓝）。
/// - 星光金 `#e6c978` → [`Palette::blue`]（选中行 / 进度 / 当前歌词）与
///   [`Palette::acid`]（播放键 / 贴纸 / 频谱峰值）。金色是亮面，压在上面的字
///   必须走 [`fg_on`] 现挑深色，不能沿用 `ink`。
/// - hover `#f0d892` → [`Palette::shelf_hover`]；同样要求 hover 态文字改用
///   [`fg_on`]，否则近白字压在浅金上几乎看不见。
/// - 按下 / 次要点缀 `#a88e44` → [`Palette::ornament`]（卡带轮毂齿）。
/// - 辅助藏青 `#343b54` → [`Palette::pink`]（列表色条 / 歌单卡点缀）以及
///   [`Palette::selected_mark`]（金底上的标记条）、[`Palette::art_bg`]（展台底）、
///   [`Palette::panel_alt`]（凹陷子面）、[`Palette::well_rail`]（舱体导轨）。
/// - 正文 `#ececf0` → [`Palette::ink`]；次要文字 `#989fb3` → [`Palette::mid`]。
/// - 频谱底 `#14151a` → [`Palette::monitor_bg`]；低柱 `#7a6736` →
///   [`Palette::spectrum_low`]（向峰值 `acid` 平滑过渡）。
///
/// 反相不是把两个值对调就完事：
/// - 投影必须压到近黑（[`Palette::shadow`]/[`Palette::shelf_shadow`]）：
///   反相后 `ink` 是近白，继续拿它当硬投影会画出一圈白光；
/// - `label_light` 是**文本框底**，跟着墨色走的话字就看不见了，必须一起压暗；
/// - `spine_end_*` 的选中态改成黑压金（白压金会把尾栏洗成一块脏色）；
/// - 红心单独走 [`Palette::favorite`]：辅助藏青当红心会失去"喜欢"的语义。
pub const MARISA: Palette = Palette {
    paper: Color32::from_rgb(0x1B, 0x1C, 0x25),
    panel: Color32::from_rgb(0x27, 0x29, 0x33),
    ink: Color32::from_rgb(0xEC, 0xEC, 0xF0),
    shadow: Color32::from_rgb(0x0D, 0x0E, 0x14),
    line: Color32::from_rgb(0x39, 0x3B, 0x49),
    blue: Color32::from_rgb(0xE6, 0xC9, 0x78),
    art_bg: Color32::from_rgb(0x34, 0x3B, 0x54),
    acid: Color32::from_rgb(0xE6, 0xC9, 0x78),
    selected_mark: Color32::from_rgb(0x34, 0x3B, 0x54),
    ornament: Color32::from_rgb(0xA8, 0x8E, 0x44),
    pink: Color32::from_rgb(0x34, 0x3B, 0x54),
    favorite: Color32::from_rgb(0xE6, 0xC9, 0x78),
    silver: Color32::from_rgb(0x98, 0x9F, 0xB3),
    mid: Color32::from_rgb(0x98, 0x9F, 0xB3),
    desktop: Color32::from_rgb(0x14, 0x15, 0x1A),
    panel_alt: Color32::from_rgb(0x34, 0x3B, 0x54),
    monitor_bg: Color32::from_rgb(0x14, 0x15, 0x1A),
    shelf_hover: Color32::from_rgb(0xF0, 0xD8, 0x92),
    shelf_shadow: Color32::from_rgb(0x0D, 0x0E, 0x14),
    tape_black: Color32::from_rgb(0x12, 0x13, 0x1A),
    status_ok: Color32::from_rgb(0x5F, 0xD0, 0x6A),
    // 标题栏 1px 条纹：取藏青压暗一档，1/3 占空比下不抢标题。
    titlebar_stripe: Color32::from_rgb(0x2B, 0x2D, 0x3A),
    error_mark: Color32::from_rgb(0xE4, 0x62, 0x3F),
    label_light: Color32::from_rgb(0x27, 0x29, 0x33),
    reel_bg: Color32::from_rgb(0x0F, 0x10, 0x14),
    reel_pack: Color32::from_rgb(0x26, 0x28, 0x33),
    reel_pack_dark: Color32::from_rgb(0x17, 0x18, 0x21),
    hub_dark: Color32::from_rgb(0x0B, 0x0C, 0x11),
    slot_black: Color32::from_rgb(0x0A, 0x0B, 0x0F),
    slot_inner: Color32::from_rgb(0x34, 0x3B, 0x54),
    fslot: Color32::from_rgb(0x1B, 0x1C, 0x25),
    thumb: Color32::from_rgb(0xE6, 0xC9, 0x78),
    // 展台底纹：浅色主题里 28%/35% 白压蓝是"蓝图格"，压到藏青上会把整个展台
    // 洗成灰蓝、亮度反超金色强调。暗色档降到 28%/16%，保留格线但退到背景。
    art_inset: Color32::from_rgba_premultiplied(72, 72, 72, 72),
    art_stripe: Color32::from_rgba_premultiplied(40, 40, 40, 40),
    // 未选中尾栏是白压黑（提亮 4% + 一条 10% 分隔线）；选中尾栏是黑压金
    // （压暗 12% + 25% 分隔线）——金底上再叠白会把尾栏洗成脏色。
    spine_end_line: Color32::from_rgba_unmultiplied_const(235, 235, 225, 26),
    spine_end_bg: Color32::from_rgba_unmultiplied_const(235, 235, 225, 10),
    spine_end_bg_sel: Color32::from_rgba_premultiplied(0, 0, 0, 31),
    spine_end_line_sel: Color32::from_rgba_premultiplied(0, 0, 0, 64),
    wave_bar: Color32::from_rgb(0x7A, 0x67, 0x36),
    spectrum_low: Color32::from_rgb(0x7A, 0x67, 0x36),
    hot_up: Color32::from_rgb(0xEF, 0x53, 0x50),
    hot_down: Color32::from_rgb(0x4C, 0xC5, 0x7A),
    well_cavity: Color32::from_rgb(0x14, 0x15, 0x1A),
    well_rail: Color32::from_rgb(0x34, 0x3B, 0x54),
    text_shadow: Color32::from_rgb(0x44, 0x46, 0x54),
    ..SANA
};

/// 首次启动的默认主题下标：1 =「博丽灵梦」。
///
/// 只决定**prefs 里没有 theme 键**时落在哪（首次运行、老配置）；用户在设置里
/// 选过主题后，磁盘上的值永远优先——它只是默认值，不是覆盖规则。
pub const DEFAULT_THEME: usize = 1;

/// 全部可选主题。**下标就是持久化值**：只许追加，不许重排或删除。
///
/// 2 号槽位是"暗色档"这一格的**连续身份**：换掉它的配色（守矢·夜 → 雾雨魔理沙）
/// 可以，挪动顺序不行——旧 prefs 存的 `2` 必须继续落在暗色上。
pub static THEMES: [Theme; 3] = [
    Theme {
        name: "东风谷早苗",
        palette: SANA,
    },
    Theme {
        name: "博丽灵梦",
        palette: REIMU,
    },
    Theme {
        name: "雾雨魔理沙",
        palette: MARISA,
    },
];

/// 当前主题下标（`AtomicUsize`：UI 线程写、绘制时读，没有比"一个下标"更小的共享面）。
///
/// 初值取 [`DEFAULT_THEME`] 而不是 0：主程序读完 prefs 才调到用户的选择，
/// 中间这一小段若按 0 画就会闪一帧早苗配色再跳回灵梦。
static ACTIVE_THEME: AtomicUsize = AtomicUsize::new(DEFAULT_THEME);

/// 把任意下标夹进 [`THEMES`] 的有效范围。
///
/// 单独抽出来有两个理由：越界值来自磁盘（别的机器 / 旧版本写来的 prefs），
/// 属于外部输入，必须在入口夹一次；而 `THEMES.len()` 会随新配色追加而变，
/// 夹取规则得跟着走，不能写死 3。
fn clamp_theme(index: usize) -> usize {
    index.min(THEMES.len() - 1)
}

/// 当前生效的配色表。所有绘制代码取色只经这里。
pub fn pal() -> &'static Palette {
    &THEMES[clamp_theme(ACTIVE_THEME.load(Ordering::Relaxed))].palette
}

/// 当前主题下标（设置面板回显 + 写 prefs）。
pub fn theme_index() -> usize {
    clamp_theme(ACTIVE_THEME.load(Ordering::Relaxed))
}

/// 切换主题；越界的下标夹回有效范围（prefs 里可能是别的机器写来的值）。
pub fn set_theme_index(index: usize) {
    ACTIVE_THEME.store(clamp_theme(index), Ordering::Relaxed);
}

/// 主题名字列表（设置面板直接铺出来）。
pub fn theme_names() -> Vec<&'static str> {
    THEMES.iter().map(|t| t.name).collect()
}

// ---------------------------------------------------------------------------
// 压在强调面上的前景色
// ---------------------------------------------------------------------------

/// sRGB 相对亮度（WCAG 定义，线性化后加权）。
fn luma(c: Color32) -> f32 {
    let f = |v: u8| {
        let s = f32::from(v) / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
}

/// WCAG 对比度（1.0 = 完全同色，21.0 = 黑白）。
fn contrast(a: Color32, b: Color32) -> f32 {
    let (hi, lo) = if luma(a) >= luma(b) { (a, b) } else { (b, a) };
    (luma(hi) + 0.05) / (luma(lo) + 0.05)
}

/// 压在 `bg` 上、当前主题里对比度更强的那个文字色（`ink` / `paper` 二选一）。
///
/// 选中行、强调面、确认按钮这类"底色是主题色"的地方不能写死 `Color32::WHITE`：
/// 早苗档的强调面是深蓝，白字没问题；夜档的强调面是亮蓝，白字只剩 2.4:1。
/// 与其为每个面再配一个"上面的字"令牌（组合数爆炸），不如从既有的两个文字令牌
/// 里现挑——任何新配色，包括只有深底的一套，都自动成立。
pub fn fg_on(bg: Color32) -> Color32 {
    fg_on_palette(pal(), bg)
}

/// [`fg_on`] 的纯函数版本（主题表逐项自检要用，见 `tests::fg_on_is_legible_on_every_accent_surface`）。
fn fg_on_palette(p: &Palette, bg: Color32) -> Color32 {
    if contrast(p.ink, bg) >= contrast(p.paper, bg) {
        p.ink
    } else {
        p.paper
    }
}

// ---------------------------------------------------------------------------
// §1.4 线宽（逻辑像素）
// ---------------------------------------------------------------------------

/// 结构描边（窗口、面板、按钮、卡带壳）。
pub const STROKE_STRUCTURAL: f32 = 2.0;
/// 面板/组件描边。
pub const STROKE_COMPONENT: f32 = 1.5;
/// 发丝线（面板内分隔）。
pub const STROKE_HAIRLINE: f32 = 1.0;

/// 结构描边（2px line）。
pub fn stroke_structural() -> Stroke {
    Stroke::new(STROKE_STRUCTURAL, pal().line)
}

/// 组件描边（1.5px line）。
pub fn stroke_component() -> Stroke {
    Stroke::new(STROKE_COMPONENT, pal().line)
}

/// 发丝线（1px line）。
pub fn stroke_hairline() -> Stroke {
    Stroke::new(STROKE_HAIRLINE, pal().line)
}

// ---------------------------------------------------------------------------
// §1.4 硬投影（blur 恒为 0，只许偏移）
// ---------------------------------------------------------------------------

/// 构造硬投影：**blur 恒为 0**（规范 §1.4 / §9 egui 映射）。
///
/// egui 0.37 的 `Shadow` 用 `[i8; 2]` 偏移、`u8` 的 blur/spread——
/// 设计梯度 8/6/4/3/2px 全部落在 i8 值域内。
pub fn hard_shadow(offset_x: i8, offset_y: i8, color: Color32) -> Shadow {
    Shadow {
        offset: [offset_x, offset_y],
        blur: 0,
        spread: 0,
        color,
    }
}

/// `shadow/window` 8px 8px #171717（暗色档为近黑，不是反相后的近白）。
pub fn shadow_window() -> Shadow {
    hard_shadow(8, 8, pal().shadow)
}

/// `shadow/button` 2px 2px #171717。
pub fn shadow_button() -> Shadow {
    hard_shadow(2, 2, pal().shadow)
}

/// `shadow/shelf` 2px 2px #C4C2B8（列表项）。
pub fn shadow_shelf() -> Shadow {
    hard_shadow(2, 2, pal().shelf_shadow)
}

/// `shadow/selected` 3px 3px #171717（选中列表项）。
pub fn shadow_selected() -> Shadow {
    hard_shadow(3, 3, pal().shadow)
}

/// `shadow/tape` 6px 6px rgba(0,0,0,.32)（卡带壳）。
pub fn shadow_tape() -> Shadow {
    hard_shadow(6, 6, pal().tape_shadow)
}

/// `shadow/toast` 3px 3px #171717（Toast，与 selected 同梯度）。
pub fn shadow_toast() -> Shadow {
    hard_shadow(3, 3, pal().shadow)
}

// ---------------------------------------------------------------------------
// §1.3 间距（基数 2px）
// ---------------------------------------------------------------------------

/// 面板内边距。
pub const PAD_PANEL: f32 = 14.0;
/// 面板之间间距。
pub const GAP_PANEL: f32 = 10.0;
/// 玩家容器内边距。
pub const PAD_WINDOW: f32 = 10.0;
/// 列表项之间间距。
pub const GAP_SPINE: f32 = 7.0;
/// 面板头与内容间距。
pub const GAP_PANEL_HEAD: f32 = 12.0;

/// `Frame` 内边距（面板 14px，egui 0.37 `Margin` 为 i8）。
pub fn margin_panel() -> Margin {
    Margin::same(PAD_PANEL as i8)
}

/// `Frame` 内边距（容器 10px）。
pub fn margin_window() -> Margin {
    Margin::same(PAD_WINDOW as i8)
}

// ---------------------------------------------------------------------------
// §1.2 字号（egui 用 point 近似逻辑像素）
// ---------------------------------------------------------------------------

/// `type/window-title` 13 / 800。
///
/// 阶段 22 整体上调约 10%：原表（12/10/9/24…）是照 1600×900 的设计稿定的，
/// 在 2K / 125% 的实际桌面上肉眼偏小，用户反馈"全局字体偏小"。
pub const FONT_WINDOW_TITLE: f32 = 13.0;
/// `type/label` 11 / 800。
pub const FONT_LABEL: f32 = 11.0;
/// `type/tiny` 10 / 500。
pub const FONT_TINY: f32 = 10.0;
/// `type/track-title` 26 / 900。
pub const FONT_TRACK_TITLE: f32 = 26.0;
/// `type/spine-name` 13 / 900。
pub const FONT_SPINE_NAME: f32 = 13.0;
/// `type/spine-artist` 9 / 500。
pub const FONT_SPINE_ARTIST: f32 = 9.0;

// §1.2 字距（em；px = em × 字号，§9 经 LayoutJob 逐字插间距实现）
/// `type/window-title` +0.12em。
pub const TRACKING_WINDOW_TITLE: f32 = 0.12;
/// `type/label` +0.16em。
pub const TRACKING_LABEL: f32 = 0.16;
/// `type/tiny` +0.08em。
pub const TRACKING_TINY: f32 = 0.08;
/// `type/track-title` −0.06em。
pub const TRACKING_TRACK_TITLE: f32 = -0.06;
/// `type/spine-name` +0.02em。
pub const TRACKING_SPINE_NAME: f32 = 0.02;
/// `type/spine-artist` +0.16em。
pub const TRACKING_SPINE_ARTIST: f32 = 0.16;
/// 卡带标签行 +0.18em（7 / 800）。
pub const TRACKING_CASS_LABEL: f32 = 0.18;
/// 卡带主标题 −0.03em（14 / 900）。
pub const TRACKING_CASS_TITLE: f32 = -0.03;
/// 卡带副行 +0.14em（7.5）。
pub const TRACKING_CASS_ARTIST: f32 = 0.14;

/// 卡带主标题字号（16 / 900）。
pub const FONT_CASS_TITLE: f32 = 16.0;
/// 卡带副行字号（8.5）。
pub const FONT_CASS_ARTIST: f32 = 8.5;
/// 卡带标签行字号（8 / 800）。
pub const FONT_CASS_LABEL: f32 = 8.0;

/// `type/spine-artist` 的 62% 不透明度（§1.2 表）。
pub const SPINE_ARTIST_ALPHA: u8 = 158; // 255 × 0.62

/// 全局圆角 = 0（§1.4；唯一例外 5px 螺丝与状态点是自绘圆，不经 CornerRadius）。
pub const RADIUS_NONE: u8 = 0;

/// 全局零圆角（egui 0.37 `CornerRadius`）。
pub fn radius_none() -> CornerRadius {
    CornerRadius::same(RADIUS_NONE)
}

/// 应用全局视觉：窗口/面板底色、交互色、焦点环（§4.3 2px blue）。
pub fn apply_global_style(ctx: &egui::Context) {
    // R10：显式固定 Light——否则深色系统上 set_visuals 只写当前主题分支，
    // style 走 Dark 分支导致间距/字体与设计稿不一致。
    ctx.set_theme(egui::Theme::Light);
    let mut visuals = egui::Visuals::light();
    visuals.panel_fill = pal().paper;
    // 浮层（tooltip / 上下文菜单）用面板色：暗色档页底太深，浮层贴上去会糊成一片。
    visuals.window_fill = pal().panel;
    visuals.extreme_bg_color = pal().paper;
    visuals.faint_bg_color = pal().panel_alt;
    visuals.override_text_color = Some(pal().ink);
    visuals.selection.bg_fill = pal().blue;
    visuals.selection.stroke = Stroke::new(STROKE_HAIRLINE, fg_on(pal().blue));
    visuals.hyperlink_color = pal().blue;
    visuals.widgets.noninteractive.bg_stroke = stroke_hairline();
    visuals.widgets.inactive.bg_stroke = stroke_component();
    visuals.widgets.hovered.bg_stroke = stroke_component();
    visuals.widgets.active.bg_stroke = stroke_structural();
    visuals.widgets.open.bg_stroke = stroke_structural();
    // §1.4 全局圆角 0（egui 0.37 字段名）。
    visuals.window_corner_radius = radius_none();
    visuals.menu_corner_radius = radius_none();
    visuals.widgets.noninteractive.corner_radius = radius_none();
    visuals.widgets.inactive.corner_radius = radius_none();
    visuals.widgets.hovered.corner_radius = radius_none();
    visuals.widgets.active.corner_radius = radius_none();
    visuals.widgets.open.corner_radius = radius_none();
    visuals.window_shadow = shadow_window();
    visuals.popup_shadow = hard_shadow(4, 4, pal().shadow);
    // 内建控件填充：不让 TextEdit / Button 落到 egui 默认灰底（R6）。
    visuals.widgets.noninteractive.bg_fill = pal().paper;
    visuals.widgets.inactive.bg_fill = pal().paper;
    visuals.widgets.inactive.weak_bg_fill = pal().panel_alt;
    visuals.widgets.hovered.bg_fill = pal().shelf_hover;
    // 内建 Button 走 weak_bg_fill 且文字色由调用方的 LayoutJob 固定为 ink：
    // hover 底若用浅金，暗色档上近白字会消失在底里。这里保持凹陷子面色，
    // 让自绘列表行去用浅金 hover。
    visuals.widgets.hovered.weak_bg_fill = pal().panel_alt;
    visuals.widgets.active.bg_fill = pal().panel_alt;
    visuals.widgets.active.weak_bg_fill = pal().panel_alt;
    visuals.widgets.open.bg_fill = pal().paper;
    visuals.widgets.open.weak_bg_fill = pal().panel_alt;
    // 文本输入底走标签纸色（.field{background:#fbfaf4}）。
    visuals.extreme_bg_color = pal().label_light;
    ctx.set_visuals(visuals);

    // ---- 间距 / 字体（R2 / R6）----
    //
    // egui 默认 item_spacing = (8, 3)，会与代码里手写的 `add_space(8)` 叠加成 16，
    // 纵向每个 allocate 之间也白送 3px。设计稿的间距全是显式值，故全局归零，
    // 间距一律由调用方 `add_space` 给出；`widgets::panel_at` 内部同样归零。
    // egui 0.37 起 `Context::style()/set_style()` 已移除，改为按主题访问；
    // 这里用 `all_styles_mut` 同时覆盖 Light/Dark 两套，避免主题切换后丢样式。
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::Vec2::ZERO;
        style.spacing.button_padding = egui::vec2(10.0, 8.0);
        style.text_styles = [
            (
                egui::TextStyle::Body,
                egui::FontId::new(FONT_LABEL, FontFamily::Proportional),
            ),
            (
                egui::TextStyle::Button,
                egui::FontId::new(FONT_LABEL, FontFamily::Proportional),
            ),
            (
                egui::TextStyle::Small,
                egui::FontId::new(FONT_TINY, FontFamily::Proportional),
            ),
            (
                egui::TextStyle::Monospace,
                egui::FontId::new(FONT_LABEL, FontFamily::Monospace),
            ),
            (
                egui::TextStyle::Heading,
                egui::FontId::new(FONT_TRACK_TITLE, FontFamily::Proportional),
            ),
        ]
        .into();
    });

    // 全局零圆角 + 全直角：关闭羽化，线条落在物理像素上就是实心边。
    ctx.tessellation_options_mut(|o| o.feathering = false);
}

/// 内嵌 MiSans Latin 原版静态字体，保留 Inter 与 Noto 的缺字回退。
///
/// 正文：MiSans Latin Regular → Inter 400 → Noto SC 400 → Noto JP 400。
/// 粗体：MiSans Latin Bold → Inter 700 → Noto SC 700 → Noto JP 700。
/// 标题：MiSans Latin Heavy → Inter 900 → Noto SC 700 → Noto JP 700。
/// 等宽数字继续使用 Hack；缺字先交给 MiSans Latin，再走其余后备。
/// 字重是界面语义档，不把原字体的 OS/2 字重值强改成 400/700/900。
///
/// MiSans 文件直接来自用户提供的 MiSans_Latin.zip，未裁剪或修改。
/// 版权与许可见 assets/fonts/NOTICE-MiSans.txt、LICENSE-MiSans.pdf。
/// Latin 包不覆盖韩文、泰文、阿拉伯文等，不能当作全球文字字体包。
pub fn setup_fonts(ctx: &egui::Context) {
    let mut defs = FontDefinitions::default();

    let embedded: &[(&str, &[u8])] = &[
        ("latin-fallback-regular", include_bytes!("../../assets/fonts/Inter_400Regular.ttf")),
        ("latin-fallback-bold", include_bytes!("../../assets/fonts/Inter_700Bold.ttf")),
        ("latin-fallback-black", include_bytes!("../../assets/fonts/Inter_900Black.ttf")),
        (
            "ui-regular",
            include_bytes!("../../assets/fonts/MiSansLatin-Regular.ttf"),
        ),
        (
            "ui-bold",
            include_bytes!("../../assets/fonts/MiSansLatin-Bold.ttf"),
        ),
        (
            "ui-black",
            include_bytes!("../../assets/fonts/MiSansLatin-Heavy.ttf"),
        ),
        (
            "cjk-regular",
            include_bytes!("../../assets/fonts/NotoSansSC_400.ttf"),
        ),
        (
            "cjk-bold",
            include_bytes!("../../assets/fonts/NotoSansSC_700.ttf"),
        ),
        // 日文补充面：GB2312 没有日文新字体汉字（時/楽/図/駅/円…），
        // Google Fonts 的 Noto Sans SC 本身也不含这些码位（无 slice 覆盖 U+6642），
        // 只能在同族里补一张 Noto Sans JP。子集只保留 SC 缺失的码位（≈3.3k），
        // 两面对拼后日文歌词/曲名不再出方块。
        (
            "jp-regular",
            include_bytes!("../../assets/fonts/NotoSansJP_400.ttf"),
        ),
        (
            "jp-bold",
            include_bytes!("../../assets/fonts/NotoSansJP_700.ttf"),
        ),
    ];
    for (name, bytes) in embedded {
        defs.font_data.insert(
            (*name).to_owned(),
            std::sync::Arc::new(FontData::from_owned((*bytes).to_vec())),
        );
    }

    // 回退链只引用 font_data 里真实存在的字库名。
    // egui 0.37 默认字库（Hack / Ubuntu-Light / egui-icons）不含
    // emoji-icon-font / NotoEmoji-Regular（旧版 egui 才有），
    // 误引用会在 set_fonts 后首次布局时 panic；
    // 常用符号由 cjk-regular（Noto Sans SC）兜底。
    //
    // 顺序即优先级：MiSans Latin → Inter 补充 → 简体汉字与符号走 SC →
    // 日文新字体汉字走 JP。三面覆盖范围互补，缺字才会落到最后一个。
    let proportional = vec![
        "ui-regular".to_owned(),
        "latin-fallback-regular".to_owned(),
        "cjk-regular".to_owned(),
        "jp-regular".to_owned(),
    ];
    defs.families.insert(FontFamily::Proportional, proportional);

    let mono = vec![
        "Hack".to_owned(),
        "ui-regular".to_owned(),
        "latin-fallback-regular".to_owned(),
        "cjk-regular".to_owned(),
        "jp-regular".to_owned(),
    ];
    defs.families.insert(FontFamily::Monospace, mono);

    for (family, primary, fallback) in [
        (font_family_bold(), "ui-bold", "latin-fallback-bold"),
        (font_family_black(), "ui-black", "latin-fallback-black"),
    ] {
        // 真粗面在前；中文没有 900 面，黑色档同样落到 cjk-bold，
        // 再由 `paint_job` 的字号自适应双描补到 ≈900。
        let list = vec![
            primary.to_owned(),
            fallback.to_owned(),
            "cjk-bold".to_owned(),
            "jp-bold".to_owned(),
        ];
        defs.families.insert(family, list);
    }

    super::icons::install(&mut defs);
    ctx.set_fonts(defs);
}

/// 700 字族名（真粗面）。
pub fn font_family_bold() -> FontFamily {
    FontFamily::Name(std::sync::Arc::from("ui-bold"))
}
/// 900 字族名（标题级；中文落到 700 面 + 双描）。
pub fn font_family_black() -> FontFamily {
    FontFamily::Name(std::sync::Arc::from("ui-black"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_hard_shadows_have_zero_blur() {
        // §1.4「硬投影是这个设计的骨相，模糊 = 失去风格」
        for shadow in [
            shadow_window(),
            shadow_button(),
            shadow_shelf(),
            shadow_selected(),
            hard_shadow(4, 4, pal().ink),
        ] {
            assert_eq!(shadow.blur, 0u8, "所有硬投影 blur 必须为 0");
            assert_eq!(shadow.spread, 0u8, "硬投影不允许 spread");
        }
    }

    #[test]
    fn hard_shadow_preserves_offset() {
        let s = hard_shadow(8, 8, pal().ink);
        assert_eq!(s.offset, [8, 8]);
        let b = hard_shadow(2, 2, pal().shelf_shadow);
        assert_eq!(b.offset, [2, 2]);
        assert_eq!(b.color, pal().shelf_shadow);
    }

    #[test]
    fn shadow_gradient_matches_spec() {
        // §1.4 硬投影梯度：window 8 / tape 6 / menu 4 / selected 3 / button·shelf 2
        assert_eq!(shadow_window().offset, [8, 8]);
        assert_eq!(shadow_selected().offset, [3, 3]);
        assert_eq!(shadow_button().offset, [2, 2]);
        assert_eq!(shadow_shelf().offset, [2, 2]);
        assert_eq!(shadow_shelf().color, pal().shelf_shadow);
    }

    #[test]
    fn strokes_follow_width_ladder() {
        // §1.4：结构 2 / 组件 1.5 / 发丝 1
        assert_eq!(stroke_structural().width, 2.0);
        assert_eq!(stroke_component().width, 1.5);
        assert_eq!(stroke_hairline().width, 1.0);
    }

    #[test]
    fn strokes_follow_the_line_token() {
        // 描边经这一族取色，颜色令牌才能一处换掉整个界面的边框
        assert_eq!(stroke_structural().color, pal().line);
        assert_eq!(stroke_component().color, pal().line);
        assert_eq!(stroke_hairline().color, pal().line);
    }

    #[test]
    fn hard_shadows_use_the_shadow_token() {
        // 暗色档的反相陷阱：ink 是近白，拿它当投影会画白光
        assert_eq!(shadow_window().color, pal().shadow);
        assert_eq!(shadow_button().color, pal().shadow);
        assert_eq!(shadow_selected().color, pal().shadow);
        assert_eq!(shadow_toast().color, pal().shadow);
    }

    #[test]
    fn shadow_is_darker_than_the_surface_it_falls_on() {
        for theme in THEMES.iter() {
            let p = &theme.palette;
            assert!(
                luma(p.shadow) < luma(p.paper),
                "{}：投影比页面底还亮，会变成一圈高光",
                theme.name
            );
            assert!(
                luma(p.shelf_shadow) < luma(p.paper),
                "{}：列表投影比页面底还亮",
                theme.name
            );
        }
    }

    #[test]
    fn radius_is_zero() {
        // §1.4「全局圆角 0」
        assert_eq!(RADIUS_NONE, 0u8);
        assert_eq!(radius_none(), CornerRadius::same(0));
    }

    #[test]
    fn spine_artist_alpha_is_62_percent() {
        // §1.2：spine-artist 62% 不透明度
        assert_eq!(SPINE_ARTIST_ALPHA, (255.0_f32 * 0.62).round() as u8);
    }

    #[test]
    fn first_run_theme_is_hakurei_reimu() {
        // 交付口径：首次进入软件默认落在「博丽灵梦」；用户选过的主题由 prefs
        // 覆盖（见 prefs 的往返测试），这里只钉住"没得选时用哪套"
        assert_eq!(DEFAULT_THEME, 1);
        assert_eq!(THEMES[DEFAULT_THEME].name, "博丽灵梦");
        assert_eq!(THEMES[DEFAULT_THEME].palette, REIMU);
    }

    #[test]
    fn themes_are_addressable_by_index_with_unique_names_and_palettes() {
        // prefs 存的是下标：名字撞车会让回显和文件对不上，配色撞车则白占一个槽
        assert_eq!(theme_names().len(), THEMES.len());
        for (i, a) in THEMES.iter().enumerate() {
            assert_eq!(theme_names()[i], a.name, "主题名字必须按 THEMES 顺序给出");
            assert!(!a.name.is_empty(), "主题 {i} 没有名字");
            for (j, b) in THEMES.iter().enumerate() {
                if i != j {
                    assert_ne!(a.name, b.name, "主题 {i} 与 {j} 同名");
                    assert_ne!(a.palette, b.palette, "主题 {i} 与 {j} 配色相同");
                }
            }
        }
    }

    #[test]
    fn out_of_range_theme_indices_clamp_to_the_last_theme() {
        // 越界值来自磁盘（新机器 / 旧版本写的 prefs），只能夹、不能 panic；
        // 夹的边界是 THEMES.len()，将来追加第 4 套配色时这里不必改
        assert_eq!(clamp_theme(0), 0);
        assert_eq!(clamp_theme(THEMES.len() - 1), THEMES.len() - 1);
        assert_eq!(clamp_theme(THEMES.len()), THEMES.len() - 1);
        assert_eq!(clamp_theme(usize::MAX), THEMES.len() - 1);
    }

    /// WCAG 相对亮度（sRGB 线性化后加权）。
    #[test]
    fn luma_and_contrast_match_wcag_endpoints() {
        // 自检度量函数本身：黑白 = 21，同色 = 1，中灰亮度落在 0.2 附近
        assert!((contrast(Color32::BLACK, Color32::WHITE) - 21.0).abs() < 0.05);
        assert_eq!(contrast(pal().paper, pal().paper), 1.0);
        assert!((luma(Color32::from_rgb(0x80, 0x80, 0x80)) - 0.216).abs() < 0.01);
    }

    #[test]
    fn every_theme_keeps_its_text_legible_on_paper() {
        // 新增配色时最容易漏的就是对比度：暗底沿用黑墨、亮底沿用白墨都能过编译，
        // 但界面直接糊成一片。这里按「正文 / 次级文字 / 强调块」三档各卡一条线。
        //
        // mid 的线卡在 2.0 而不是 WCAG AA 的 4.5：早苗档实测 mid/paper = 2.17，
        // 这是设计稿里刻意留的"印刷淡灰"，改它就等于改现有界面。这条线的用途是
        // 拦住未来配色把次级文字做得比现在更糊，不是声明当前已合规。
        for theme in THEMES.iter() {
            let p = &theme.palette;
            assert!(
                contrast(p.ink, p.paper) >= 7.0,
                "{}：正文 ink/paper 对比度 {} 不足 7",
                theme.name,
                contrast(p.ink, p.paper)
            );
            assert!(
                contrast(p.mid, p.paper) >= 2.0,
                "{}：次级文字 mid/paper 对比度 {} 不足 2（比现档还淡）",
                theme.name,
                contrast(p.mid, p.paper)
            );
            assert!(
                contrast(p.blue, p.paper) >= 3.0,
                "{}：强调 blue/paper 对比度 {} 不足 3",
                theme.name,
                contrast(p.blue, p.paper)
            );
            // 次级文字必须比正文淡，但仍是同一侧（不能一个压亮一个压暗）
            assert_eq!(
                luma(p.ink) > luma(p.paper),
                luma(p.mid) > luma(p.paper),
                "{}：ink / mid 不在 paper 的同一侧，层次会翻",
                theme.name
            );
            assert!(
                contrast(p.ink, p.paper) > contrast(p.mid, p.paper),
                "{}：mid 比 ink 还抢眼，主次倒了",
                theme.name
            );
            // 文本框底不能和字一个亮度
            assert_ne!(
                p.label_light, p.ink,
                "{}：label_light 与 ink 同色",
                theme.name
            );
        }
    }

    #[test]
    fn module_panels_keep_text_legible() {
        // 面板底是独立令牌（暗色档比页面底亮一档），正文/次级文字都得在它上面可读
        for theme in THEMES.iter() {
            let p = &theme.palette;
            assert!(
                contrast(p.ink, p.panel) >= 7.0,
                "{}：正文 ink/panel 对比度 {} 不足 7",
                theme.name,
                contrast(p.ink, p.panel)
            );
            assert!(
                contrast(p.mid, p.panel) >= 2.0,
                "{}：次级文字 mid/panel 对比度 {} 不足 2",
                theme.name,
                contrast(p.mid, p.panel)
            );
            assert!(
                contrast(p.line, p.panel) >= 1.2,
                "{}：描边 line/panel 对比度 {} 太低，模块轮廓会消失",
                theme.name,
                contrast(p.line, p.panel)
            );
        }
    }

    #[test]
    fn fg_on_is_legible_on_every_accent_surface() {
        // 选中行 / 强调面 / 确认按钮都靠 fg_on 取字色：每套主题的每个可能底色，
        // 挑出来的前景都必须至少 3:1（大字/图形的 WCAG 线），否则反相档会糊。
        for theme in THEMES.iter() {
            let p = &theme.palette;
            for (name, bg) in [
                ("blue", p.blue),
                ("acid", p.acid),
                ("pink", p.pink),
                ("error_mark", p.error_mark),
                ("panel_alt", p.panel_alt),
                ("panel", p.panel),
                ("label_light", p.label_light),
                ("shelf_hover", p.shelf_hover),
                ("art_bg", p.art_bg),
            ] {
                let fg = fg_on_palette(p, bg);
                assert!(
                    contrast(fg, bg) >= 3.0,
                    "{}：{name} 面上 {fg:?} 只有 {} 对比度",
                    theme.name,
                    contrast(fg, bg)
                );
            }
        }
    }

    #[test]
    fn palettes_are_all_opaque_where_that_matters() {
        // paper / ink 是实心面与描边的基色，带 alpha 会和下层叠出设计外的灰
        for theme in THEMES.iter() {
            for (field, c) in [
                ("paper", theme.palette.paper),
                ("panel", theme.palette.panel),
                ("ink", theme.palette.ink),
                ("shadow", theme.palette.shadow),
                ("line", theme.palette.line),
                ("blue", theme.palette.blue),
                ("acid", theme.palette.acid),
                ("favorite", theme.palette.favorite),
                ("selected_mark", theme.palette.selected_mark),
                ("ornament", theme.palette.ornament),
                ("art_bg", theme.palette.art_bg),
                ("spectrum_low", theme.palette.spectrum_low),
                ("thumb", theme.palette.thumb),
            ] {
                assert_eq!(c.a(), 255, "{}：{field} 不允许半透明", theme.name);
            }
        }
    }

    #[test]
    fn every_semi_transparent_token_is_valid_premultiplied() {
        // egui 的 Color32 是预乘 alpha：rgb 不得大于 alpha。写了未预乘的亮色
        // 就会在暗底上渲染成纯白块（「雾雨魔理沙」尾栏踩过），而对比度测试
        // 只看不透明文字，拦不住这类问题，所以在这里逐项卡住。
        for theme in THEMES.iter() {
            let p = &theme.palette;
            for (field, c) in [
                ("tape_shadow", p.tape_shadow),
                ("modal_dim", p.modal_dim),
                ("art_inset", p.art_inset),
                ("art_stripe", p.art_stripe),
                ("spine_end_line", p.spine_end_line),
                ("spine_end_bg", p.spine_end_bg),
                ("spine_end_bg_sel", p.spine_end_bg_sel),
                ("spine_end_line_sel", p.spine_end_line_sel),
            ] {
                assert!(
                    c.r() <= c.a() && c.g() <= c.a() && c.b() <= c.a(),
                    "{}：{field} 不是合法预乘色 rgba=({}, {}, {}, {})",
                    theme.name,
                    c.r(),
                    c.g(),
                    c.b(),
                    c.a()
                );
            }
        }
    }

    #[test]
    fn margins_map_panel_padding() {
        // §1.3：面板内边距 14 / 容器 10（egui 0.37 Margin 为 i8）
        assert_eq!(margin_panel(), Margin::same(14));
        assert_eq!(margin_window(), Margin::same(10));
    }
}
