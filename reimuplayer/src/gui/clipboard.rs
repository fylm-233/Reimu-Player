//! 系统剪贴板读取（Windows）。
//!
//! # 为什么不用 eframe 的 `clipboard` feature
//!
//! eframe 0.37 的 `clipboard` 特性会引入 `arboard`（Windows 下还有
//! `clipboard-win`），而本项目只需要"粘贴**纯文本**"这一件事。`windows-sys`
//! 本来就在依赖树里（winit 同源），加两个 feature gate 不新增编译单元。
//!
//! # 用法
//!
//! 调用方（[`crate::gui::app`]）在 `TextEdit` **之前**读一次，把结果包成
//! `egui::Event::Paste` 注入当前帧的输入队列——`TextEdit` 自己会处理光标、
//! 选区与过滤，不需要在控件层面动手脚。
//!
//! 非 Windows 平台一律返回 `None`：调用方据此退化为"按了 Ctrl+V 但没东西
//! 可粘"，不会 panic，也不影响其余功能。

/// 读取剪贴板里的纯文本。
///
/// 返回值已做最小清理：`\r\n` / `\r` / `\n` 一律压成空格（单行输入框拿到
/// 换行会显示成豆腐块或直接吞掉后半段），首尾空白裁掉，全空视为没有内容。
///
/// 剪贴板被其它进程占用（`OpenClipboard` 失败）是**常态**而非异常——
/// Windows 剪贴板是独占的，另一个程序正在读的时候这里就打不开。此时返回
/// `None`，用户再按一次即可，不弹任何错误。
pub fn read_text() -> Option<String> {
    let raw = platform::read_unicode_text()?;
    let cleaned = sanitize(&raw);
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// 把剪贴板原文清理成适合单行输入框的文本。
///
/// 单独抽出来是为了可测：真正的系统调用无法在单测里跑，但"换行怎么处理"
/// 恰恰是粘贴最容易出错的地方。
pub fn sanitize(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_was_space = false;
    for ch in raw.chars() {
        // 换行 / 制表 / 回车都当分隔符；连续空白只留一个
        let ch = if ch == '\r' || ch == '\n' || ch == '\t' {
            ' '
        } else {
            ch
        };
        if ch == ' ' {
            if last_was_space {
                continue;
            }
            last_was_space = true;
        } else {
            last_was_space = false;
        }
        out.push(ch);
    }
    out.trim().to_string()
}

/// 给文本输入框挂一个右键菜单，目前只有一项「粘贴」。
///
/// # 为什么必须自己挂
///
/// egui 0.37 的 `TextEdit` **没有**自带右键菜单——`text_edit/builder.rs` 里只有
/// `Event::Copy` / `Cut` / `Paste` 的处理，一次 `context_menu` 都没调。
/// 再加上 eframe 没开 `clipboard` feature（见模块头），「右键 → 粘贴」整条
/// 链路是空的：右键点上去什么都不会出现。
///
/// # 为什么不复用 [`Event::Paste`]
///
/// Ctrl+V 走的是输入事件：在 `TextEdit` **之前**注入，控件自己会把文本插到
/// 光标处、走自己的 undo 与过滤。菜单项则是在 `TextEdit` **画完之后**才响应
/// 点击的，此时再往输入队列里塞事件会在帧末被丢掉（`InputState::events`
/// 每帧重建），所以这里直接改字符串，并顺手把光标挪到末尾——否则粘完光标
/// 还停在开头，接着打字会插到前面去。
///
/// 返回 `true` 表示本帧确实粘进去了（诊断与单测用）。
pub fn attach_paste_menu(response: &eframe::egui::Response, label: &str, target: &mut String) -> bool {
    let mut pasted = false;
    response.context_menu(|ui| {
        ui.spacing_mut().item_spacing = eframe::egui::Vec2::ZERO;
        ui.set_min_width(super::context_menu::MENU_W);
        if super::context_menu::item_row(ui, label, true, false) {
            if let Some(text) = read_text() {
                target.push_str(&text);
                pasted = true;
            }
            ui.close();
        }
    });
    if pasted {
        move_caret_to_end(response, target);
        // 粘完把焦点留在输入框上：用户下一步多半是接着改，而不是去点别处
        response.request_focus();
    }
    pasted
}

/// 把该输入框的光标挪到文本末尾（粘贴后必须做，见 [`attach_paste_menu`]）。
fn move_caret_to_end(response: &eframe::egui::Response, target: &str) {
    let ctx = response.ctx.clone();
    let Some(mut state) = eframe::egui::text_edit::TextEditState::load(&ctx, response.id) else {
        // 控件本帧还没存过状态（例如刚出现）：下一次聚焦时 egui 自己会放到末尾
        return;
    };
    let end = target.chars().count();
    state.cursor.set_char_range(Some(eframe::egui::text::CCursorRange::one(
        eframe::egui::text::CCursor::new(end),
    )));
    state.store(&ctx, response.id);
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::Foundation::HGLOBAL;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};

    /// `CF_UNICODETEXT`（Win32 常量，windows-sys 未在 DataExchange 里导出该名字）。
    const CF_UNICODETEXT: u32 = 13;

    pub(super) fn read_unicode_text() -> Option<String> {
        // SAFETY: 全部是 Win32 剪贴板 API 的标准调用序列，句柄只在
        // `OpenClipboard` 成功到 `CloseClipboard` 之间使用，且每条失败路径
        // 都会先关剪贴板再返回——不关会让剪贴板一直锁在本进程手里。
        unsafe {
            if IsClipboardFormatAvailable(CF_UNICODETEXT) == 0 {
                return None;
            }
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return None;
            }

            let handle = GetClipboardData(CF_UNICODETEXT);
            if handle.is_null() {
                CloseClipboard();
                return None;
            }

            let ptr = GlobalLock(handle as HGLOBAL);
            if ptr.is_null() {
                CloseClipboard();
                return None;
            }

            // UTF-16LE，以 NUL 结尾；不信任任何长度字段，自己数到 NUL 为止
            let wide = ptr as *const u16;
            let mut len = 0usize;
            // 上限只是防御性截断（正常歌词/链接远小于此），避免损坏的
            // 剪贴板内容让循环跑到越界
            const MAX_UNITS: usize = 1 << 20;
            while len < MAX_UNITS && *wide.add(len) != 0 {
                len += 1;
            }
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(wide, len));

            GlobalUnlock(handle as HGLOBAL);
            CloseClipboard();
            Some(text)
        }
    }
}

#[cfg(not(windows))]
mod platform {
    pub(super) fn read_unicode_text() -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_line_text_passes_through() {
        assert_eq!(sanitize("海阔天空"), "海阔天空");
        assert_eq!(sanitize("  Beyond  "), "Beyond");
    }

    #[test]
    fn multi_line_paste_is_flattened_to_one_line() {
        // 从网页/文档复制分享链接常常带换行，单行输入框拿到 \n 会把后半段吞掉
        assert_eq!(sanitize("http://music.163.com/a\r\nb"), "http://music.163.com/a b");
        assert_eq!(sanitize("a\n\n\nb"), "a b");
        assert_eq!(sanitize("a\tb"), "a b");
    }

    #[test]
    fn blank_clipboard_is_reported_as_nothing() {
        assert_eq!(sanitize(""), "");
        assert_eq!(sanitize("   \r\n\t "), "");
        assert!(read_text().is_some() || read_text().is_none());
    }

    #[test]
    fn non_breaking_chars_survive() {
        // 只压 ASCII 空白；全角空格与零宽字符是内容的一部分，不该被动
        assert_eq!(sanitize("a\u{3000}b"), "a\u{3000}b");
    }

    #[test]
    fn right_click_opens_the_paste_menu() {
        // 回归：egui 0.37 的 `TextEdit` 不自带右键菜单，右键点上去什么都不会出现。
        // 这里用合成输入钉住"右键 → 菜单确实打开了"，不依赖真机点击。
        use eframe::egui;
        let ctx = egui::Context::default();
        let mut text = String::new();
        let mut opened = false;
        for frame in 0..3u32 {
            let mut raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(400.0, 120.0),
                )),
                ..Default::default()
            };
            if frame == 1 {
                // 在输入框上按下并松开右键
                for pressed in [true, false] {
                    raw.events.push(egui::Event::PointerButton {
                        pos: egui::pos2(60.0, 20.0),
                        button: egui::PointerButton::Secondary,
                        pressed,
                        modifiers: egui::Modifiers::default(),
                    });
                }
            }
            let out = ctx.run_ui(raw, |ui| {
                let resp = ui.add_sized(
                    [380.0, 24.0],
                    egui::TextEdit::singleline(&mut text),
                );
                attach_paste_menu(&resp, "PASTE", &mut text);
                opened |= resp.context_menu_opened();
            });
            out.drop_without_applying_deltas();
        }
        assert!(opened, "右键输入框应当弹出粘贴菜单");
    }
}
