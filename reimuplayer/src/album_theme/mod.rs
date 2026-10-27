//! 专辑取色自适应主题 —— 纯计算层 facade。
//!
//! 模块划分：
//! - [`color`]：OKLab/OKLCH ↔ 线性 sRGB、色域裁剪、WCAG 对比度、OKLab 混色；
//! - [`ramp`]：中性暖调色 + 12 色相锚点 + 11 级共享曲线（const 表）；
//! - [`semantic`]：语义 token（Light/Dark 双实现）+ 解析；
//! - [`extract`]：封面取色（中位切分 + 过滤 + 吸附 + 滞回）；
//! - [`state`]：主题模式 + 450ms 过渡 + reduced-motion；
//! - [`export`]：导出 token 表 / 主题矩阵（供设计核对与快照测试）。
//!
//! 硬约束（附录 A.3）：
//! - 本模块**不 import** 任何业务模块、UI 框架类型、全局状态、文件系统或网络；
//! - 依赖方向单向：业务 → 本模块，禁止反向；
//! - 纯计算：输入像素/主题状态，输出颜色值；不持 UI 句柄、不起线程。

pub mod color;
pub mod export;
pub mod extract;
pub mod ramp;
pub mod semantic;
pub mod state;

pub use color::{gamut_clip, in_gamut, oklch_to_srgb_raw, Oklch, Srgb};
pub use extract::{extract, CoverView, Extraction};
pub use semantic::{build_tokens, default_tokens, Accent, ResolvedTheme, ThemeTokens};
pub use state::{is_light, SystemTheme, ThemeMode, ThemeState, TRANSITION_MS};
