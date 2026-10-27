//! 具体 Provider 实现。
//!
//! 每个实现负责把某个第三方 API 适配成 [`crate::music::provider::MusicProvider`]。
//! **第三方 JSON 与第三方错误类型不得越过本模块边界。**
//!
//! 依赖与许可证（`DESIGN_SPEC.md` 15.4 要求逐项记录）：
//!
//! | Provider | 依赖 | 许可证 | 说明 |
//! |---|---|---|---|
//! | [`ncm`] | `ncm-api-rs` 0.1.0 | WTFPL | 网易云音乐 API SDK |
//! | [`http_fetch`] | `reqwest` 0.12 | MIT OR Apache-2.0 | 音频字节下载（依赖树既有，rustls，无新编译单元） |
//!
//! 本阶段只实现网易云 Provider。本地文件走
//! [`crate::music::media::load_media`]，不经过 Provider 抽象
//! （本地路径与在线 ID 语义不同，见规范 6.2）。
//!
//! 模块地图（网络 API 层）：
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`ncm`] | 网易云 API 适配：搜索 / 歌曲详情 / 歌单 / 歌词 / 播放地址 / 精选列表（推荐、排行榜）/ 登录态；SDK 返回的原始 JSON 在此归一为类型化模型，每类请求按 [`ProviderTimeouts`] 在适配层内施加整体超时 |
//! | [`http_fetch`] | 音频字节下载器（[`HttpAudioFetcher`]）：空闲超时 + Range 断点续传 + 写盘缓冲 + 体积上限，产物交 [`crate::music::online_playback`] 回收 |
//! | [`test_support`] | 仅 `cfg(test)`：手写本地 HTTP/1.1 服务器，演出断流 / 206 续传 / 慢涓流 / chunked / 403，供下载与流式传输两侧测试共用 |

pub mod http_fetch;
pub mod ncm;
#[cfg(test)]
pub(crate) mod test_support;

pub use http_fetch::HttpAudioFetcher;
pub use ncm::{NeteaseProvider, ProviderTimeouts};
