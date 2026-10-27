//! 登录面板（二维码 / 手机号 / Cookie 三方式，复用同一面板）。
//!
//! 设计取舍（与 `NETEASE_INTEGRATION_SPEC.md` 的凭据约束对齐）：
//! - **二维码**为主：密码 / 手机号不走第三方图片服务，二维码本地编码（[`super::qr`]）；
//! - **手机号**：密码只经 SDK 做 MD5 后发送，不落盘、不入日志；
//! - **Cookie**：用户粘贴的会话 Cookie 只进 SDK 内存客户端，不落盘、不入日志；
//! - 登录成功后会话 Cookie 由 `app` 侧**加密持久化**（见 `session_store`），
//!   重启后可恢复——三方式共用同一套「登录成功 → 持久化」链路；
//! - 状态机与 [`LoginState`] 对应，**不做乐观 UI**（没确认成功就显示未登录）。
//!
//! 本模块只做「状态 + 绘制 + 动作回传」，不直接调用 Provider——
//! 发请求的时机与节奏由 [`crate::gui::app`] 掌握，保持单向数据流。

use eframe::egui::{self, Align2, TextureId, Ui};

use crate::i18n::{pick, Lang};
use crate::music::provider::LoginState;

use super::theme::*;
use super::widgets;

/// 登录方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LoginMode {
    /// 扫码登录（默认）。
    #[default]
    Qr,
    /// 手机号 + 密码登录。
    Cellphone,
    /// 粘贴 Cookie 登录。
    Cookie,
}

impl LoginMode {
    /// 全部方式（切换菜单按此顺序排按钮）。
    pub const ALL: [LoginMode; 3] = [LoginMode::Qr, LoginMode::Cellphone, LoginMode::Cookie];

    /// 面板显示名。
    pub fn label(self, lang: Lang) -> &'static str {
        match self {
            LoginMode::Qr => pick(lang, "扫码", "QR"),
            LoginMode::Cellphone => pick(lang, "手机", "PHONE"),
            LoginMode::Cookie => pick(lang, "Cookie", "COOKIE"),
        }
    }
}

/// 面板动作（由 app 执行副作用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginAction {
    /// 无操作。
    None,
    /// 关闭面板。
    Close,
    /// 重新发起二维码（首次打开 / 过期 / 用户点刷新）。
    Start,
    /// 轮询指定 key 的扫码状态。
    Poll(String),
    /// 切换登录方式。
    SwitchMode(LoginMode),
    /// 用手机号 + 密码登录。
    LoginCellphone,
    /// 用粘贴的 Cookie 登录。
    LoginCookie,
    /// 退出登录。
    Logout,
}

/// 登录面板状态。
#[derive(Debug, Clone, Default)]
pub struct LoginUi {
    /// 面板是否可见。
    pub open: bool,
    /// 当前登录方式。
    pub mode: LoginMode,
    /// 当前展示的登录状态（来自 Provider，不本地推测）。
    pub state: Option<LoginState>,
    /// 二维码内容（Provider 返回的 `qr_url`）。
    pub qr_url: Option<String>,
    /// 轮询凭据。
    pub qr_key: Option<String>,
    /// 最近一次错误（展示在面板底部）。
    pub error: Option<String>,
    /// 是否已请求过 `Start`（避免每帧重复发起）。
    pub started: bool,
    /// 二维码纹理的**自然边长**（px，含静区）。0 = 尚无纹理。
    ///
    /// 绘制时按这个尺寸 1:1 落框，不做缩放——缩放会让模块宽度在整数像素间
    /// 跳动，手机摄像头容易误读。
    pub qr_side: f32,
    /// 手机号输入。
    pub phone: String,
    /// 密码输入。
    pub password: String,
    /// Cookie 输入。
    pub cookie_input: String,
}

impl LoginUi {
    /// 打开面板（不立即发起请求，由 app 在下一帧判断）。
    pub fn show(&mut self) {
        self.open = true;
    }

    /// 关闭面板并清空瞬时状态（保留已登录状态，账号信息不因关面板丢失）。
    pub fn hide(&mut self) {
        self.open = false;
        self.error = None;
    }

    /// 已登录（展示用；`None` 表示尚未查询过）。
    ///
    /// 服务端没给昵称时用 `lang` 决定占位文案——这里也走界面语言，
    /// 否则英文界面会出现 `SIGNED IN · 已登录`。
    pub fn logged_in(&self, lang: Lang) -> Option<&str> {
        match &self.state {
            Some(LoginState::LoggedIn { nickname, .. }) => Some(
                nickname
                    .as_deref()
                    .unwrap_or_else(|| pick(lang, "已登录", "signed in")),
            ),
            _ => None,
        }
    }

    /// 是否处于「需要重新扫码」的状态。
    ///
    /// 只有**二维码正在生效**（等待扫码 / 已扫码待确认）或**已登录**时才算
    /// 不需要重开；其余一律需要。
    ///
    /// ⚠ 这里必须包含 [`LoginState::Anonymous`]：启动时会查一次登录状态，
    /// 未登录时拿到 `Anonymous`。若把它当成「不需要重开」，打开面板就永远
    /// 不会去取二维码——面板停在 `REQUESTING…`，用户扫码无从谈起
    /// （历史 bug：扫码无法正常登录）。
    pub fn needs_restart(&self) -> bool {
        matches!(
            self.state,
            None | Some(LoginState::Anonymous) | Some(LoginState::QrExpired)
        ) || self.error.is_some()
    }

    /// 状态文案（跟界面语言）。
    ///
    /// `None`（还没拿到第一轮轮询结果）单独给一句，不借用
    /// [`LoginState`] 的任何变体——那个空窗期既不是"未登录"也不是"过期"。
    pub fn status_text(&self, lang: Lang) -> String {
        match &self.state {
            None => pick(lang, "准备中…", "Preparing…").to_string(),
            Some(s) => s.message(lang),
        }
    }
}

/// 绘制登录面板（居中模态）。返回用户动作。
pub fn login_panel(
    ctx: &egui::Context,
    state: &mut LoginUi,
    qr_texture: Option<TextureId>,
    lang: Lang,
) -> LoginAction {
    if !state.open {
        return LoginAction::None;
    }
    let mut action = LoginAction::None;

    // 背景压暗（不拦截点击由 Area 自身负责：面板外的点击落到下层，按 Esc 关闭）
    let screen = ctx.content_rect();
    egui::Area::new(egui::Id::unique("reimu-player-login-scrim"))
        .order(egui::Order::Middle)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            // 遮罩必须**压暗**：深色模式下若跟着 ink 走反而会提亮背景
            ui.painter()
                .rect_filled(screen, radius_none(), pal().modal_dim);
        });

    let panel_w = 348.0;
    let panel_h = 452.0;
    egui::Area::new(egui::Id::unique("reimu-player-login"))
        .order(egui::Order::Foreground)
        .anchor(Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(panel_w, panel_h), egui::Sense::hover());
            widgets::panel_at(ui, rect, |ui| {
                widgets::panel_head(
                    ui,
                    pick(lang, "Login / 登录", "Login"),
                    pick(lang, "安全登录", "SECURE LOGIN"),
                );
                ui.add_space(6.0);

                // ---- 左上角：登录方式切换菜单（复用当前面板，不另建页面）----
                ui.horizontal(|ui| {
                    for mode in LoginMode::ALL {
                        if widgets::tiny_button(
                            ui,
                            ui.scope_id().with("login-mode").with(mode as u8),
                            mode.label(lang),
                            state.mode == mode,
                        )
                        .clicked()
                            && state.mode != mode
                        {
                            action = LoginAction::SwitchMode(mode);
                        }
                        ui.add_space(4.0);
                    }
                });
                ui.add_space(8.0);

                // ---- 内容区：按方式分发（已登录统一显示昵称 + 退出）----
                if let Some(name) = state.logged_in(lang).map(str::to_string) {
                    draw_logged_in(ui, &name, &mut action, lang);
                } else {
                    match state.mode {
                        LoginMode::Qr => draw_qr(ui, state, qr_texture, lang, &mut action),
                        LoginMode::Cellphone => draw_cellphone(ui, state, lang, &mut action),
                        LoginMode::Cookie => draw_cookie(ui, state, lang, &mut action),
                    }
                }

                ui.add_space(10.0);
                // ---- 状态行 ----
                let status = state.status_text(lang);
                let color = if state.logged_in(lang).is_some() {
                    pal().ink
                } else {
                    pal().mid
                };
                ui.label(widgets::spec_job(
                    &status,
                    FONT_TINY,
                    TRACKING_TINY,
                    color,
                    true,
                ));

                if let Some(err) = &state.error {
                    ui.add_space(4.0);
                    ui.label(widgets::spec_job(
                        &format!("{}: {err}", pick(lang, "错误", "ERR")),
                        FONT_TINY,
                        TRACKING_TINY,
                        pal().error_mark,
                        true,
                    ));
                }

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if state.logged_in(lang).is_some() {
                        // 已登录：左 = 退出登录，右 = 关闭
                        if widgets::text_button(
                            ui,
                            ui.scope_id().with("login-logout"),
                            pick(lang, "退出登录", "LOG OUT"),
                            true,
                            false,
                        )
                        .clicked()
                        {
                            action = LoginAction::Logout;
                        }
                    } else if state.mode == LoginMode::Qr {
                        // 扫码：刷新 + 关闭
                        let refresh = pick(lang, "换一张", "NEW CODE");
                        if widgets::text_button(
                            ui,
                            ui.scope_id().with("login-refresh"),
                            refresh,
                            true,
                            false,
                        )
                        .clicked()
                        {
                            action = LoginAction::Start;
                        }
                    }
                    ui.add_space(8.0);
                    if widgets::text_button(
                        ui,
                        ui.scope_id().with("login-close"),
                        pick(lang, "关闭", "CLOSE"),
                        false,
                        false,
                    )
                    .clicked()
                    {
                        action = LoginAction::Close;
                    }
                });

                // 底部说明：登录态会加密持久化，不再"只在内存"
                ui.add_space(10.0);
                for line in [
                    pick(
                        lang,
                        "登录后会话将加密保存在本机，重启无需重复登录",
                        "Your session is stored encrypted on this device — no re-login after restart",
                    ),
                    pick(
                        lang,
                        "本程序不保存账号密码，Cookie 仅用于网易云登录",
                        "No passwords are stored — the cookie is used only for NetEase sign-in",
                    ),
                ] {
                    ui.label(widgets::spec_job(line, FONT_TINY, 0.0, pal().mid, false));
                    ui.add_space(2.0);
                }
            });
        });

    // Esc 关闭
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        action = LoginAction::Close;
    }
    action
}

/// 二维码绘制框边长（px）。
///
/// 对外公开：`app` 生成二维码纹理时按这个宽度反算**整数**模块像素
/// （见 [`crate::gui::app`] 的 `start_qr_login` 响应处理），保证纹理能
/// 1:1 落在框里、模块宽度不出现 3px/4px 混排。
pub const QR_BOX_SIDE: f32 = 196.0;

/// 二维码区（196×196 纹理 + 四角标记）。
fn draw_qr(
    ui: &mut Ui,
    state: &LoginUi,
    qr_texture: Option<TextureId>,
    lang: Lang,
    action: &mut LoginAction,
) {
    const SIDE: f32 = QR_BOX_SIDE;
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), SIDE), egui::Sense::hover());
    let qr_rect = egui::Rect::from_center_size(rect.center(), egui::vec2(SIDE, SIDE));

    // 底纸 + 描边（二维码必须有白底，否则扫不出来）
    ui.painter()
        .rect_filled(qr_rect, radius_none(), egui::Color32::WHITE);
    ui.painter().rect_stroke(
        qr_rect,
        radius_none(),
        stroke_component(),
        egui::StrokeKind::Inside,
    );

    match qr_texture {
        Some(tex) => {
            // **1:1 绘制**，不做缩放。纹理宽度已按整数模块尺寸算好（≤ 框宽），
            // 一旦交给 painter 缩放，最近邻采样会让模块宽度忽 3 忽 4 px，
            // 手机摄像头容易误读——这是"二维码看着正常但扫不出来"的常见成因。
            let natural = if state.qr_side > 1.0 {
                state.qr_side
            } else {
                SIDE
            };
            let side = natural.min(SIDE);
            let img_rect = egui::Rect::from_center_size(qr_rect.center(), egui::vec2(side, side));
            ui.painter().image(
                tex,
                img_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
        None => {
            // 无二维码：占位 + 提示（不是"假装加载中"）
            widgets::paint_spec_text(
                ui,
                qr_rect.center(),
                Align2::CENTER_CENTER,
                if state.error.is_some() {
                    pick(lang, "二维码不可用", "QR UNAVAILABLE")
                } else {
                    pick(lang, "正在获取…", "REQUESTING…")
                },
                FONT_LABEL,
                TRACKING_LABEL,
                pal().mid,
                true,
                true,
            );
        }
    }

    // 四角定位标记（复古扫描框语言）
    let arm = 14.0;
    for (corner, dx, dy) in [
        (qr_rect.left_top(), 1.0, 1.0),
        (qr_rect.right_top(), -1.0, 1.0),
        (qr_rect.left_bottom(), 1.0, -1.0),
        (qr_rect.right_bottom(), -1.0, -1.0),
    ] {
        let h = egui::Rect::from_min_size(
            egui::pos2(corner.x.min(corner.x + dx * arm), corner.y - 1.0),
            egui::vec2(arm, 2.0),
        );
        let v = egui::Rect::from_min_size(
            egui::pos2(corner.x - 1.0, corner.y.min(corner.y + dy * arm)),
            egui::vec2(2.0, arm),
        );
        ui.painter()
            .rect_filled(h, radius_none(), fg_on(egui::Color32::WHITE));
        ui.painter()
            .rect_filled(v, radius_none(), fg_on(egui::Color32::WHITE));
    }

    // 过期/出错时给一个显眼的重新获取入口
    if state.needs_restart() && qr_texture.is_some() {
        ui.painter().rect_filled(
            qr_rect.shrink(6.0),
            radius_none(),
            widgets::with_opacity(pal().paper, 0.86),
        );
        let (btn_rect, _) = ui.allocate_exact_size(egui::vec2(1.0, 1.0), egui::Sense::hover());
        let _ = btn_rect;
        if widgets::text_button(
            ui,
            ui.scope_id().with("qr-reload"),
            pick(lang, "换一张", "NEW CODE"),
            true,
            false,
        )
        .clicked()
        {
            *action = LoginAction::Start;
        }
    }
}

/// 手机号 + 密码输入区。
fn draw_cellphone(ui: &mut Ui, state: &mut LoginUi, lang: Lang, action: &mut LoginAction) {
    const SIDE: f32 = 196.0;
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), SIDE), egui::Sense::hover());
    let box_rect = egui::Rect::from_center_size(rect.center(), egui::vec2(SIDE, SIDE));
    ui.painter()
        .rect_filled(box_rect, radius_none(), pal().panel_alt);
    ui.painter().rect_stroke(
        box_rect,
        radius_none(),
        stroke_component(),
        egui::StrokeKind::Inside,
    );

    ui.scope_builder(
        egui::UiBuilder::new().max_rect(box_rect.shrink(14.0)),
        |ui| {
            ui.add_space(18.0);
            ui.label(widgets::spec_job(
                pick(lang, "手机号", "PHONE"),
                FONT_TINY,
                TRACKING_TINY,
                pal().ink,
                true,
            ));
            ui.add_space(4.0);
            let paste_label = pick(lang, "粘贴", "PASTE");
            let phone_edit = ui.add(
                egui::TextEdit::singleline(&mut state.phone)
                    .hint_text(pick(lang, "+86 手机号", "+86 phone"))
                    .desired_width(ui.available_width()),
            );
            super::clipboard::attach_paste_menu(&phone_edit, paste_label, &mut state.phone);
            ui.add_space(12.0);
            ui.label(widgets::spec_job(
                pick(lang, "密码", "PASSWORD"),
                FONT_TINY,
                TRACKING_TINY,
                pal().ink,
                true,
            ));
            ui.add_space(4.0);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut state.password)
                    .password(true)
                    .hint_text(pick(lang, "网易云密码", "NetEase password"))
                    .desired_width(ui.available_width()),
            );
            // 密码框尤其需要粘贴：`password(true)` 只是显示成圆点，内容照常可粘
            super::clipboard::attach_paste_menu(&resp, paste_label, &mut state.password);
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.add_space(12.0);
            if widgets::text_button(
                ui,
                ui.scope_id().with("login-cellphone"),
                pick(lang, "登录", "LOG IN"),
                true,
                false,
            )
            .clicked()
                || enter
            {
                *action = LoginAction::LoginCellphone;
            }
        },
    );
}

/// Cookie 输入区。
fn draw_cookie(ui: &mut Ui, state: &mut LoginUi, lang: Lang, action: &mut LoginAction) {
    const SIDE: f32 = 196.0;
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), SIDE), egui::Sense::hover());
    let box_rect = egui::Rect::from_center_size(rect.center(), egui::vec2(SIDE, SIDE));
    ui.painter()
        .rect_filled(box_rect, radius_none(), pal().panel_alt);
    ui.painter().rect_stroke(
        box_rect,
        radius_none(),
        stroke_component(),
        egui::StrokeKind::Inside,
    );

    ui.scope_builder(
        egui::UiBuilder::new().max_rect(box_rect.shrink(14.0)),
        |ui| {
            ui.add_space(18.0);
            ui.label(widgets::spec_job(
                pick(lang, "会话 Cookie", "SESSION COOKIE"),
                FONT_TINY,
                TRACKING_TINY,
                pal().ink,
                true,
            ));
            ui.add_space(4.0);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut state.cookie_input)
                    .hint_text("MUSIC_U=…; __csrf=…")
                    .desired_width(ui.available_width()),
            );
            // Cookie 是一条几百字符的长串，右键粘贴比手打现实得多
            super::clipboard::attach_paste_menu(
                &resp,
                pick(lang, "粘贴", "PASTE"),
                &mut state.cookie_input,
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.add_space(6.0);
            for line in [
                pick(
                    lang,
                    "从浏览器登录网易云后复制 Cookie 粘贴到这里",
                    "Copy your NetEase cookie from a logged-in browser and paste it here",
                ),
                pick(
                    lang,
                    "Cookie 只用于登录，加密保存、不外传",
                    "Used for sign-in only — stored encrypted, never shared",
                ),
            ] {
                ui.label(widgets::spec_job(line, FONT_TINY, 0.0, pal().mid, false));
                ui.add_space(2.0);
            }
            ui.add_space(8.0);
            if widgets::text_button(
                ui,
                ui.scope_id().with("login-cookie"),
                pick(lang, "登录", "LOG IN"),
                true,
                false,
            )
            .clicked()
                || enter
            {
                *action = LoginAction::LoginCookie;
            }
        },
    );
}

/// 已登录态：昵称 + 说明（不展示任何凭据）。
fn draw_logged_in(ui: &mut Ui, name: &str, action: &mut LoginAction, lang: Lang) {
    const SIDE: f32 = 196.0;
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), SIDE), egui::Sense::hover());
    let box_rect = egui::Rect::from_center_size(rect.center(), egui::vec2(SIDE, SIDE));
    ui.painter()
        .rect_filled(box_rect, radius_none(), pal().panel_alt);
    ui.painter().rect_stroke(
        box_rect,
        radius_none(),
        stroke_component(),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        box_rect.center() - egui::vec2(0.0, 8.0),
        Align2::CENTER_CENTER,
        pick(lang, "已登录", "LOGGED IN"),
        egui::FontId::new(FONT_TINY, egui::FontFamily::Proportional),
        pal().mid,
    );
    ui.painter().text(
        box_rect.center() + egui::vec2(0.0, 14.0),
        Align2::CENTER_CENTER,
        name,
        egui::FontId::new(FONT_TRACK_TITLE, egui::FontFamily::Proportional),
        pal().ink,
    );
    let _ = action;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logged_in_detects_nickname() {
        let mut ui = LoginUi::default();
        assert!(ui.logged_in(Lang::Zh).is_none());
        ui.state = Some(LoginState::LoggedIn {
            nickname: Some("耳朵".into()),
            user_id: None,
        });
        assert_eq!(ui.logged_in(Lang::Zh), Some("耳朵"));
    }

    #[test]
    fn logged_in_without_nickname_falls_back() {
        let ui = LoginUi {
            state: Some(LoginState::LoggedIn {
                nickname: None,
                user_id: Some("1".into()),
            }),
            ..LoginUi::default()
        };
        assert_eq!(ui.logged_in(Lang::Zh), Some("已登录"));
        assert_eq!(ui.logged_in(Lang::En), Some("signed in"));
    }

    #[test]
    fn needs_restart_on_expired_or_error() {
        let mut ui = LoginUi::default();
        assert!(ui.needs_restart(), "未开始时应可发起");
        ui.state = Some(LoginState::QrWaiting);
        assert!(!ui.needs_restart(), "等待扫码不需要重开");
        ui.state = Some(LoginState::QrExpired);
        assert!(ui.needs_restart());
        ui.state = Some(LoginState::QrWaiting);
        ui.error = Some("网络不可用".into());
        assert!(ui.needs_restart());
    }

    #[test]
    fn anonymous_must_restart_so_qr_is_fetched() {
        // 回归：启动时查登录状态会得到 Anonymous。若它不算「需要重开」，
        // 打开面板就不会去取二维码，永远停在 REQUESTING…（扫码无法登录）。
        let ui = LoginUi {
            state: Some(LoginState::Anonymous),
            ..LoginUi::default()
        };
        assert!(ui.needs_restart(), "匿名态必须触发取码");
    }

    #[test]
    fn active_qr_and_logged_in_do_not_restart() {
        for state in [LoginState::QrWaiting, LoginState::QrScanned] {
            let ui = LoginUi {
                state: Some(state.clone()),
                ..LoginUi::default()
            };
            assert!(!ui.needs_restart(), "{state} 时不该重发二维码");
        }
        let ui = LoginUi {
            state: Some(LoginState::LoggedIn {
                nickname: Some("n".into()),
                user_id: None,
            }),
            ..LoginUi::default()
        };
        assert!(!ui.needs_restart(), "已登录不该重发二维码");
    }

    #[test]
    fn hide_keeps_login_state() {
        let mut ui = LoginUi {
            state: Some(LoginState::LoggedIn {
                nickname: Some("n".into()),
                user_id: None,
            }),
            ..LoginUi::default()
        };
        ui.open = true;
        ui.error = Some("x".into());
        ui.hide();
        assert!(!ui.open);
        assert!(ui.error.is_none(), "瞬时错误要清掉");
        assert!(ui.logged_in(Lang::Zh).is_some(), "登录态不能因关面板丢失");
    }

    #[test]
    fn login_modes_cover_all_three_and_default_to_qr() {
        assert_eq!(LoginMode::default(), LoginMode::Qr);
        assert_eq!(LoginMode::ALL.len(), 3);
        let mut seen = std::collections::HashSet::new();
        for m in LoginMode::ALL {
            seen.insert(m);
        }
        assert_eq!(seen.len(), 3, "三种登录方式都必须在切换菜单里");
    }
}
