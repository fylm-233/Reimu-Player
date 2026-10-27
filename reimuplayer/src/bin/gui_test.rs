//! GUI 测试程序入口（`--features gui` 编译）。
//!
//! 验证在线搜索 / 歌曲详情 / 歌单内容读取在 Reimu Player 窗口 UI 规范
//! （`设计2.md`）下的：加载状态展示、请求超时控制、友好错误提示。
//!
//! 用法：
//! ```text
//! cargo run -p reimuplayer --features gui --bin gui-test
//! ```
//!
//! 可选参数：
//! - `--search-timeout-ms <ms>` 覆盖搜索超时（请求超时控制演示）；
//! - 其余超时遵循规范 8.1 默认值（[`reimuplayer::providers::ProviderTimeouts`]）。

use std::time::Duration;

use reimuplayer::providers::ProviderTimeouts;

fn main() -> eframe::Result {
    // 日志走 tracing（不写文件，验证期输出到 stderr）
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let mut timeouts = ProviderTimeouts::default();
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--search-timeout-ms" => {
                if let Some(v) = args.get(i + 1).and_then(|s| s.parse::<u64>().ok()) {
                    timeouts.search = Duration::from_millis(v);
                    i += 1;
                }
            }
            other => {
                eprintln!("未知参数: {}（支持 --search-timeout-ms <ms>）", other);
            }
        }
        i += 1;
    }

    reimuplayer::gui::run(
        "REIMU PLAYER AUDIO PLAYER — ONLINE DATA TEST",
        timeouts,
        std::sync::Arc::new(|t| {
            std::sync::Arc::new(reimuplayer::providers::NeteaseProvider::new().with_timeouts(t))
                as std::sync::Arc<dyn reimuplayer::DynMusicProvider>
        }),
    )
}
