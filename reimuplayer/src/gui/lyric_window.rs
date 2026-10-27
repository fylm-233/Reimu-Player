//! 桌面歌词悬浮窗（[`DesktopLyricSink`] 的 eframe 实现）。
//!
//! ## 一个窗口，一块组件
//!
//! ```text
//!   lyric  1000×120  歌词文字 + 右上角锁定图标（唯一视口）
//! ```
//!
//! 早先版本为了兼顾"鼠标穿透"和"能点锁定"拆成了展示窗 + 控件窗两个视口，
//! 视觉上像一块浮在歌词上的小面板。现在只保留**一个窗口**：歌词、锁定图标、
//! 拖动、右键菜单全部在同一块画布上，不再有重叠组件。
//!
//! ## 交互
//!
//! - **锁定图标**：点击在锁定 / 解锁之间切换。两态用**两个不同字形**
//!   （[`icons::LOCK`] 合上的挂锁 / [`icons::UNLOCK`] 翻开的锁梁），锁定态
//!   另加 ACID 绿——形状差异才是主判据，颜色只是加强。
//! - **拖动**：未锁定时窗口任意位置都能拖；锁定只禁用拖动与右键菜单。
//! - **透明度**：未锁定时右键呼出菜单，用滑杆调背景不透明度。
//! - **0%**：背景不画任何东西（无底板、无描边、无衬底），屏幕上只剩歌词文字。
//!
//! 窗上**只有歌词**：曲名/艺人那行辅助文字已按用户要求去掉——0% 不透明度下
//! 它是一行极淡的白字，横在歌词上方像一条多余的白色线条。
//!
//! 拖动**自己算屏幕坐标**（见 [`DragTracker`]），不用 `ViewportCommand::StartDrag`：
//! 后者在 egui-winit 里有一道 `window.has_focus()` 门槛，悬浮窗天生不是焦点窗口，
//! 命令会被静默丢弃（阶段 14 实测完全拖不动）。
//!
//! ## 透明窗口的三条纪律（都踩过）
//!
//! 1. `App::clear_color` 是**全进程一个值**，eframe 用它清每个视口的 framebuffer。
//!    默认值 `rgba(12,12,12,180)` 会把歌词窗清成一块**深色直角矩形**——外面是
//!    直角的窗口、里面是圆角底板，正是"直角外窗内嵌圆角窗口"的成因。
//!    现在 `ReimuApp::clear_color` 返回全透明，主窗口自己铺一张不透明纸底。
//! 2. 底板必须**铺满整个 rect**：圆角即窗口形状，四角透出桌面，视觉上才是
//!    "一个圆角窗口"，而不是"矩形窗口里画了个圆角方块"。
//! 3. 半透明色一律走 `Color32::from_black_alpha`（乘好 alpha 的直通色），
//!    不要拿不透明色手动改 alpha 再当预乘色用。
//! 4. 窗口**顶端 1px** 是 Windows 10 的 DWM 描边，不在 framebuffer 里，也不受
//!    去边框 / 去非客户区渲染控制；只能按物理像素用 `SetWindowRgn` 剪掉第 0 行
//!    （见 [`super::click_through::clip_top_border`]），DPI 变化后要重剪。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui;

use super::desktop_lyric::{DesktopLyricSink, LyricFrame};
use super::icons;
use super::lyric_control::{LyricControl, ProgressStyle};
use super::theme::pal;
use super::widgets;
use crate::i18n::{pick, Lang};

/// 视口的哈希种子（同一进程内唯一）。
const BODY_HASH: &str = "reimu-player-lyric-body-v3";

/// 窗口标题。
const BODY_TITLE: &str = "REIMU PLAYER LYRIC";

/// 视口默认尺寸（宽 × 高，px）。
const BODY_SIZE: [f32; 2] = [1000.0, 120.0];

/// 底板圆角半径（px）。窗口四角由它决定形状。
const RADIUS: u8 = 16;

/// 画底板描边所需的最低不透明度（0..=255，约 12.5%）。
///
/// 低于这个值只画底板、不画描边：那条 `alpha/3` 的线在极淡的底板上
/// 反而更抢眼，看起来像"阴影没擦干净"（用户实测反馈）。
const STROKE_MIN_ALPHA: u8 = 32;

/// 锁定图标的点击区（正方形，画在右上角）。
const LOCK_ICON_BOX: f32 = 24.0;

/// 锁定图标距窗口右 / 上边缘的内缩。
const LOCK_ICON_INSET: f32 = 12.0;

/// 锁定图标的字形字号（px）。
///
/// 比点击区（[`LOCK_ICON_BOX`]）小一圈：图标字库的字形四周自带留白，
/// 字号等于点击区会让锁体顶到边缘、看着像被裁掉。
const LOCK_GLYPH_SIZE: f32 = 15.0;

/// 等显示器尺寸就绪的最多帧数（约 1 秒）。
///
/// 见 [`LyricWindow::register`]：winit 的 `current_monitor()` 在窗口刚创建时
/// 可能返回 `None`，等不到就按内侧档兜底。
const LOCATE_TRIES: u32 = 60;

/// 当前行的字号。
const LINE_SIZE: f32 = 26.0;

/// 默认落位与主窗口之间的间隙（px）。
const DOCK_GAP: f32 = 8.0;

/// 下一行的字号。
const NEXT_SIZE: f32 = 15.0;

/// 一帧的自有副本（`show_viewport_deferred` 的回调要求 `'static`）。
///
/// 只留**这块窗真的会画**的东西：歌词当前行 / 下一行 / 行内进度 / 是否在播。
/// `LyricFrame` 里的 `title` / `artist` 不拷进来——窗上那行辅助文字已删除，
/// 拷过来只会变成没人读的死字段。
#[derive(Clone, Debug)]
struct OwnedFrame {
    line: Option<String>,
    next: Option<String>,
    progress: f32,
    playing: bool,
}

impl OwnedFrame {
    fn from_borrowed(frame: &LyricFrame<'_>) -> Self {
        Self {
            line: frame.line.map(str::to_string),
            next: frame.next.map(str::to_string),
            progress: frame.line_progress.clamp(0.0, 1.0),
            playing: frame.playing,
        }
    }
}

/// 歌词窗的跨帧状态（位置 / 拖动）。
///
/// 为什么塞进一个结构：`show_viewport_deferred` 的回调是 `Fn` + `'static`，
/// 只能靠 `Arc` 捕获；拆成多个字段会让闭包捕获一堆 `Arc`，反而更容易漏。
#[derive(Debug, Default)]
struct Shared {
    /// 首次摆放用的初始位置（屏幕坐标 points）。
    ///
    /// **只用于窗口创建那一刻**，之后位置只由拖动改变。踩过的坑（阶段 16）：
    /// 早先版本每帧把窗口的**实际位置**回写到这里，再作为 `with_position` 传给
    /// builder，形成"读回实际位置 → 再当成期望位置下发"的反馈环——窗口会以每帧
    /// 几像素的速度持续漂移，最终漂到屏幕外，表现为"桌面歌词打开了但看不见"。
    initial_pos: Mutex<Option<egui::Pos2>>,
    /// 拖动跟踪。
    drag: Mutex<DragTracker>,
    /// 首帧是否已经用 `OuterPosition` 把窗口挪到初始位置。
    ///
    /// 为什么不只靠 `ViewportBuilder::with_position`：阶段 16 实测在 125% 缩放下
    /// 它不可靠（窗口会落在屏幕外或屏幕中央）。`OuterPosition` 走的是
    /// `inner_rect` 同一套 points 坐标，落点精确，且**只发一次**。
    docked: AtomicBool,
    /// 初始落位尝试了几帧（等 `monitor_size` 就绪，见 [`LOCATE_TRIES`]）。
    locate_tries: AtomicU32,
    /// 窗口外观是否已修正（去 DWM 边框 / 投影 / 系统圆角）。
    ///
    /// 窗口是异步创建的，第一帧 `EnumWindows` 找不到它；成功一次就置位，
    /// 之后不再每帧枚举（那是一次全系统窗口遍历，没必要反复做）。
    frame_fixed: AtomicBool,
    /// 已剪掉顶部 1px 时的物理客户区尺寸（`宽 << 32 | 高`）。
    ///
    /// `SetWindowRgn` 的区域按物理像素计、不随 DPI 缩放：窗口尺寸一变，
    /// 旧区域就会把内容裁掉一截，所以必须按新尺寸重剪。存 0 表示还没剪过。
    clip_px: AtomicU64,
}

impl Shared {
    fn initial_pos(&self) -> Option<egui::Pos2> {
        *self.initial_pos.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn set_initial_pos(&self, pos: egui::Pos2) {
        *self.initial_pos.lock().unwrap_or_else(|p| p.into_inner()) = Some(pos);
    }
}

/// 拖动一帧的结论（纯数据，便于单测）。
#[derive(Debug, Clone, Copy, PartialEq)]
enum DragStep {
    /// 不需要下发任何命令。
    Idle,
    /// 下发 `ViewportCommand::OuterPosition(此处的位置)`。
    Move(egui::Pos2),
}

/// 拖动跟踪（**跨帧**保存）。
///
/// 为什么状态必须放在这里而不是 `render()` 里：`show_viewport_deferred` 的回调
/// 是 `Fn`（不是 `FnMut`）且每帧由 `present()` 重新注册，闭包内的局部变量留不住。
#[derive(Debug, Default)]
struct DragTracker {
    /// 按下瞬间的（指针屏幕坐标, 窗口外框左上角）。
    anchor: Option<(egui::Pos2, egui::Pos2)>,
    /// 上一次下发的窗口位置（同一值不重复下发，省掉无谓的 WM_WINDOWPOSCHANGING）。
    last: Option<egui::Pos2>,
}

/// 由「按下时」与「当前」的指针屏幕坐标算出窗口外框该在哪。
///
/// 纯函数：拖动全程 `start_outer` / `grab_screen` 固定，所以结果只跟指针
/// 真实位移有关，跟我们自己把窗口挪到哪无关。
fn drag_target(
    start_outer: egui::Pos2,
    grab_screen: egui::Pos2,
    now_screen: egui::Pos2,
) -> egui::Pos2 {
    start_outer + (now_screen - grab_screen)
}

/// 默认位置：**贴主窗口下沿外侧**居中（经典桌面歌词位置）。
///
/// 为什么不按屏幕算：`monitor_size` 与窗口位置的单位在不同缩放下极易对不上
/// （阶段 16 实测 125% 缩放下自己按屏幕算出的 y 会把窗口送到屏幕外，表现为
/// "打开了但看不见"）。主窗口的 `inner_rect` 与 `monitor_size` 都是同一套
/// points 坐标，拿它做参照**永远不会算错**，也不需要假设任务栏高度。
///
/// 两个落位档，按"下沿外侧放不放得下"自动选：
/// - **放得下**（常态）→ 主窗口下沿外侧 [`DOCK_GAP`] 处。既不遮挡播放器内容，
///   也不占用屏幕中央，最符合"桌面歌词"的直觉；
/// - **放不下**（主窗口最大化 / 已贴屏幕底）→ 退到主窗口内侧底边上方。
///   此时歌词条压在播放器内容上，用户可以随时把它拖走。
///
/// `main` 为 `None`（拿不到主窗口几何）时退化成左上角一块安全区域，绝不使用
/// 任何凭空的屏幕尺寸；`monitor` 为 `None` 时一律走内侧档，宁可叠在播放器上，
/// 也不冒"窗口落到屏幕外"的风险。
fn default_body_pos(
    main: Option<egui::Rect>,
    size: egui::Vec2,
    monitor: Option<egui::Vec2>,
) -> egui::Pos2 {
    let area = main
        .unwrap_or_else(|| egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0)));
    // 水平居中于主窗口
    let x = (area.center().x - size.x * 0.5).max(area.left());
    let below = area.bottom() + DOCK_GAP;
    let fits_below = monitor.is_some_and(|m| below + size.y <= m.y);
    let y = if fits_below {
        below
    } else {
        (area.bottom() - size.y - DOCK_GAP * 3.0).max(area.top())
    };
    egui::pos2(x, y)
}

impl DragTracker {
    /// 喂一帧输入，得到该帧的动作。
    ///
    /// `down` = 指针正按在本窗口上；`inner`/`outer` 是该视口自己的几何
    /// （`ViewportInfo::inner_rect` / `outer_rect`，屏幕坐标，points）；
    /// `pointer` = 指针在本视口内的坐标。
    fn step(
        &mut self,
        down: bool,
        inner: Option<egui::Rect>,
        outer: Option<egui::Rect>,
        pointer: Option<egui::Pos2>,
    ) -> DragStep {
        if !down {
            // 松手 / 丢焦点即结束，下次按下重新取锚点
            self.anchor = None;
            self.last = None;
            return DragStep::Idle;
        }
        let (Some(inner), Some(outer), Some(pointer)) = (inner, outer, pointer) else {
            // 最小化等极端情况下拿不到窗口几何，本帧不动（也没有可见窗口）
            return DragStep::Idle;
        };
        // 视口内坐标 + inner 原点 = 屏幕坐标。窗口被我们移动时两项反向变化，
        // 求和不变——这就是不会自激振荡的原因。
        let screen = inner.min + pointer.to_vec2();
        let (grab, start_outer) = match self.anchor {
            Some(anchor) => anchor,
            None => {
                self.anchor = Some((screen, outer.min));
                self.last = Some(outer.min);
                return DragStep::Idle;
            }
        };
        let target = drag_target(start_outer, grab, screen);
        if self.last == Some(target) {
            return DragStep::Idle;
        }
        self.last = Some(target);
        DragStep::Move(target)
    }
}

/// 桌面歌词窗口。
pub struct LyricWindow {
    ctx: egui::Context,
    /// 锁定 / 透明度 / 进度条样式的共享状态（与设置面板共用）。
    control: Arc<LyricControl>,
    /// 位置与拖动状态。
    shared: Arc<Shared>,
    /// 界面语言（每帧由主程序推送，菜单与提示文案跟着变）。
    lang: Lang,
    frame: Option<OwnedFrame>,
    /// 视口是否已经创建（关闭后置回 `false`，下次推送重建）。
    open: bool,
}

impl LyricWindow {
    pub fn new(ctx: egui::Context, control: Arc<LyricControl>) -> Self {
        Self {
            ctx,
            control,
            shared: Arc::new(Shared::default()),
            lang: Lang::Zh,
            frame: None,
            open: false,
        }
    }

    /// 控制状态的共享句柄（设置面板读写）。
    pub fn control(&self) -> Arc<LyricControl> {
        self.control.clone()
    }

    /// 更新界面语言（右键菜单与锁定提示的文案）。
    pub fn set_lang(&mut self, lang: Lang) {
        self.lang = lang;
    }

    fn body_id() -> egui::ViewportId {
        egui::ViewportId::from_hash_of(BODY_HASH)
    }

    /// 关闭视口（禁用 / 退出时调用）。
    fn close_viewport(&mut self) {
        if self.open {
            self.ctx
                .send_viewport_cmd_to(Self::body_id(), egui::ViewportCommand::Close);
            self.open = false;
            // 锁定态不跨"关闭→开启"存活：否则刚打开就是锁死的，用户还不知道为什么
            self.control.set_locked(false);
            // 下次开启会创建一个全新的 HWND：外观修正与顶部剪边都得对新窗口重做
            self.shared.frame_fixed.store(false, Ordering::SeqCst);
            self.shared.clip_px.store(0, Ordering::Relaxed);
        }
    }

    /// 向 egui 声明（或更新）窗口。
    ///
    /// **必须每个父视口 pass 调用一次**：egui 的 `context.rs` 在父 pass 结束时
    /// 会把 `is_our_child && !viewport.used` 的子视口直接删掉（连窗口一起）。
    /// `show_viewport_deferred` 的调用就是"本 pass 我还在用"的唯一凭据。
    fn register(&self) {
        let Some(owned) = self.frame.clone() else {
            return;
        };
        let control = self.control.clone();
        let shared = self.shared.clone();
        let lang = self.lang;

        // 初始位置只算一次（计算本身有副作用：写进 shared，供首帧落位用）。
        // 见 [`Shared::initial_pos`] 的踩坑说明——之后永不改写。
        if self.shared.initial_pos().is_none() {
            // 主视口的屏幕矩形 + 显示器尺寸：与窗口位置同一套 points 坐标，
            // 不会因缩放算错（按物理像素自己算才是踩坑的那条路）
            let (main, monitor) = self
                .ctx
                .input(|i| (i.viewport().inner_rect, i.viewport().monitor_size));
            // `monitor_size` 来自 winit 的 `current_monitor()`，窗口刚创建的那一帧
            // 可能还是 `None`（阶段 16 实测同一台机器不同次启动会时有时无，
            // 于是默认落位在"下沿外侧"和"内侧"两档之间随机跳）。等它就绪再算，
            // 最多等 [`LOCATE_TRIES`] 帧——等不到就按内侧档兜底，绝不无限等。
            let tries = self.shared.locate_tries.fetch_add(1, Ordering::Relaxed);
            if main.is_some() && (monitor.is_some() || tries >= LOCATE_TRIES) {
                let pos = default_body_pos(main, egui::vec2(BODY_SIZE[0], BODY_SIZE[1]), monitor);
                tracing::debug!(?main, ?monitor, ?pos, tries, "桌面歌词初始落位");
                self.shared.set_initial_pos(pos);
            }
        }

        // 不给 `with_position`：阶段 16 实测它在 125% 缩放下不可靠（窗口会落到
        // 屏幕外或屏幕中央）。落位统一交给首帧的 `OuterPosition`（见 render_body
        // 里的 `docked`），走的是 `inner_rect` 同一套坐标，落点精确。
        let builder = egui::ViewportBuilder::default()
            .with_title(BODY_TITLE)
            .with_inner_size(BODY_SIZE)
            // 固定尺寸：拖动按整窗几何算，能改大小会让落位参考漂移
            .with_resizable(false)
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_taskbar(false);

        self.ctx
            .show_viewport_deferred(Self::body_id(), builder, move |ui, _class| {
                render_body(ui, &owned, &control, &shared, lang);
            });

        // 窗口外观修正：去掉 DWM 会画的边框 / 投影 / 系统圆角。
        //
        // 不做这一步，全黑背景下能看到「透明区域外还有一圈直角硬边」——
        // 那是 DWM 的窗口边框与投影，不是我们画的东西（内容层是 16px 圆角）。
        // eframe 即使写了 `with_decorations(false)`，窗口样式里仍带
        // `WS_CAPTION | WS_SYSMENU`（本机实测 style=0x16CB0000），DWM 据此
        // 认定"这是个有标题栏的窗口"，照样画边框和阴影。
        //
        // 窗口是异步创建的，头几帧枚举不到；成功一次就置位，之后不再枚举。
        if !self.shared.frame_fixed.load(Ordering::SeqCst)
            && super::click_through::fix_transparent_frame(BODY_TITLE)
        {
            self.shared.frame_fixed.store(true, Ordering::SeqCst);
            tracing::info!("桌面歌词窗外观已修正（无边框 / 无投影 / 无系统圆角）");
        }
    }
}

impl DesktopLyricSink for LyricWindow {
    fn present(&mut self, frame: &LyricFrame<'_>) {
        let first_frame = !self.open;
        self.frame = Some(OwnedFrame::from_borrowed(frame));
        self.open = true;
        if first_frame {
            // 独立视口无法靠主窗截图验证，日志是唯一的创建证据
            tracing::info!("桌面歌词悬浮窗已创建（单视口）");
        }
        self.register();
    }

    fn tick(&mut self) {
        // 内容没变也要重新声明一次，否则暂停时窗口会被 egui 回收
        if self.open {
            self.register();
        }
    }

    fn set_enabled(&mut self, enabled: bool) {
        if !enabled {
            self.close_viewport();
            self.frame = None;
        }
    }

    fn shutdown(&mut self) {
        self.close_viewport();
        self.frame = None;
    }
}

/// 画一帧歌词，并处理这块窗上的交互（拖动 / 锁定 / 右键菜单）。
///
/// 回调拿到的是**该视口自己的根 `Ui`**（egui 0.37 起不再给 `&Context`），
/// 所以不需要 `CentralPanel`——直接在根 `Ui` 上画，视口尺寸即窗口尺寸。
fn render_body(
    ui: &mut egui::Ui,
    frame: &OwnedFrame,
    control: &LyricControl,
    shared: &Shared,
    lang: Lang,
) {
    let ctx = ui.ctx().clone();
    let rect = ui.max_rect();
    let locked = control.locked();

    // Windows 10 的 DWM 会在窗口顶端压一条 1px 描边（激活时纯白、失焦浅蓝灰）。
    // 它画在我们的 framebuffer 之外（帧里首行是透明/底板像素），低透明度底板上方
    // 那条"白色细线"就是它。Win10 没有可关掉它的 DWMWA_BORDER_COLOR，只能把
    // 第 0 行从窗口区域里挖掉（见 click_through::clip_top_border）。
    // 区域按物理像素计、不随 DPI 缩放：按物理尺寸去重，尺寸一变就重剪。
    let ppp = ctx.pixels_per_point();
    let clip_w = (rect.width() * ppp).round().max(0.0) as u64;
    let clip_h = (rect.height() * ppp).round().max(0.0) as u64;
    let clip_want = (clip_w << 32) | clip_h;
    if clip_want != 0
        && shared.clip_px.load(Ordering::Relaxed) != clip_want
        && super::click_through::clip_top_border(BODY_TITLE)
    {
        shared.clip_px.store(clip_want, Ordering::Relaxed);
    }

    // 歌词窗是独立视口，屏幕截图在这台机器上拍不到它的内容（拍到的是窗口底色），
    // 唯一可信的校对途径是直读它自己的 framebuffer。
    #[cfg(debug_assertions)]
    if let Some(path) = super::probe::lyric_shot_path() {
        static SHOT_DONE: AtomicBool = AtomicBool::new(false);
        if !SHOT_DONE.swap(true, Ordering::Relaxed) {
            ctx.request_screenshot(move |img| {
                let mut bytes = Vec::with_capacity(img.pixels.len() * 4);
                for px in &img.pixels {
                    bytes.extend_from_slice(&px.to_array());
                }
                match image::RgbaImage::from_raw(img.width() as u32, img.height() as u32, bytes) {
                    Some(buf) => match buf.save(&path) {
                        Ok(()) => tracing::info!(%path, "probe: 歌词窗帧已写盘"),
                        Err(e) => tracing::warn!(%path, %e, "probe: 歌词窗帧写盘失败"),
                    },
                    None => tracing::warn!("probe: 歌词窗帧尺寸不匹配"),
                }
            });
        }
    }

    // 首帧落位：eframe 的 `with_position` 在 125% 缩放下不可靠（实测会把窗口
    // 放到屏幕外），这里用 `OuterPosition` 校正一次。只发一次，绝不回读，
    // 所以不会形成"读回实际位置→再下发"的漂移环。
    if !shared.docked.load(Ordering::SeqCst) {
        if let (Some(want), Some(now)) =
            (shared.initial_pos(), ctx.input(|i| i.viewport().inner_rect))
        {
            shared.docked.store(true, Ordering::SeqCst);
            if (now.min - want).length() > 0.5 {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(want));
                tracing::debug!(?want, ?now, "桌面歌词首帧落位");
            }
        }
    }

    // 整窗就是拖动面：未锁定时任意位置都能拖（不用先找准手柄）。
    // 锁定态只保留 hover——位置已固定，再让鼠标蹭动就失去"锁定"的意义。
    let bg = ui.interact(
        rect,
        ui.scope_id().with("lyric-surface"),
        if locked {
            egui::Sense::hover()
        } else {
            egui::Sense::click_and_drag()
        },
    );

    if locked {
        // 清掉半截拖动状态，解锁后从新锚点开始
        let _ = shared
            .drag
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .step(false, None, None, None);
    } else {
        if bg.hovered() {
            ctx.set_cursor_icon(egui::CursorIcon::Grab);
        }
        if bg.dragged() {
            ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
        }
        // 只认主键：右键是菜单，不能顺带把窗口拖走
        let primary_down = ctx.input(|i| i.pointer.button_down(egui::PointerButton::Primary));
        let (inner, outer, pointer) = ctx.input(|i| {
            let v = i.viewport();
            (v.inner_rect, v.outer_rect, i.pointer.interact_pos())
        });
        let step = shared.drag.lock().unwrap_or_else(|p| p.into_inner()).step(
            bg.is_pointer_button_down_on() && primary_down,
            inner,
            outer,
            pointer,
        );
        if let DragStep::Move(pos) = step {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
        }
    }

    // 背景：0% 时**什么都不画**——没有底板、没有描边、没有 hover 衬底，
    // 屏幕上只剩歌词文字与锁定图标，其余区域彻底透出桌面。
    let alpha = control.backdrop_alpha();
    let painter = ui.painter();
    if alpha > 0 {
        // 底板铺满整个 rect：圆角即窗口形状，四角透出桌面
        painter.rect_filled(
            rect,
            egui::CornerRadius::same(RADIUS),
            egui::Color32::from_black_alpha(alpha),
        );
        // 描边只在底板足够"实"的时候画。透明度压得很低时，这条 `alpha/3`
        // 的线会比底板本身还显眼，看着像"阴影没擦干净"——用户实测反馈过。
        // 低于阈值就完全不画，纯底板反而干净。
        if alpha >= STROKE_MIN_ALPHA {
            painter.rect_stroke(
                rect.shrink(0.5),
                egui::CornerRadius::same(RADIUS),
                egui::Stroke::new(1.0, egui::Color32::from_black_alpha(alpha / 3)),
                egui::StrokeKind::Inside,
            );
        }
    }

    let inner = rect.shrink2(egui::vec2(28.0, 14.0));

    // 右上角：单个锁定图标。它是这块窗上唯一的控件，点击即切换锁定态。
    let lock_center = egui::pos2(
        rect.right() - LOCK_ICON_INSET - LOCK_ICON_BOX * 0.5,
        inner.top() + 6.0,
    );
    let lock_box =
        egui::Rect::from_center_size(lock_center, egui::vec2(LOCK_ICON_BOX, LOCK_ICON_BOX));

    // 曲名 / 艺人那行辅助文字已删除：0% 不透明度下它是横在歌词上方的一行
    // 极淡白字，看着就是"一条多余的白色线条"（用户实测反馈）。桌面歌词
    // 只负责歌词，曲目信息主窗口里已经有。

    // 锁定图标注册在整窗拖动面之后 → 命中测试优先给它，点图标不会拖窗。
    let icon = ui.interact(
        lock_box,
        ui.scope_id().with("lyric-lock"),
        egui::Sense::click(),
    );
    if icon.hovered() {
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    paint_lock_icon(ui, lock_center, locked);
    let hint = if locked {
        pick(
            lang,
            "解锁：恢复拖动与右键菜单",
            "Unlock: dragging and right-click come back",
        )
    } else {
        pick(
            lang,
            "锁定：固定位置并关闭右键菜单",
            "Lock: fix the position and disable right-click",
        )
    };
    if icon.on_hover_text(hint).clicked() {
        let now_locked = control.toggle_lock();
        tracing::info!(locked = now_locked, "桌面歌词锁定状态切换");
        ctx.request_repaint();
    }

    // 未锁定时右键呼出透明度菜单；锁定时不提供（位置固定就别再改外观了）。
    if !locked {
        bg.context_menu(|ui| {
            ui.set_min_width(210.0);
            ui.label(pick(lang, "背景透明度", "BACKGROUND OPACITY"));
            ui.horizontal(|ui| {
                let mut pct = control.opacity() as f32;
                let (changed, _) = widgets::range_slider(
                    ui,
                    ui.scope_id().with("lyric-opacity"),
                    &mut pct,
                    0.0,
                    100.0,
                    140.0,
                    false,
                );
                if changed {
                    control.set_opacity(pct.round() as u8);
                    ctx.request_repaint();
                }
                ui.label(format!("{}%", pct.round() as u8));
            });
        });
    }

    // 当前行（无歌词时给一句诚实的占位）
    let current = match frame.line.as_deref() {
        Some(line) if !line.trim().is_empty() => line,
        _ => "♪",
    };
    let line_color = if frame.playing {
        egui::Color32::WHITE
    } else {
        egui::Color32::from_white_alpha(190)
    };
    let line_job = widgets::truncate_to(
        widgets::spec_job_weight(
            current,
            LINE_SIZE,
            0.01,
            line_color,
            false,
            widgets::Weight::Bold,
        ),
        inner.width(),
    );
    let line_y = inner.center().y - 6.0;
    // 字影：白色歌词压在亮壁纸上时靠这层深色描边立住
    let mut shadow = line_job.clone();
    for section in &mut shadow.sections {
        section.format.color = egui::Color32::from_black_alpha(140);
    }
    widgets::paint_job(
        ui,
        egui::pos2(inner.center().x + 1.5, line_y + 1.5),
        egui::Align2::CENTER_CENTER,
        shadow,
        false,
    );
    widgets::paint_job(
        ui,
        egui::pos2(inner.center().x, line_y),
        egui::Align2::CENTER_CENTER,
        line_job,
        false,
    );

    // 下一行（预告）
    if let Some(next) = frame.next.as_deref().filter(|s| !s.trim().is_empty()) {
        let next_job = widgets::truncate_to(
            widgets::spec_job(
                next,
                NEXT_SIZE,
                0.01,
                egui::Color32::from_white_alpha(150),
                false,
            ),
            inner.width(),
        );
        widgets::paint_job(
            ui,
            egui::pos2(inner.center().x, line_y + LINE_SIZE + 6.0),
            egui::Align2::CENTER_CENTER,
            next_job,
            false,
        );
    }

    // 行内进度条：样式可配（默认细白线，彩色档跟随当前主题强调色）。
    // 0% 时按要求只留文字，进度条也一起隐藏。
    if frame.line.is_some() && alpha > 0 {
        paint_progress(ui, inner, frame.progress, control.progress());
    }

    // 播放中才需要连续帧（逐字进度）；暂停时 10fps 足够
    ctx.request_repaint_after(std::time::Duration::from_millis(if frame.playing {
        16
    } else {
        200
    }));
}

/// 进度条的一帧外观（纯数据，便于单测"主题色只属于 Acid 档"这件事）。
#[derive(Debug, Clone, Copy, PartialEq)]
struct ProgressLook {
    track: egui::Color32,
    fill: egui::Color32,
    height: f32,
    radius: egui::CornerRadius,
}

/// 样式 → 外观。`Off` 返回 `None`（整条不画）。
///
/// **只有 [`ProgressStyle::Acid`] 用主题强调色**；默认档 `Line` 是细白线。
/// 这条"彩色不是默认"的约束就钉在这个函数里，由单测守着。
fn progress_look(style: ProgressStyle) -> Option<ProgressLook> {
    match style {
        ProgressStyle::Off => None,
        ProgressStyle::Line => Some(ProgressLook {
            track: egui::Color32::from_white_alpha(45),
            fill: egui::Color32::from_white_alpha(235),
            height: 2.0,
            radius: egui::CornerRadius::same(1),
        }),
        ProgressStyle::Acid => Some(ProgressLook {
            track: egui::Color32::from_white_alpha(40),
            fill: pal().acid,
            height: 3.0,
            radius: egui::CornerRadius::ZERO,
        }),
    }
}

/// 行内进度条（三种样式共用同一套几何，只换颜色与高度）。
fn paint_progress(ui: &egui::Ui, inner: egui::Rect, progress: f32, style: ProgressStyle) {
    let Some(look) = progress_look(style) else {
        return;
    };
    let bar_w = (inner.width() * 0.62).min(520.0);
    let bar = egui::Rect::from_center_size(
        egui::pos2(inner.center().x, inner.bottom() - 2.0),
        egui::vec2(bar_w, look.height),
    );
    let painter = ui.painter();
    painter.rect_filled(bar, look.radius, look.track);
    let mut done = bar;
    done.set_width(bar.width() * progress.clamp(0.0, 1.0));
    painter.rect_filled(done, look.radius, look.fill);
}

/// 画锁定图标（图标字库字形，两态用两个不同字形）。
///
/// **形状是主判据**：锁定 = [`icons::LOCK`]（合上的挂锁），解锁 =
/// [`icons::UNLOCK`]（锁梁翻开的开锁）。锁定态再叠一层 ACID 绿加强，
/// 但即使看不清颜色，字形本身也能区分——早先两态共用一个手绘锁形、只改颜色，
/// 用户实测反馈"分不清锁没锁"。
///
/// 图标与文字一样垫一层深色投影，浅色壁纸下也读得清。
fn paint_lock_icon(ui: &egui::Ui, center: egui::Pos2, locked: bool) {
    let (glyph, color) = if locked {
        (icons::LOCK, pal().acid)
    } else {
        (icons::UNLOCK, egui::Color32::from_white_alpha(215))
    };
    let shadow = icons::icon_job(glyph, LOCK_GLYPH_SIZE, egui::Color32::from_black_alpha(150));
    widgets::paint_job(
        ui,
        center + egui::vec2(1.0, 1.0),
        egui::Align2::CENTER_CENTER,
        shadow,
        false,
    );
    let job = icons::icon_job(glyph, LOCK_GLYPH_SIZE, color);
    widgets::paint_job(ui, center, egui::Align2::CENTER_CENTER, job, false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_frame_clamps_progress_and_copies_text() {
        let frame = LyricFrame {
            title: "T",
            artist: "A",
            line: Some("line"),
            next: Some("next"),
            line_progress: 1.7,
            playing: true,
        };
        let owned = OwnedFrame::from_borrowed(&frame);
        assert_eq!(owned.progress, 1.0);
        assert_eq!(owned.line.as_deref(), Some("line"));
        assert_eq!(owned.next.as_deref(), Some("next"));
        assert!(owned.playing);
    }

    #[test]
    fn viewport_id_is_stable() {
        // 同一进程内必须稳定，否则每次注册都会开出一个新窗
        assert_eq!(LyricWindow::body_id(), LyricWindow::body_id());
    }

    #[test]
    fn control_is_shared_with_the_settings_panel() {
        let ctx = egui::Context::default();
        let control = Arc::new(LyricControl::new());
        let window = LyricWindow::new(ctx, control.clone());
        let flag = window.control();
        assert!(!flag.locked());
        // 设置面板改的是同一个对象，歌词窗下一帧就能读到
        flag.set_locked(true);
        assert!(window.control().locked());
    }

    #[test]
    fn closing_the_window_resets_the_lock() {
        // 「关掉桌面歌词再打开」必须回到可交互状态，否则用户会遇到
        // "刚打开就点不动、又不知道该去哪解锁"
        let ctx = egui::Context::default();
        let control = Arc::new(LyricControl::new());
        let mut window = LyricWindow::new(ctx, control.clone());
        window.open = true;
        control.set_locked(true);
        window.set_enabled(false);
        assert!(!control.locked(), "关闭后锁定态必须复位");
        assert!(!window.open);
    }

    #[test]
    fn reopening_the_window_redoes_the_frame_work() {
        // 重新开启会创建新的 HWND：外观修正（frame_fixed）与顶部剪边（clip_px）
        // 两个"已完成"标记都必须清掉，否则新窗口既没有去边框、也会把白线带回来
        let ctx = egui::Context::default();
        let control = Arc::new(LyricControl::new());
        let mut window = LyricWindow::new(ctx, control.clone());
        window.open = true;
        window.shared.frame_fixed.store(true, Ordering::SeqCst);
        window.shared.clip_px.store(0x04E2_0096, Ordering::Relaxed);
        window.set_enabled(false);
        assert!(!window.shared.frame_fixed.load(Ordering::SeqCst));
        assert_eq!(window.shared.clip_px.load(Ordering::Relaxed), 0);
    }

    /// 渲染一帧并返回绘制出的形状（用于断言"0% 不画背景"）。
    fn render_shapes(
        ctx: &egui::Context,
        control: &LyricControl,
        shared: &Shared,
    ) -> Vec<egui::epaint::ClippedShape> {
        let frame = OwnedFrame {
            line: Some("line".into()),
            next: None,
            progress: 0.3,
            playing: true,
        };
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(BODY_SIZE[0], BODY_SIZE[1]),
            )),
            ..Default::default()
        };
        let out = ctx.run_ui(raw, |ui| {
            render_body(ui, &frame, control, shared, crate::i18n::Lang::Zh);
        });
        let shapes = out.shapes.clone();
        out.drop_without_applying_deltas();
        shapes
    }

    /// 与窗口等大的矩形数量（底板上、下描边都算）。
    fn backdrop_rects(shapes: &[egui::epaint::ClippedShape]) -> usize {
        shapes
            .iter()
            .filter(|s| match &s.shape {
                egui::Shape::Rect(r) => {
                    (r.rect.width() - BODY_SIZE[0]).abs() < 1.5
                        && (r.rect.height() - BODY_SIZE[1]).abs() < 1.5
                }
                _ => false,
            })
            .count()
    }

    #[test]
    fn window_renders_in_both_lock_states() {
        // 冒烟：单窗渲染走真实 egui 上下文，抓借用冲突/字体未就绪之类的 panic。
        let ctx = egui::Context::default();
        crate::gui::theme::setup_fonts(&ctx);
        let control = LyricControl::new();
        let shared = Shared::default();
        // 第一帧上传字体图集，之后的帧才是真正的布局
        let _ = render_shapes(&ctx, &control, &shared);
        for locked in [false, true] {
            control.set_locked(locked);
            let _ = render_shapes(&ctx, &control, &shared);
        }
    }

    #[test]
    fn zero_opacity_paints_no_backdrop_at_all() {
        // 需求：透明度最低时背景"完全无渲染"，只剩文字。
        let ctx = egui::Context::default();
        crate::gui::theme::setup_fonts(&ctx);
        let control = LyricControl::new();
        let shared = Shared::default();
        let _ = render_shapes(&ctx, &control, &shared); // 字体预热

        control.set_opacity(0);
        let shapes = render_shapes(&ctx, &control, &shared);
        assert_eq!(backdrop_rects(&shapes), 0, "0% 时不得绘制任何背景矩形");

        control.set_opacity(66);
        let shapes = render_shapes(&ctx, &control, &shared);
        assert!(backdrop_rects(&shapes) >= 1, "非 0% 时必须照常绘制底板");
    }

    #[test]
    fn default_position_docks_under_the_main_window() {
        let size = egui::vec2(BODY_SIZE[0], BODY_SIZE[1]);
        let main = egui::Rect::from_min_size(egui::pos2(120.0, 120.0), egui::vec2(1600.0, 900.0));
        // 屏幕（points）比主窗口下沿高得多 → 走"下沿外侧"档
        let monitor = egui::vec2(2200.0, 1600.0);
        let p = default_body_pos(Some(main), size, Some(monitor));
        // 水平居中于主窗口
        assert_eq!(p.x, main.center().x - size.x * 0.5);
        // 竖直贴主窗口下沿外侧，且整条都在屏幕内
        assert_eq!(p.y, main.bottom() + DOCK_GAP);
        assert!(p.y + size.y <= monitor.y, "下沿外侧档必须整条落在屏幕内");

        // 主窗口最大化（下沿已到屏幕底）→ 退回主窗口内侧底边上方
        let full = egui::vec2(1600.0, 900.0);
        let q = default_body_pos(Some(main), size, Some(full));
        assert!(q.y + size.y <= main.bottom(), "内侧档不得超出主窗口底边");
        assert!(q.y >= main.top());

        // 拿不到显示器尺寸 → 一律走内侧档（绝不冒"窗口落到屏幕外"的风险）
        let r = default_body_pos(Some(main), size, None);
        assert_eq!(r, q);

        // 主窗口比歌词条还窄时，至少不能给出负坐标
        let narrow = egui::Rect::from_min_size(egui::pos2(50.0, 50.0), egui::vec2(400.0, 300.0));
        let s = default_body_pos(Some(narrow), size, Some(monitor));
        assert!(s.x >= narrow.left() && s.y >= narrow.top());
        // 拿不到主窗口几何时退化成安全区域，绝不使用凭空的屏幕尺寸
        let unknown = default_body_pos(None, size, None);
        assert!(unknown.x >= 0.0 && unknown.y >= 0.0);
    }

    #[test]
    fn initial_position_is_computed_exactly_once() {
        // 反馈环防护：位置只在"还没有初始位置"时算一次，之后 register() 不再改写
        // （早先版本每帧把窗口实际位置回写再当期望位置下发，窗口会漂出屏幕）
        let s = Shared::default();
        assert!(s.initial_pos().is_none(), "首次进入时没有初始位置");
        s.set_initial_pos(egui::pos2(10.0, 20.0));
        assert_eq!(s.initial_pos(), Some(egui::pos2(10.0, 20.0)));
        // 再调 set 也不会被 register() 的守卫放行（守卫条件是 is_none）
        assert!(s.initial_pos().is_some());
    }

    #[test]
    fn the_window_is_docked_only_once() {
        // `docked` 保证首帧落位只下发一次，之后不再校正（否则每帧一次
        // WM_WINDOWPOSCHANGING，窗口会跟用户的拖动打架）
        let s = Shared::default();
        assert!(!s.docked.load(Ordering::SeqCst));
        s.docked.store(true, Ordering::SeqCst);
        assert!(s.docked.load(Ordering::SeqCst));
    }

    #[test]
    fn only_the_acid_style_uses_the_theme_accent() {
        // 需求：彩色条改成可选、非默认。这条约束必须钉在"样式→外观"这一层，
        // 否则以后谁改了默认值都没人拦。
        let line = progress_look(ProgressStyle::Line).expect("默认档必须画");
        let acid = progress_look(ProgressStyle::Acid).expect("彩色档必须画");
        assert_eq!(
            line.fill,
            egui::Color32::from_white_alpha(235),
            "默认档必须是白线"
        );
        assert_ne!(line.fill, pal().acid, "默认档绝不能是主题强调色");
        assert_eq!(acid.fill, pal().acid, "只有 Acid 档用主题强调色");
        assert_ne!(line.fill, acid.fill);
        // 关掉时整条不画
        assert!(progress_look(ProgressStyle::Off).is_none());
        // 三种档位的填充色两两不同，肉眼才分得出来
        assert_ne!(acid.fill, line.track);
    }

    /// 视口几何：`(inner, outer, 指针视口内坐标)` 三元组。
    fn geo(
        win: egui::Pos2,
        size: egui::Vec2,
        pointer: egui::Pos2,
    ) -> (egui::Rect, egui::Rect, egui::Pos2) {
        let r = egui::Rect::from_min_size(win, size);
        (r, r, pointer)
    }

    fn window_size() -> egui::Vec2 {
        egui::vec2(BODY_SIZE[0], BODY_SIZE[1])
    }

    #[test]
    fn a_press_alone_never_moves_the_window() {
        // 单击（只按下不移动）不得让窗口跳一下——否则点锁定按钮时窗口会"抖"
        let mut tracker = DragTracker::default();
        let (inner, outer, p) = geo(
            egui::pos2(100.0, 300.0),
            window_size(),
            egui::pos2(40.0, 12.0),
        );
        assert_eq!(
            tracker.step(true, Some(inner), Some(outer), Some(p)),
            DragStep::Idle
        );
        assert_eq!(
            tracker.step(true, Some(inner), Some(outer), Some(p)),
            DragStep::Idle
        );
        assert_eq!(
            tracker.anchor,
            Some((egui::pos2(140.0, 312.0), egui::pos2(100.0, 300.0)))
        );
    }

    #[test]
    fn dragging_moves_the_window_and_never_oscillates() {
        let mut tracker = DragTracker::default();
        let start = egui::pos2(100.0, 300.0);
        let size = window_size();

        // ① 按下：锚点取在指针屏幕坐标 (600,360)，窗口原地不动
        let (inner, outer, p) = geo(start, size, egui::pos2(500.0, 60.0));
        assert_eq!(
            tracker.step(true, Some(inner), Some(outer), Some(p)),
            DragStep::Idle
        );

        // ② 指针在屏幕上右移 30、下移 12（窗口还没动，所以视口内坐标同步变化）
        let (inner, outer, p) = geo(start, size, egui::pos2(530.0, 72.0));
        assert_eq!(
            tracker.step(true, Some(inner), Some(outer), Some(p)),
            DragStep::Move(egui::pos2(130.0, 312.0))
        );

        // ③ 窗口真的挪到 (130,312)，指针在屏幕上没动 → 视口内坐标退回 (500,60)。
        //    此时若按"每帧累加 pointer.delta()"来算，就会算出 -30/-12 把窗口拉回去，
        //    下一帧再拉回来——这就是自激振荡。屏幕坐标法在这里必须得出 Idle。
        let (inner, outer, p) = geo(egui::pos2(130.0, 312.0), size, egui::pos2(500.0, 60.0));
        assert_eq!(
            tracker.step(true, Some(inner), Some(outer), Some(p)),
            DragStep::Idle,
            "窗口被自己移动后，指针屏幕坐标不变就不该再下发位置"
        );

        // ④ 继续拖：屏幕坐标再右移 10 → 目标 140
        let (inner, outer, p) = geo(egui::pos2(130.0, 312.0), size, egui::pos2(510.0, 60.0));
        assert_eq!(
            tracker.step(true, Some(inner), Some(outer), Some(p)),
            DragStep::Move(egui::pos2(140.0, 312.0))
        );

        // ⑤ 松手：锚点清空，之后不再移动
        assert_eq!(
            tracker.step(false, Some(inner), Some(outer), Some(p)),
            DragStep::Idle
        );
        assert!(tracker.anchor.is_none() && tracker.last.is_none());
    }

    #[test]
    fn a_drag_can_start_from_any_corner() {
        // 从窗口右下角抓（而不是中间）也必须 1:1 跟随
        let mut tracker = DragTracker::default();
        let start = egui::pos2(0.0, 0.0);
        let size = window_size();
        let (inner, outer, p) = geo(start, size, egui::pos2(980.0, 110.0));
        assert_eq!(
            tracker.step(true, Some(inner), Some(outer), Some(p)),
            DragStep::Idle
        );
        let (inner, outer, p) = geo(start, size, egui::pos2(960.0, 90.0));
        assert_eq!(
            tracker.step(true, Some(inner), Some(outer), Some(p)),
            DragStep::Move(egui::pos2(-20.0, -20.0))
        );
    }

    #[test]
    fn missing_geometry_is_a_no_op_instead_of_a_jump() {
        // 最小化 / 显示器被拔掉时 inner_rect 是 None。此时宁可不动，
        // 也不能把窗口扔到 (0,0)。
        let mut tracker = DragTracker::default();
        assert_eq!(
            tracker.step(true, None, None, Some(egui::pos2(10.0, 10.0))),
            DragStep::Idle
        );
        assert_eq!(
            tracker.step(
                true,
                Some(egui::Rect::ZERO),
                None,
                Some(egui::pos2(10.0, 10.0))
            ),
            DragStep::Idle
        );
        assert!(tracker.anchor.is_none());
    }

    #[test]
    fn drag_target_is_pure_translation_of_the_pointer() {
        let start = egui::pos2(100.0, 200.0);
        let grab = egui::pos2(500.0, 400.0);
        assert_eq!(drag_target(start, grab, grab), start, "指针没动窗口就不动");
        assert_eq!(
            drag_target(start, grab, grab + egui::vec2(10.0, -6.0)),
            egui::pos2(110.0, 194.0)
        );
    }
}
