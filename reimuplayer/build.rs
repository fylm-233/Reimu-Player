//! 把产品图标与版本信息写进 PE 资源段。
//!
//! 资源管理器 / 桌面 / 快捷方式 / 开始菜单 / 任务管理器读的是 exe 里的
//! `RT_GROUP_ICON`，这和窗口图标（`WM_SETICON`）是两套来源——这里负责前者。
//! 只在 Windows 目标上编译资源，其他平台直接跳过（库本身仍可跨平台构建）。

fn main() {
    println!("cargo:rerun-if-changed=../assets/icon/reimu-player.ico");

    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

    let icon =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../assets/icon/reimu-player.ico");
    let mut res = winresource::WindowsResource::new();
    res.set_icon(icon.to_str().expect("图标路径不是合法 UTF-8"))
        .set("FileDescription", "Reimu Player 音乐播放器")
        .set("ProductName", "Reimu Player")
        .set("CompanyName", "Reimu Player")
        .set("OriginalFilename", "reimu-player.exe")
        .set("InternalName", "reimu-player")
        .set("LegalCopyright", "© 2026 Reimu Player")
        .set_language(0x0804);
    // 只有产品二进制带资源：CLI 验证程序与 gui-test 不是发布物。
    res.compile_for("reimu-player")
        .expect("编译 Windows 资源（图标 + 版本信息）失败");
}
