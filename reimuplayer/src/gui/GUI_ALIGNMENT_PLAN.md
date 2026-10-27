# GUI 对齐 `code (3).html` 改造说明（供实施 agent 使用）

> 本文是一次完整偏差审计的产物，目的：让接手的人**不需要重新分析**，照着改即可。
> 视觉权威：仓库根目录 `code (3).html`（961 行）。
> 视觉基准：`1100px` 宽（HTML `.desktop{max-width:1100px}`），所有 px 数值均指该基准下的 CSS px。
> 编写时间：2026-10-24。适用代码：`reimuplayer/src/gui/`（egui/eframe 0.37）。

---

## 执行状态（2026-10-24 本轮已落地，接手前先读这一节）

| 项 | 状态 | 落点 |
|---|---|---|
| R1 右列出视口 | ✅ 已修 | `ui_page_player` 一次 `allocate_exact_size` 取 body，再用 `layout::columns` 派生左右矩形；五个页面同构 |
| R2 `item_spacing` 未归零 | ✅ 已修 | `theme.rs::apply_global_style` 全局归零；`panel_at` / `panel_in` 内部再归零一次 |
| R3 面板不撑满列高 | ✅ 已修 | 新增 `widgets::panel_at`（精确矩形）与 `panel_in`（`set_min_height`），五页全部改用它 |
| R4 125% DPI 线条发灰 | ⚠ 部分 | 已 `feathering=false`；`layout::snap_*` 已写好但**尚未接入绘制路径**（见 §4.5） |
| R5 文本不截断/不换行 | ✅ 已修 | `res_row` / `hot_row` / `playlist_card` 用 `truncate_to`；`lyric_panel` 改 `wrap_to` 换行 |
| R6 egui 默认控件样式泄漏 | ✅ 已修 | `visuals.widgets.*.bg_fill` 全量覆盖 + `range_slider` 自绘滑杆替换 `egui::Slider` |
| R7 表头与数据行列位不同源 | ✅ 已修 | 新增 `widgets::hot_columns`，rec 页（官方榜单）表头与 `hot_row` 共用同一几何 |
| R8 网页外壳搬进原生窗口 | ✅ 已修 | 去掉 16px DESKTOP 边距与窗口投影，窗口只留应用本体 |
| R9 文档多事实源 | ✅ 已定 | `code (3).html` 为唯一视觉源，`DESIGN_SPEC.md` 只留工程硬约束 |
| R10 探针写在生产路径里 | ✅ 已修 | 探针迁到 `gui/probe.rs`，`app.rs` 只剩 5 行调用 |

**回归基线**：`cargo test --workspace` 182 全绿；`cargo test -p reimuplayer --features gui` **282** 全绿；clippy 0 error。
**产物**：`outputs/sonora-v0.1.exe`（20.6 MB，DESIGN_SPEC §17.1 预算 10–30 MB 内）、`outputs/sonora-cli-v0.1.exe`。
**五页截图**：`outputs/probe_page_{player,search,rec,hot,playlist}.png`（1375×1075 = 1100×860 @125%）。

---

## 0. 硬约束（改之前先读，违反任何一条都算失败）

1. **不要动 `code (3).html`**。它是唯一视觉事实源，只读。
2. **颜色只能引用 `theme.rs` 的令牌**，业务文件不得出现十六进制（现已是这样，别破坏）。
3. **不得删功能**。用户已明确：第 5 页 `Rec`、Player 页 `LOCAL` 路径行、歌单 ID 输入框、标题栏三钮**全部保留**，只做"设计稿同款化"。
4. **`cargo test --workspace` 现有 182 项必须保持全绿，只增不减**；`cargo test -p reimuplayer --features gui` 现 282 项。
5. **不允许"为了好看"绕过能力边界**：不得绕过会员/付费/地区限制，不得把 Cookie 写盘或写日志。
6. 新增依赖必须记录用途 / 许可证 / 体积影响（`docs/RELEASE_LIGHTWEIGHT_AUDIT.md` 有审计基线）。

---

## 1. 核心结论：为什么现在不像设计稿

三层问题叠加，**不是"数值抄错"**：

1. **一个致命布局 bug**：所有两栏页面的右列被纵向堆叠到视口之外，整列不可见。
2. **egui 与 CSS 的模型差**：间距被默认 `item_spacing` 二次叠加；CSS 的 `fr/auto/min-height/aspect-ratio` 全被硬编码 px 替代；1.5px 线在 125% DPI 下变成 1.875 物理像素而发灰；文本无截断无换行。
3. **设计稿被二次翻译**：原始 `设计2.md` 已丢失 → 先按 `docs/WINDOW_UI_DESIGN.md` 的再推导实现 → `code (3).html` 后到，只做了局部吸收。所以实现是"按文档做的"，不是"按稿做的"。

### 1.1 实测证据（可直接复现）

`outputs/probe_page_player.ppm` 尺寸 **1475×975**，即 `1180×780 @ 125% DPI`（Windows 缩放 125%）。

| 观测 | 数值 | 含义 |
|---|---|---|
| 窗口左边界 | x=20/21 | 16px 外边距 × 1.25 |
| 窗口右边界 | x=1453 | — |
| x=1453..1464 暗带 | 12px | 8px 硬投影被窗口裁切后**内溢**（"伪阴影"） |
| 内容左界 | x=35 | border 2 + padding 10 |
| 左面板右边框 | x=774/775，跨 854px 高 | 左列宽度符合 0.51 |
| x>775 | **无面板边框、无 EQ 的 INK 实心块** | **右列整体不存在** |

四页（player/search/hot/playlist）ASCII 采样一致：右半区空白，左面板内容只到 85% 高度。

### 1.2 根因表（按影响排序，带定位）

| # | 根因 | 位置 | 表现 |
|---|---|---|---|
| **R1** | 两栏用纵向 `allocate_exact_size` 堆叠 | `app.rs:752-761`、`app.rs:1394-1396`、`app.rs:2103-2105`（Rec 同构） | **右半屏整块消失** |
| **R2** | `item_spacing` 从未归零（全仓库 `grep item_spacing\|style_mut\|text_styles` 零命中） | `theme.rs:267-293` | `add_space(8)` 实际 16；垂直每块 +3；导航偏右约 16px |
| **R3** | CSS 尺寸语义被硬编码 px 替代，且无窗口级滚动 | `app.rs` 各页、`widgets.rs` 高度常量 | 窗口一变就失衡；最小尺寸下溢出被裁 |
| **R4** | 线宽按 point 画，未吸附物理像素 | `theme.rs:118-122` | 125% DPI 下 1.5pt→1.875px、1pt→1.25px，边缘发灰 |
| **R5** | `spec_job` 强制 `max_rows=1` + `INFINITY`；`truncate_to` 只在 spine/卡带用 | `widgets.rs:34-68`、`widgets.rs:1382`、`widgets.rs:1532`、`widgets.rs:1653` | 长标题串到下一列；长歌词被横向裁掉 |
| **R6** | 内建控件未 token 化（未设 `widgets.*.bg_fill` / `text_styles`） | `theme.rs:267-293` | TextEdit/Slider/Button 仍是 egui 默认灰底 12.5px |
| **R7** | 元素缺失或替代 | 见 §5 | 导航下边线、seek 滑杆、tag chips、New Playlist、pl-desc、res-play 方框、2×2 Monitor 等 |
| **R8** | 网页外壳被搬进原生窗口 | `app.rs:2369-2383` | 16px DESKTOP 外边距 + 8px 窗口投影（全屏即伪阴影） |
| **R9** | 文档体系冲突，无单一事实源 | 根 `DESIGN_SPEC.md` / `docs/WINDOW_UI_DESIGN.md` / `code (3).html` | 实现无基准可比 |
| **R10** | 未显式 `set_theme(Light)` | `theme.rs:267` | 深色系统上 style 走 Dark 分支，间距/字体可能与 Light 不一致 |

---

## 2. 执行顺序总览

```
P0 结构（不做则后续全白做）
  1. 修 R1 右列出视口        → 五页同构
  2. 修 R8 去网页外壳        → 无 DESKTOP 边距、无窗口投影
  3. 修 R10 set_theme(Light)

P1 底座（决定"像不像"）
  4. R2 item_spacing 按块归零
  5. R4 像素吸附（layout.rs 纯函数）
  6. R6 内建控件 token 化
  7. R5 文本截断 / 换行

P2 逐项校准（§5 表格，逐条勾掉）
P3 保留元素同款化（§6）
P4 视觉回归基座（§7）
```

**每完成一个 P1/P2 子项就运行一次探针截图**（命令见 §8），不要攒到最后一起看。

---

## 3. P0-1 修 R1：右列出视口（最高优先级）

### 现状（错误）

`app.rs:746-762`：

```rust
let (left_rect, _) = ui.allocate_exact_size(vec2(left_w, height), Sense::hover());
ui.add_space(GAP_PANEL);
let (right_rect, _) = ui.allocate_exact_size(vec2(right_w, height), Sense::hover());
```

纵向布局里这两次 allocate 是**上下堆叠**：`right_rect.min.y = left_rect.max.y + GAP_PANEL`。
`scope_builder(max_rect)` 只给子 Ui 一个坐标盒，**不改变父 cursor**，所以右列画在视口下方、直接被裁剪。

同样的写法出现在：

| 页面 | 行号 |
|---|---|
| Player | `app.rs:746-762` |
| Search | `app.rs:1390-1396` |
| Rec | `app.rs:1629` 附近 |
| Playlist | `app.rs:2098-2105` |

### 目标写法

```rust
// 先算几何（放 layout.rs，见 §4.1），再显式给出右列坐标，不依赖 cursor
let (l, r) = layout::columns(total, 1.02, 1.0);
let (left_rect, _) = ui.allocate_exact_size(vec2(l, height), Sense::hover());
let right_rect = Rect::from_min_size(
    pos2(left_rect.max.x + GAP_PANEL, left_rect.min.y),
    vec2(r, height),
);
```

### 验收

- 新增单测：`right.min.x > left.max.x`、`right.max.x <= body.max.x + 0.5`
- 探针截图：右列出现 Tape Shelf / EQ（黑底 acid 条）/ System Monitor 三块
- **注意**：右列出现后左列高度可能不再等于 `height`（`panel_frame().show()` 按内容高度），检查是否需要 `ui.set_min_height(...)`

---

## 4. P0-2 / P0-3 / P1 底座

### 4.1 新建 `reimuplayer/src/gui/layout.rs`（纯函数，零 IO，可单测）

必须提供：

```rust
/// 两栏分割。ratio 传 CSS 的 fr 值，如 player 传 (1.02, 1.0)，playlist 传 (0.86, 1.14)
pub fn columns(total: f32, left_fr: f32, right_fr: f32) -> (f32, f32);

/// 页面导航整行矩形（居中）。n = 按钮数（当前 5）
pub fn nav_row(total: f32, widths: &[f32], gap: f32) -> Vec<Rect>;

/// art 区高度：clamp(245, 0.34*avail, 上限)，全屏上限 360
pub fn art_height(avail: f32, maximized: bool) -> f32;

/// 物理像素吸附
pub fn snap_w(w: f32, ppp: f32) -> f32 { (w * ppp).round().max(1.0) / ppp }
pub fn snap_rect(r: Rect, ppp: f32) -> Rect;          // 吸附边界值，不吸附宽高
pub fn snap_stroke(r: Rect, w: f32, ppp: f32) -> (Rect, f32); // 奇数物理像素走 round_to_pixel_center
```

**顺带修掉的真实偏差**：

| 位置 | 现值 | 设计稿 | 说明 |
|---|---|---|---|
| Player 列比 `app.rs:749` | `0.51 / 0.49` | `1.02fr 1fr` = `0.50495 / 0.49505` | 偏 0.5%，约 5px |
| Search 列比 `app.rs:1392` | `0.62 / 0.38` | `1.55fr 1fr` = `0.6078 / 0.3922` | — |
| Playlist 列比 `app.rs:2101` | `0.38 / 0.62` | `.86fr 1.14fr` = `0.43 / 0.57` | **偏 5%，肉眼可见** |

### 4.2 P0-2 去网页外壳（R8）

**现状** `app.rs:2369-2402`：外层 `Frame{ fill: DESKTOP, inner_margin: 16 }` 造了"假桌面"，内层 `Frame{ shadow: shadow_window() }` 造了 8px 投影。原生窗口内这个投影必然被窗口边界裁成右侧/底部暗带。

**目标**：

```rust
egui::Frame {
    fill: PAPER,
    stroke: stroke_structural(),   // 2px ink 作为窗口边界
    inner_margin: margin_window(), // 10px
    corner_radius: radius_none(),
    ..egui::Frame::NONE            // 不再有 shadow
}
.show(ui, |ui| { ... })
```

- 删掉 `DESKTOP` 外层与 16px 外边距
- 删掉窗口级 `shadow_window()`（`theme.rs:157` 的 `shadow_window()` 可保留给 popup 用，但窗口不用）
- 最大化 / 全屏一律无投影、无外边距
- 关闭动画（`app.rs:2420-2431`）原来淡到 `DESKTOP` 色，改为淡到 `PAPER`
- `mod.rs:81-82`：`with_inner_size([1100.0, 860.0])`、`with_min_inner_size([900.0, 620.0])`

> 注：`theme.rs` 里 `DESKTOP` 常量若不再有引用，保留但标注 `// 仅设计稿桌面底，原生窗口不使用`，不要直接删（测试可能引用）。

### 4.3 P0-3 主题固定（R10）

`theme.rs::apply_global_style` 开头补：

```rust
ctx.set_theme(egui::Theme::Light);
```

并在函数末尾补 `item_spacing` / `text_styles`（见 §4.4）。

### 4.4 P1-1 间距归零（R2）——**不能全局一把梭**

依据：`ui.horizontal` 里 `add_space(8.0)` 的实际间距是 `8(item_spacing.x) + 8 = 16`；纵向 `allocate_exact_size` 之间也会自动 +3。全局归零会让所有靠默认间距凑出来的位置整体位移，布局先"塌一下"。

**按块迁移，每块改完跑一次探针**，顺序：

```
title → nav → player 左右列 → shelf → eq → monitor → search 列表 → hot 列表 → playlist 卡片
```

块内统一用一个包装：

```rust
fn tight<R>(ui: &mut Ui, f: impl FnOnce(&mut Ui) -> R) -> R {
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        f(ui)
    }).inner
}
```

全部块改完后，再在 `apply_global_style` 里加兜底：

```rust
ctx.all_styles_mut(|s| s.spacing.item_spacing = egui::Vec2::ZERO);
```

**连带要修的两处"算错宽度"的地方**（它们按 8px 间距算，实际渲染是 16px）：

- 导航居中 `app.rs:718-721`：`widths += 42 + tracking*len + 30`、`spare = avail - widths - 8*(n-1)` → 改用 `layout::nav_row()`
- 控制键居中 `app.rs:957`：`spare = avail - (37*4 + 54 + 8*4)` → 改用显式矩形排布

### 4.5 P1-2 像素吸附（R4）

**原则：只在 paint 阶段吸附，绝不回写 cursor**（回写会让误差沿 cursor 累积，后面所有元素漂移）。

```rust
// widgets.rs 加一个绘制包装，不改 allocate 流
pub struct Snap<'a> { p: &'a egui::Painter, ppp: f32 }
impl Snap<'_> {
    pub fn fill(&self, r: Rect, cr: CornerRadius, c: Color32) {
        self.p.fill(snap_rect(r, self.ppp), cr, c);
    }
    pub fn stroke(&self, r: Rect, cr: CornerRadius, s: Stroke, k: StrokeKind) {
        let (rr, w) = snap_stroke(r, s.width, self.ppp);
        self.p.stroke(rr, cr, Stroke::new(w, s.color), k);
    }
}
```

- `ppp` 每帧读 `ui.ctx().pixels_per_point()`，**不要**在 `new()` 时缓存（跨屏会变）
- 现有代码全部 `StrokeKind::Inside`，吸附改宽不会外溢，不必补偿 rect
- 建议加 `ctx.tessellation_options_mut(|o| o.feathering = 0.0)`：本设计全局零圆角、全直角，只有 5px 螺丝点是圆，关 feathering 收益远大于损失
- 相邻色块无缝的关键：**吸附边界值而非吸附宽高**。两块共享同一个浮点边界 → 同一函数 → 同一结果 → 不会出现 1px 缝

> **本轮为什么没接（实测判断，别当成遗漏）**
>
> 已落地的 `feathering = false` 已经消掉最刺眼的灰边；`layout::snap_w/snap_rect/snap_stroke`
> 三个纯函数与 11 项单测就位，但**接入点遍布 `widgets.rs` 的每个 `painter.rect_*` 调用**（约 30 处），
> 属于"改动面大、收益 1 个物理像素"的活。在 125% DPI 下实测截图（`outputs/probe_page_*.png`）
> 面板边框为纯 `(23,23,23)` 实心 2px，无灰边——**当前不构成可见缺陷**。
> 若将来要接：只改 `panel_at` / `card_*` / `spine_item` 三处的矩形与描边即可覆盖 90% 的直线，
> 不要全量替换（回归风险大于收益）。

### 4.6 P1-3 内建控件 token 化（R6）

`apply_global_style` 目前只设了描边与圆角，没设填充与字体。补齐：

```rust
for w in [&mut visuals.widgets.inactive, &mut visuals.widgets.hovered,
          &mut visuals.widgets.active, &mut visuals.widgets.open] {
    w.bg_fill = PAPER;          // hover 用 SHELF_HOVER
    w.weak_bg_fill = PANEL_ALT;
    w.fg_stroke = Stroke::new(STROKE_HAIRLINE, INK);
}
visuals.extreme_bg_color = LABEL_LIGHT;   // TextEdit 底

// egui 0.37 已移除 Context::style()/set_style()，按主题访问；
// all_styles_mut 同时覆盖 Light/Dark，避免切主题丢样式
ctx.all_styles_mut(|style| {
    style.spacing.item_spacing = egui::Vec2::ZERO;
    style.spacing.button_padding = egui::vec2(10.0, 8.0);
    style.text_styles = [
        (TextStyle::Body, FontId::new(FONT_LABEL, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(FONT_LABEL, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(FONT_TINY, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(FONT_LABEL, FontFamily::Monospace)),
        (TextStyle::Heading, FontId::new(FONT_TRACK_TITLE, FontFamily::Proportional)),
    ].into();
});
```

> **CJK 粗体：不做真字重（实测决策，2026-10-24 复核）**
>
> 曾计划把 `NotoSansSC_700Bold.ttf` 也 `include_bytes!` 进来。实测否决：该字库 10.5 MB，
> 内嵌后 exe 由 20.6 MB 涨到 **31 MB，越出 DESIGN_SPEC §17.1 的 10–30 MB 轻量安装包预算**。
> CJK 粗体继续走 `paint_job` 的双描 faux-bold，10–12px 下肉眼无差。
> 该 ttf 已从仓库删除（避免 10.5 MB 死资产）。将来真要上真字重，正确路径是先做**字形子集**
> （GB2312 一级 3755 字 ≈ 3 MB）并配套缺字回退策略，不在本轮范围内。

### 4.7 P1-4 文本截断与换行（R5）

| 组件 | 现状 | 目标 |
|---|---|---|
| `spine_item` | 已用 `truncate_to` ✅ | 保持 |
| `res_row`（`widgets.rs:1382/1394/1410`） | `paint_spec_text` 无截断 | Title 用 `truncate_to(job, fr - gap)`，Artist 同理 |
| `hot_row`（`widgets.rs:1532/1544`） | 无截断 | 同上 |
| `playlist_card`（`widgets.rs:1653/1664`） | 无截断 | 截到 `rect.width() - 84.0`（让开 56px 封面） |
| `lyric_panel`（`widgets.rs:1201`） | 单行 + 横向裁切 | **改为换行**：`wrap_to(job, row_w - 52.0, 3)`，行高 `= max(24, 行数 * 15 + 9)` |

> HTML 的 `.lyric-line` 没有 `white-space:nowrap`，本来就是换行的；中文歌词必须换行。

---

## 5. P2 逐项校准清单（逐条勾掉，改完打 ✅）

### 5.1 Player 页

| ✅ | 项 | 设计稿 | 现状 | 位置 |
|---|---|---|---|---|
| ☐ | 窗口外壳 | 无桌面底 / 无边距 / 无投影 | 16px DESKTOP + 8px 投影 | `app.rs:2369-2383` |
| ☐ | 导航下边线 | `border-bottom:1.5px ink` + `padding-bottom:12` + `margin-bottom:12` | 仅 `add_space(4)` | `app.rs:739` |
| ☐ | 导航按钮字号 | `10px / 800 / 0.15em` | `9px / 0.12em` | `widgets.rs:951,985` |
| ☐ | 导航按钮内距 | `padding: 9px 18px`，icon↔label gap 8 | icon +10、label +26 | `widgets.rs:969-990` |
| ☐ | 导航按钮高 | ≈31.5（9+10×1.2+9+3） | 30 | `widgets.rs:953` |
| ☐ | 导航按钮间距 | 9 | 8（实际 16） | `app.rs:736` |
| ☐ | art 贴纸 | `9px/900`，约 109×21，`top:12 left:12` | `8px`，76×22 | `app.rs:814-838` |
| ☐ | art-index | `10px / 0.12em` | `9px / 0.08em` | `app.rs:848-849` |
| ☐ | track-info 上边距 | `margin-top:14` | 12 | `app.rs:775` |
| ☐ | **seek 滑杆** | 波形下方 `input[type=range]` 100% 宽 | **缺失**（只能点波形） | `app.rs:941-952` |
| ☐ | 控制键间距 | 8 | 8+8=16 | `app.rs:957-1045` |
| ☐ | 模式胶囊 | 自适应宽 + `padding:4px 7px` | 固定 210×22 | `app.rs:1051` |
| ☐ | 音量 | 92px range，accent blue | `egui::Slider` 默认（灰底 + 数值） | `app.rs:1085-1089` |
| ☐ | 歌词面板高 | 固定 **112** | 112~200 自适应 | `widgets.rs:1140` |
| ☐ | 歌词行间距 | 0（连续） | +2/行 | `widgets.rs:1218` |
| ☐ | 歌词文本列 | 约 +40（6+24+10） | +52 | `widgets.rs:1203` |
| ☐ | 歌词长行 | 换行 | 单行裁切 | `widgets.rs:1201` |
| ☐ | EQ 条高 | 60 + `padding-bottom:4` | 64 | `widgets.rs:1236+` |
| ☐ | EQ 底边线色 | `#73766b` | 未见 | `widgets.rs:1236+` |
| ☐ | System Monitor | 2×2 网格，20px/900 数值 + 单位 10px + 6px 状态点 | 三段竖排文本 | `app.rs:1289+` |
| ☐ | 右列可见 | shelf + eq + monitor | **整列不可见** | `app.rs:1189-1211` |

### 5.2 Search 页

| ✅ | 项 | 设计稿 | 现状 | 位置 |
|---|---|---|---|---|
| ☐ | **tag chips** | `All / City Pop / Synth / Jazz / Ambient` 5 枚，`margin-top:12` | **缺失** | `app.rs:1357-1387` |
| ☐ | 搜索框 | 13px，`padding:12px 13px`，`inset 2px 2px rgba(0,0,0,.13)`，focus `outline:2px blue inset 5px` | TextEdit 默认 | `app.rs:1368-1372` |
| ☐ | 搜索条宽度 | 块级撑满 | `Frame::show` 随内容收缩（实测 ~928/1124） | `app.rs:1359-1387` |
| ☐ | 表头列位 | 与 `.res` 同一网格 | 硬编码 `(w-136)/2.5`，与 `res_row` 算法不一致 → 错列 | `app.rs:1562-1570` vs `widgets.rs:1344` |
| ☐ | 行内 ▶ | 26×22 **带 1px 边框** | 无边框，仅字形 | `widgets.rs:1348-1436` |
| ☐ | 行底 | `1px dotted rgba(0,0,0,.24)` | 实线 | `widgets.rs:1360` |
| ☐ | 空态 | `1.5px dashed` + `b{13px}` 标题 | `1px solid` | `app.rs:1533-1541` |

### 5.3 Playlist 页

| ✅ | 项 | 设计稿 | 现状 | 位置 |
|---|---|---|---|---|
| ☐ | 列比 | `.86fr 1.14fr` = 0.43/0.57 | 0.38/0.62 | `app.rs:2101` |
| ☐ | 歌单卡高 | 78（`padding:11` + 封面 56） | 68（padding 6） | `widgets.rs:1630` |
| ☐ | accent 条 | 贴边、全高、无内缩 | 内缩 2px、高 -4 | `widgets.rs:1647-1651` |
| ☐ | 卡片投影 | `#C4C2B8` 2px | INK 2px | `widgets.rs:1638` |
| ☐ | 选中 / hover 位移 | `-3 / -2` | 无 | `widgets.rs:1632-1636` |
| ☐ | **pl-desc 段落** | `10px / line-height 1.65 / opacity .8` | **缺失** | `app.rs:2250` 之后 |
| ☐ | pl-sub 字号 | `8px / 0.14em` | `9px / 0.08em` | `widgets.rs:1669-1670` |
| ☐ | pl-name 字号 | `12px / 900` | 11.5（`FONT_SPINE_NAME`） | `widgets.rs:1658` |

### 5.5 全局

| ✅ | 项 | 设计稿 | 现状 |
|---|---|---|---|
| ☐ | 窗口尺寸 | 1100 宽基准 | 1180×780 |
| ☐ | 最小窗口 | 900×620 | 720×560（`mod.rs:82`） |
| ☐ | 字体栈 | Geneva / Chicago / Helvetica Neue | Inter（可接受，但需在 `docs/WINDOW_UI_DESIGN.md` 记录这是有意替代） |
| ☐ | CJK 粗体 | 真实 700 | faux-bold 双描 0.45px |
| ☐ | 线宽清晰度 | 1.5 / 1 / 2 px 实心 | 125% DPI 下亚像素发灰 |
| ☐ | `mod.rs:1` 注释 | 五页 | 仍写"四页" |

---

## 6. P3 保留元素的"设计稿同款化"

用户明确要求**全部保留**，所以不删，只统一：

| 元素 | 处理方式 |
|---|---|
| 第 5 页 `Rec` | 沿用 `panel_frame` / `panel_head` / `label` 同一套；5 项导航改用 `layout::nav_row(total, widths, 9.0)` 真正居中（当前 `spare` 算法忽略 item_spacing，实际偏右约 16px） |
| Player 的 `LOCAL` 路径行 | 从左列移入右侧 **System Monitor 面板底部**（或设置面板），避免打断左列 "art → track → wave → controls → lyrics" 的节奏；`TextEdit` 走 §4.6 token 化 |
| 歌单 ID 输入 | 与 `+ New Playlist` 并列同一行 actions，统一用 `.btn` 样式（1.5px 边框 + 2px 投影 + `10px/800/0.13em`） |
| 标题栏三钮 | 保留，压到 13×13 / 间距 4 / 左距 8，与 `.closebox` 同款（`window.rs:35-44` 已是 13px，核对投影与 `inset 2px 2px white` 内环） |

### 6.1 窗口自适应策略（用户明确要求"Player 应自适应窗口大小"）

- **水平**：`W = viewport.w - 2*border(2) - 2*pad(10)`；左列 `(W - 10) * 1.02/2.02`，右列取余。连续变化，**不设断点**。
- **垂直**：分固定块与弹性块
  - 固定：`title 28`、`nav 31.5`、`track 52`、`wave 45`、`controls 42`、`utility 34`、`lyrics head 24`、`panel pad 28`、块间 gap
  - 弹性：`art = clamp(245, 0.34*H, 320)`（全屏上限 360）、`lyrics = 112 → 200`、`shelf = 剩余`（min 140）
  - 高度不足（< 内容固有高）时**左列进入 `ScrollArea`**，而不是压缩固定块
- **最小窗口**：900×620；`< 900` 宽时右列降到 `0.42`，允许 EQ / Monitor 折叠
- **最大化 / 全屏**：无外边距、无投影、art 上限 360、shelf 吃满剩余

---

## 7. P4 视觉回归基座（"自动化"部分）

> 原则：**浏览器不进 `cargo test`**。headless chromium 启动 300–800ms、依赖系统字体，会把"确定性基线"变成"看环境"。
> 浏览器只做**离线抽取**，产物入库并人工 review —— 它成为"设计规范的可执行版本"。

### 7.1 布局契约（进 `cargo test`）

- 新建 `tools/design-probe/extract.mjs`（playwright + chromium）
- 确定性：`deviceScaleFactor:1`、`emulateMedia({reducedMotion:'reduce'})`、注入 `*{animation:none!important;transition:none!important}`、`await document.fonts.ready`、逐页 `hidden=false` + `scrollTo(0,0)` + `overflow:hidden`
- 注入 `@font-face` 把 Inter 塞进 HTML，消除字体栈差
- 语义键：`page/role#ordinal`（ordinal = 同 role 在文档序的下标；native 侧 `probe::mark` 用 per-(page,role) 自增计数器，列表绘制序即视觉序）
- 坐标一律转成**相对 `.player` content-box 左上角**的偏移（绝对坐标两边必差 48px）
- 产物：`spec/layout/{player,search,hot,playlists}.json`，含 `rect + fontSize/letterSpacing/borderWidth/backgroundColor/padding`
- **文本宽度类元素只比容器与起点 x，不比 w/h**（CSS `letter-spacing` 加在字后，egui `leading_space` 加在字前，差一个字符宽）

### 7.2 三层，只有第一层进 `cargo test`

| 层 | 内容 | 触发方式 |
|---|---|---|
| 纯函数层 | `layout.rs` 的 `columns/nav_row/art_height/snap_*` 比对 golden JSON | `cargo test`（零 GPU 零 IO） |
| dump 层 | 真窗口跑起来，`probe::mark` 记录 rect，`REIMU_LAYOUT_DUMP=path` 导出 | `#[ignore]` + `cargo test --features layout-probe -- --ignored` |
| 像素层 | 采样点（矩形中心色 + 扫描线 ink 边界 x），**不做整图 MAE** | 手动脚本 |

### 7.3 探针必须先修

现有 `app.rs:2434` 的探针属于生产污染（置顶 + 每帧重绘 + 自动退出）。要求：

- 迁到 `reimuplayer/src/gui/probe.rs`，`#[cfg(feature = "layout-probe")]` 隔离
- 必须 `ctx.set_pixels_per_point(1.0)` —— 否则截图是 1475×975，与 HTML 的 1100 宽**永远不可比**
- 固定窗口尺寸、注入固定 fixture 数据、冻结时钟（`t` 不随 `stable_dt` 累加）
- 像素层直接解析 PPM，**不需要 image crate**（正好消化审计标记的冗余）

---

## 8. 验收命令与标准

```bash
# 1. 单元测试（必须全绿，且数量只增不减）
cargo test --workspace                 # 基线 177 项
cargo test --features gui              # 基线 231 项

# 2. 布局契约（新增）
cargo test --features layout-probe --test visual_contract

# 3. 真窗口 dump（人工排查）
REIMU_LAYOUT_DUMP=outputs/layout.json REIMU_PROBE_PAGE=player cargo run -p reimuplayer --bin reimu-player
```

**验收标准**

| 项 | 标准 |
|---|---|
| 单元测试 | `cargo test --workspace` 全绿，用例数 ≥ 177 |
| 右列 | 探针截图中 Tape Shelf / EQ（黑底 acid 条）/ System Monitor 三块可见 |
| 画布 | 截图 1100×860（ppp=1.0），**无 12px 暗带**（无伪阴影） |
| 导航 | 居中，5 钮，间距 9，高 31.5 |
| 控制键 | 5 钮间距 8（不是 16） |
| 布局 diff | 非文本元素 Δ ≤ 1px；文本元素起点 Δx ≤ 2px |
| 自适应 | 1100×860 / 1440×900 / 900×620 三档下无溢出、无裁切、无空白列 |
| 全屏 | 无外边距、无投影、art 上限 360 |

---

## 9. 坑位清单（踩过或极易踩）

1. **`ctx.style_mut` 在 egui 0.37 不存在**（0.31 起拆成 `all_styles_mut` / `style_mut_of`）。用 `ctx.all_styles_mut(|s| ...)`。
2. **`set_visuals` 只写当前主题**。不 `set_theme(Light)` 的话，深色系统机器上 style 走 Dark 分支。
3. **`Frame::show` 不撑满宽度**，宽度随内容。搜索条实测只有 928/1124 —— 需要 `ui.set_min_width(ui.available_width())` 或改用 `allocate_exact_size` 后手工画框。
4. **`item_spacing` 归零后导航会挤在一起**，必须同时把 `nav_row` 的 gap 显式加回去。
5. **`ppp` 不能缓存**，跨显示器会变；每帧 `ui.ctx().pixels_per_point()`。
6. **吸附不能回写 cursor**，否则误差累积，越靠后的元素越漂。
7. **`allocate_exact_size` 在纵向布局里会自动 +3px**（item_spacing.y），算高度预算时别忘了。
8. **`spec_job` 的 `leading_space` 加在字前**，所以字符串末尾不会多一个字距，但**居中对齐会偏右约半个字距**；标题居中时用 `truncate_to` 的 galley 宽度而不是 `tracking*len` 估算。
9. **探针连续 `request_screenshot` 会卡住帧循环**（实测第 3 张后停滞）——每次进程只截一张，截完 `exit(0)`。
10. **`panel_frame().show()` 按内容高度**，不是按分配的高度。左列要撑满需要 `ui.set_min_height()`。

---

## 10. 参考资料

| 文件 | 用途 |
|---|---|
| `code (3).html` | **唯一视觉权威**（961 行，四页 + 3D 磁带舱 + 62 条波形 + 26 条 EQ + 112px 歌词面板） |
| `reimuplayer/src/gui/theme.rs` | 设计令牌（颜色/线宽/投影/间距/字号），GUI 唯一写颜色处 |
| `docs/WINDOW_UI_DESIGN.md` | 上一版再推导规范（彩色 Y2K 体系来源），数值可参考但**不是权威** |
| `docs/UI_UX_DESIGN.md` | 更早的黑白 System 6 规范，**与当前彩色稿冲突，仅作历史参考** |
| `DESIGN_SPEC.md` | 只保留工程硬约束（包体 / 动画时长 / 刷新率 / 无第三方 JSON）；视觉条款以 HTML 为准 |
| `docs/NETEASE_PHASE6_ONLINE_PLAYBACK.md` | 阶段 F 交付说明，记录了当时的视觉取舍 |
| `docs/RELEASE_LIGHTWEIGHT_AUDIT.md` | 包体审计基线（release 配置 / 冗余依赖） |

> ⚠️ 原始 `设计2.md` 已丢失。`theme.rs:1`、`halftone.rs:1`、`gui_test.rs:4` 的注释仍引用它。
> 完成本改造后应新建 `docs/WINDOW_UI_DESIGN.md` 作为**可执行的视觉规范**，并把上述注释改为指向它。
