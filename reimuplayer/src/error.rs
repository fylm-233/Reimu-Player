//! 统一错误类型。
//!
//! 约束（`NETEASE_INTEGRATION_SPEC.md` 3.2 第 3 条）：所有底层错误——含网络错误——
//! 都必须转换为项目内部类型，不得把 `reqwest::Error`、`serde_json::Error`
//! 等第三方类型暴露给 UI 层。
//!
//! 这里定义两个职责清晰的错误类型：
//! - [`PlayerError`]：媒体加载、播放控制、音频设备相关。
//! - [`ProviderError`]（见 [`crate::providers`]）：在线服务相关。
//!
//! 分开而非合成一个巨型枚举，是因为二者的触发场景、恢复策略和 UI 提示完全不同。

use std::path::PathBuf;

/// 播放器本地能力相关错误。
#[derive(Debug, thiserror::Error)]
pub enum PlayerError {
    // ---- 媒体加载 ----
    #[error("媒体文件不存在: {0}")]
    MediaNotFound(PathBuf),

    #[error("无法读取媒体文件 {path}: {source}")]
    MediaRead {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("不支持的媒体格式: {0}")]
    UnsupportedFormat(String),

    #[error("解析媒体元数据失败 {path}: {detail}")]
    MetadataFailed { path: PathBuf, detail: String },

    #[error("解码失败 {path}: {detail}")]
    DecodeFailed { path: PathBuf, detail: String },

    // ---- 音频设备 ----
    #[error("音频输出设备不可用: {0}")]
    AudioDeviceUnavailable(String),

    #[error("音频输出错误: {0}")]
    OutputError(String),

    // ---- 播放控制 ----
    #[error("当前状态「{state}」不支持操作「{action}」")]
    InvalidState { state: String, action: String },

    #[error("播放位置无效: {0}")]
    InvalidPosition(String),

    #[error("音量超出范围 [0.0, 1.0]: {0}")]
    InvalidVolume(f32),
}

impl PlayerError {
    /// 该错误是否值得让用户重试一次。
    ///
    /// 用于 UI 决定是否显示「重试」按钮，避免对不可恢复错误（如格式不支持）误导用户。
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            PlayerError::AudioDeviceUnavailable(_) | PlayerError::OutputError(_)
        )
    }
}

/// 便捷别名。
pub type Result<T> = std::result::Result<T, PlayerError>;
