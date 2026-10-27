//! 让某个视口窗口"点得穿"，同时保住逐像素透明（圆角四角真的透出桌面）。
//!
//! ## 背景：两件互相打架的事
//!
//! 1. **鼠标穿透**要 `WS_EX_TRANSPARENT`，而 Windows 只在窗口**同时**带
//!    `WS_EX_LAYERED` 时才把它纳入"命中测试跳过"的路径（阶段 16 实测：只加
//!    `WS_EX_TRANSPARENT`，`WindowFromPoint` 依旧返回歌词窗本身，点击照样被吞）。
//! 2. **分层窗口默认 alpha = 0**。`WS_EX_LAYERED` 一旦被加上，DWM 就等应用
//!    用 `SetLayeredWindowAttributes` / `UpdateLayeredWindow` 告诉它怎么合成；
//!    两个都没调用时，整窗按全透明处理——渲染完全正常（`PrintWindow` 能拍到
//!    文字），屏幕上却**一片空白**。这正是"桌面歌词打开了但看不见"的真凶，
//!    也是 winit `set_cursor_hittest(false)` 的副作用（它顺手加了 LAYERED，
//!    见 winit-0.30.13 `platform_impl/windows/window_state.rs:301`）。
//!
//! ## 解法：分层之后，把 blur-behind 重新打开
//!
//! 透明 OpenGL 窗口在 Windows 上的标准配方是"`WS_EX_LAYERED` +
//! `DwmEnableBlurBehindWindow`（空区域）"——两者齐备时 DWM 才会拿交换链的
//! alpha 通道做逐像素合成。winit 在窗口创建时设过一次，但**加 LAYERED 是
//! 后来才发生的事**，那一步会把窗口切进"等应用给分层属性"的状态。
//! 所以这里在加完 LAYERED 之后再补一次 `DwmEnableBlurBehindWindow`，
//! 把它拉回逐像素合成。
//!
//! 依赖说明：`windows-sys` 是既有直接依赖（媒体键用），本模块用到已启用的
//! `Win32_Foundation` / `Win32_System_Threading` / `Win32_UI_WindowsAndMessaging`，
//! 以及为透明悬浮窗新增的 `Win32_Graphics_Dwm` / `Win32_Graphics_Gdi`。
//! 同一个 crate、只是多编几个绑定，**不新增编译单元**。

/// 把标题为 `title` 的本进程窗口设为「鼠标穿透 + 逐像素透明」。返回是否成功。
///
/// 找不到窗口（还没创建 / 标题被系统改写）时返回 `false`，调用方下一帧重试即可。
#[cfg(windows)]
pub fn set_click_through(title: &str) -> bool {
    imp::set_click_through(title)
}

/// 非 Windows：桌面歌词窗口本身就不是这套 Win32 语义，直接视为成功。
#[cfg(not(windows))]
pub fn set_click_through(_title: &str) -> bool {
    true
}

/// 只修正透明悬浮窗**外观**（去 DWM 边框 / 投影 / 系统圆角），**不改命中测试**。
///
/// 为什么需要单独一个入口：歌词窗现在是单视口，锁定按钮与拖动面都在窗内，
/// **必须能收鼠标**——用 [`set_click_through`] 会加 `WS_EX_TRANSPARENT`，
/// 整窗点不中，锁定按钮就废了。但"别让 DWM 画边框和阴影"这件事与命中测试
/// 无关，两种窗口都要做。
///
/// 返回是否成功找到并处理了窗口（窗口还没创建时 `false`，调用方下一帧重试）。
#[cfg(windows)]
pub fn fix_transparent_frame(title: &str) -> bool {
    imp::fix_transparent_frame(title)
}

/// 非 Windows：无 DWM 语义，直接视为成功。
#[cfg(not(windows))]
pub fn fix_transparent_frame(_title: &str) -> bool {
    true
}

/// 把窗口顶部那 1px 从可见区域里剪掉。
///
/// Windows 10 的 DWM 会在窗口**客户区顶端**压一条 1px 高亮描边（激活时纯白、
/// 失焦后浅蓝灰）。它不画在应用的 framebuffer 里（`request_screenshot` 拍到的
/// 首行是底板像素），`PrintWindow` 与屏幕都能拍到——桌面歌词低透明度时上方
/// 那条"白色细线"就是它。Win10 没有 `DWMWA_BORDER_COLOR`（本机 19044 实测
/// 返回 `E_INVALIDARG`），清窗口样式、`DWMWA_NCRENDERING_POLICY=DISABLED`、
/// 换 `WS_POPUP`、加 `WS_EX_LAYERED` 都拦不住；实测唯一有效的是 `SetWindowRgn`：
/// 把第 0 行从窗口区域里挖掉，DWM 的描边随之被裁剪（连同命中测试一起，
/// 1px 的顶部边缘点不到，其余区域不受影响）。
///
/// 区域按**物理像素**计、不随 DPI 缩放，窗口尺寸变化后调用方必须重剪。
/// 返回是否成功（窗口还没创建时 `false`，调用方下次重试）。
#[cfg(windows)]
pub fn clip_top_border(title: &str) -> bool {
    imp::clip_top_border(title)
}

/// 非 Windows：没有 DWM 描边可剪，直接视为成功。
#[cfg(not(windows))]
pub fn clip_top_border(_title: &str) -> bool {
    true
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, RECT, TRUE};
    use windows_sys::Win32::Graphics::Dwm::{
        DwmEnableBlurBehindWindow, DwmSetWindowAttribute, DWMNCRP_DISABLED,
        DWMWA_NCRENDERING_POLICY, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
        DWM_BB_BLURREGION, DWM_BB_ENABLE, DWM_BLURBEHIND,
    };
    use windows_sys::Win32::Graphics::Gdi::{CreateRectRgn, DeleteObject, SetWindowRgn};
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClientRect, GetWindowLongPtrW, GetWindowTextW, GetWindowThreadProcessId,
        SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, GWL_STYLE, SWP_FRAMECHANGED, SWP_NOMOVE,
        SWP_NOSIZE, SWP_NOZORDER, WS_CAPTION, WS_EX_LAYERED, WS_EX_TRANSPARENT, WS_SYSMENU,
        WS_THICKFRAME,
    };

    /// 枚举时用的上下文（通过 `lparam` 传进去，避免全局可变状态）。
    struct Search<'a> {
        title: &'a str,
        found: HWND,
    }

    unsafe extern "system" fn on_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` 是 `set_click_through` 里那个 `&mut Search` 的地址，
        // 枚举在同一个栈帧内同步完成，生命周期足够。
        let search = unsafe { &mut *(lparam as *mut Search<'_>) };

        let mut pid = 0u32;
        // SAFETY: 标准 Win32 调用，hwnd 来自 EnumWindows，pid 是本地变量。
        if unsafe { GetWindowThreadProcessId(hwnd, &mut pid) } == 0 {
            return TRUE;
        }
        // SAFETY: 同上；只比较数值，不碰句柄内容。
        if pid != unsafe { GetCurrentProcessId() } {
            return TRUE;
        }

        let mut buf = [0u16; 128];
        // SAFETY: buf 长度如实传入，函数最多写 buf.len() 个 u16。
        let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
        if n <= 0 {
            return TRUE;
        }
        if String::from_utf16_lossy(&buf[..n as usize]) == search.title {
            search.found = hwnd;
            return 0; // 找到就停，别再无谓地枚举下去
        }
        TRUE
    }

    /// 修正透明悬浮窗的**外观**：让 DWM 不再画边框、投影与系统圆角。
    ///
    /// 与 [`apply`] 分开是必要的：歌词窗现在是单视口，锁定按钮与拖动面都在
    /// 窗内，**必须能收鼠标**，不能加 `WS_EX_TRANSPARENT`。但"去边框去阴影"
    /// 这件事与命中测试无关，两种窗口都需要。
    ///
    /// 四步各有各的坑：
    ///
    /// 1. **清窗口样式里的边框位**：`WS_THICKFRAME` 还在，DWM 就按「可缩放
    ///    窗口」画一圈投影；`WS_CAPTION` 还在，就按「有标题栏」画边框与阴影。
    ///    eframe 的 deferred viewport 即使写了 `with_decorations(false)`，
    ///    实测样式里仍带 `WS_CAPTION | WS_SYSMENU`（本机实测 style=0x16CB0000），
    ///    所以必须显式清掉。
    /// 2. **`DWMWA_NCRENDERING_POLICY = DISABLED`**：直接告诉 DWM「非客户区
    ///    什么都别画」。上一步是"别让它想画"，这一步是"画了也别显示"——
    ///    单靠清样式，部分显卡驱动仍会在窗口外缘留 1px 暗边。
    /// 3. **`DWMWA_WINDOW_CORNER_PREFERENCE = DONOTROUND`**：Windows 11 默认
    ///    给顶层窗口套 8px 系统圆角。内容自己画 16px 圆角时两者错位，系统圆角
    ///    外侧那圈会被 DWM 填成不透明色——全黑背景上就是**"透明区域出现直角
    ///    边界"**。关掉系统圆角，形状完全由内容决定。
    /// 4. **`SWP_FRAMECHANGED`**：样式改了不重算非客户区等于没改。
    fn fix_frame(hwnd: HWND) {
        // SAFETY: hwnd 是刚枚举出来的有效窗口句柄。
        unsafe {
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            let borders = (WS_THICKFRAME | WS_CAPTION | WS_SYSMENU) as isize;
            if style & borders != 0 {
                SetWindowLongPtrW(hwnd, GWL_STYLE, style & !borders);
            }

            let nc_disabled: i32 = DWMNCRP_DISABLED;
            let hr_nc = DwmSetWindowAttribute(
                hwnd,
                DWMWA_NCRENDERING_POLICY as u32,
                &nc_disabled as *const i32 as *const core::ffi::c_void,
                std::mem::size_of::<i32>() as u32,
            );
            if hr_nc < 0 {
                tracing::debug!(hr_nc, "关闭非客户区渲染失败（旧系统可忽略）");
            }

            let no_round: i32 = DWMWCP_DONOTROUND;
            let hr_round = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                &no_round as *const i32 as *const core::ffi::c_void,
                std::mem::size_of::<i32>() as u32,
            );
            if hr_round < 0 {
                tracing::debug!(hr_round, "关闭系统圆角失败（Win10 无此属性，正常）");
            }

            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
            );
        }
    }

    /// 把窗口切到「点得穿 + 逐像素透明 + 无边框无阴影」。
    ///
    /// 顺序不可反：先修外观 → 再加 `LAYERED`（拿命中测试跳过）→ 最后补
    /// `DwmEnableBlurBehindWindow`（加 LAYERED 会把窗口重置回"等分层属性"的
    /// 状态，alpha=0 全不可见，这一步把它拉回逐像素合成）。
    fn apply(hwnd: HWND) {
        fix_frame(hwnd);

        // SAFETY: hwnd 是刚枚举出来的有效窗口句柄。
        unsafe {
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            let want = ex | WS_EX_TRANSPARENT as isize | WS_EX_LAYERED as isize;
            if want != ex {
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, want);
            }

            // 空区域 = "整窗都按交换链 alpha 合成"（winit 创建时用的同一招）
            let region = CreateRectRgn(0, 0, -1, -1);
            let bb = DWM_BLURBEHIND {
                dwFlags: DWM_BB_ENABLE | DWM_BB_BLURREGION,
                fEnable: TRUE,
                hRgnBlur: region,
                fTransitionOnMaximized: 0,
            };
            let hr = DwmEnableBlurBehindWindow(hwnd, &bb);
            if !region.is_null() {
                windows_sys::Win32::Graphics::Gdi::DeleteObject(region as _);
            }
            if hr < 0 {
                tracing::warn!(hr, "DwmEnableBlurBehindWindow 失败，歌词窗可能不可见");
            }
        }
    }

    pub fn set_click_through(title: &str) -> bool {
        let hwnd = find_window(title);
        if hwnd.is_null() {
            return false;
        }
        apply(hwnd);
        true
    }

    /// 只修外观，不加穿透位。
    pub fn fix_transparent_frame(title: &str) -> bool {
        let hwnd = find_window(title);
        if hwnd.is_null() {
            return false;
        }
        fix_frame(hwnd);
        true
    }

    /// 按标题找本进程的顶层窗口；找不到返回空句柄。
    fn find_window(title: &str) -> HWND {
        let mut search = Search {
            title,
            found: std::ptr::null_mut(),
        };
        // SAFETY: 回调签名与 WNDENUMPROC 一致；lparam 指向本栈帧里的 search，
        // EnumWindows 返回前一直有效。
        unsafe {
            EnumWindows(Some(on_window), &mut search as *mut Search<'_> as isize);
        }
        search.found
    }

    /// 把第 0 行从窗口区域里挖掉（DWM 顶部 1px 描边随之被裁剪）。
    ///
    /// `SetWindowRgn` 成功时区域所有权归系统，失败时仍归我们，必须自己
    /// `DeleteObject`，否则每重试一次就漏一个 GDI 对象。
    pub fn clip_top_border(title: &str) -> bool {
        let hwnd = find_window(title);
        if hwnd.is_null() {
            return false;
        }
        // SAFETY: hwnd 是本进程枚举到的有效顶层窗口；rc 是本地栈变量；
        // rgn 在成功交给系统前由本帧持有。
        unsafe {
            let mut rc = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if GetClientRect(hwnd, &mut rc) == 0 {
                return false;
            }
            let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
            // 只剩 1 行时挖掉就什么都不剩了，宁可保留描边也别把窗口剪没
            if w <= 0 || h <= 1 {
                return false;
            }
            let rgn = CreateRectRgn(0, 1, w, h);
            if rgn.is_null() {
                return false;
            }
            if SetWindowRgn(hwnd, rgn, TRUE) == 0 {
                DeleteObject(rgn as _);
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn missing_window_is_reported_instead_of_panicking() {
        // 不存在的标题必须安静地返回 false（调用方据此下一帧重试），
        // 绝不能 panic —— 它跑在每帧的渲染回调里。
        let ok = super::set_click_through("REIMU PLAYER- DEFINITELY NOT A REAL WINDOW TITLE");
        #[cfg(windows)]
        assert!(
            !ok,
            "找不到窗口时必须返回 false，否则调用方会误以为穿透已生效"
        );
        #[cfg(not(windows))]
        assert!(ok, "非 Windows 平台无此语义，直接视为成功");

        let clip = super::clip_top_border("REIMU PLAYER- DEFINITELY NOT A REAL WINDOW TITLE");
        #[cfg(windows)]
        assert!(!clip, "找不到窗口时剪边必须返回 false，让调用方下次重试");
        #[cfg(not(windows))]
        assert!(clip, "非 Windows 平台没有 DWM 描边，直接视为成功");
    }
}
