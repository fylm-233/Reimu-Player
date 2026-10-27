//! 日志初始化。
//!
//! 约束（`DESIGN_SPEC.md` 15.2）：**音频回调中禁止写日志文件**。
//! 本模块只负责在进程启动时初始化一次性日志设施；运行期日志由 `tracing` 输出，
//! 默认写到 stderr，不做任何文件 I/O。
//!
//! 通过环境变量 `RUST_LOG` 控制级别，例如：
//! ```text
//! RUST_LOG=reimuplayer=debug,info
//! ```
//! 未设置时默认 `info`，避免验证期被高频调试日志淹没。

use tracing_subscriber::EnvFilter;

/// 初始化全局日志。重复调用是安全的（后续调用为 no-op）。
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // `try_init` 而非 `init`：多次初始化不应导致 panic。
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();

    tracing::debug!("日志已初始化");
}
