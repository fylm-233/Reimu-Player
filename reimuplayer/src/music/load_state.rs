//! 异步数据加载状态机（加载状态展示）。
//!
//! 设计约束（`NETEASE_INTEGRATION_SPEC.md`）：
//! - 8.2：UI 应根据错误类型显示简短、可理解的消息，而不是完整 JSON 或内部堆栈
//!   ——[`LoadState::message`] 只输出人话提示，错误载体是统一 [`ProviderError`]。
//! - 9.2「网络响应不得无限累积」——状态机**最多持有一份数据**：
//!   新一次加载会整体替换旧值，`Loading` 时旧数据立即释放。
//! - 8.4「在线服务状态与本地播放状态分离」——本状态机只描述**在线数据**加载，
//!   不与 [`crate::music::PlaybackState`] 混用。
//!
//! 状态流转（唯一合法路径）：
//! ```text
//! Idle ──begin_load──▶ Loading ──finish(Ok)──▶ Ready(T)
//!   ▲                     │
//!   │                     └──finish(Err)──▶ Failed(ProviderError)
//!   └─────────reset───────────────────────────────┘（任意状态可回 Idle）
//! ```
//!
//! 用法（UI/CLI 消费方）：
//! 1. 发起请求前调用 [`LoadState::begin_load`]，展示「加载中…」。
//! 2. 请求完成后调用 [`LoadState::finish`]，展示数据或 [`LoadState::message`]。
//! 3. 请求必须在异步运行时执行，**不得阻塞 UI 线程**（规范 7.1/8.1），
//!    超时与错误分类由 Provider 层负责，本模块只承载结果。

use crate::i18n::{pick, Lang};
use crate::music::provider::ProviderError;

/// 单个数据项的加载状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadState<T> {
    /// 尚未发起加载。
    Idle,
    /// 加载进行中。
    Loading,
    /// 加载成功，持有唯一一份数据。
    Ready(T),
    /// 加载失败，持有统一分类错误。
    Failed(ProviderError),
}

// 手写 impl 而不是 derive：`#[derive(Default)]` 会给泛型加上 `T: Default`
// 约束，`LoadState<非 Default 类型>` 就没法用了；`Idle` 与 T 无关。
#[allow(clippy::derivable_impls)]
impl<T> Default for LoadState<T> {
    fn default() -> Self {
        LoadState::Idle
    }
}

impl<T> LoadState<T> {
    /// 创建空闲状态。
    pub fn new() -> Self {
        LoadState::Idle
    }

    /// 进入加载中。**释放旧数据**（规范 9.2：响应不得无限累积）。
    pub fn begin_load(&mut self) {
        *self = LoadState::Loading;
    }

    /// 结束加载：`Ok` → 就绪持有数据，`Err` → 失败持有错误。
    pub fn finish(&mut self, result: Result<T, ProviderError>) {
        *self = match result {
            Ok(data) => LoadState::Ready(data),
            Err(e) => LoadState::Failed(e),
        };
    }

    /// 回到空闲，释放数据与错误。
    pub fn reset(&mut self) {
        *self = LoadState::Idle;
    }

    /// 是否空闲。
    pub fn is_idle(&self) -> bool {
        matches!(self, LoadState::Idle)
    }

    /// 是否加载中。
    pub fn is_loading(&self) -> bool {
        matches!(self, LoadState::Loading)
    }

    /// 是否就绪。
    pub fn is_ready(&self) -> bool {
        matches!(self, LoadState::Ready(_))
    }

    /// 是否失败。
    pub fn is_failed(&self) -> bool {
        matches!(self, LoadState::Failed(_))
    }

    /// 就绪数据（只读）。
    pub fn get(&self) -> Option<&T> {
        match self {
            LoadState::Ready(data) => Some(data),
            _ => None,
        }
    }

    /// 失败错误（只读）。
    pub fn error(&self) -> Option<&ProviderError> {
        match self {
            LoadState::Failed(e) => Some(e),
            _ => None,
        }
    }

    /// 状态标签（简短，用于 UI 状态栏）。
    ///
    /// 跟界面语言：中文走短词（未加载/加载中…），英文走全大写等宽观感
    /// （与面板上其它 meta 标签一致）。
    pub fn status_text(&self, lang: Lang) -> &'static str {
        match self {
            LoadState::Idle => pick(lang, "未加载", "NOT LOADED"),
            LoadState::Loading => pick(lang, "加载中…", "LOADING…"),
            LoadState::Ready(_) => pick(lang, "已加载", "READY"),
            LoadState::Failed(_) => pick(lang, "加载失败", "FAILED"),
        }
    }

    /// 面向用户的简短提示（规范 8.2：不暴露 JSON/堆栈）。
    ///
    /// - 空闲/加载中：状态标签。
    /// - 就绪：不额外提示（数据即内容），返回空串由调用方决定是否展示。
    /// - 失败：`加载失败：{错误人话}`，可直接显示。
    pub fn message(&self, lang: Lang) -> String {
        match self {
            LoadState::Ready(_) => String::new(),
            LoadState::Failed(e) => {
                format!(
                    "{}{}",
                    pick(lang, "加载失败：", "Load failed: "),
                    e.message(lang)
                )
            }
            other => other.status_text(lang).to_string(),
        }
    }

    /// 空列表占位文案：把"为什么这里没东西"讲清楚。
    ///
    /// 空态最常见的问题是所有情况都显示 "NO DATA"，用户分不清是没加载、
    /// 加载失败、还是真的没有内容。这里按状态给出可执行的下一步。
    pub fn status_hint(&self, lang: Lang) -> String {
        match self {
            LoadState::Idle => pick(lang, "— 尚未加载 —", "— NOT LOADED YET —").to_string(),
            LoadState::Loading => pick(lang, "— 加载中 —", "— LOADING —").to_string(),
            LoadState::Failed(ProviderError::AuthenticationRequired) => {
                pick(lang, "— 需要登录后才能使用 —", "— SIGN IN TO USE THIS —").to_string()
            }
            LoadState::Failed(e) => format!(
                "{}{}{}",
                pick(lang, "— 加载失败：", "— Load failed: "),
                e.message(lang),
                pick(lang, " —", " —")
            ),
            LoadState::Ready(_) => pick(lang, "— 没有内容 —", "— NOTHING HERE —").to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_state_is_idle() {
        let s: LoadState<u32> = LoadState::new();
        assert!(s.is_idle());
        assert!(!s.is_loading());
        assert!(!s.is_ready());
        assert!(!s.is_failed());
        assert_eq!(s.get(), None);
        assert_eq!(s.error(), None);
    }

    #[test]
    fn default_is_idle() {
        let s: LoadState<u32> = LoadState::default();
        assert!(s.is_idle());
    }

    #[test]
    fn begin_load_transitions_to_loading_and_releases_old_data() {
        let mut s = LoadState::Ready(42u32);
        s.begin_load();
        assert!(s.is_loading());
        assert_eq!(s.get(), None, "进入加载中必须释放旧数据（规范 9.2）");
    }

    #[test]
    fn finish_ok_becomes_ready_holding_data() {
        let mut s: LoadState<String> = LoadState::new();
        s.begin_load();
        s.finish(Ok("结果".into()));
        assert!(s.is_ready());
        assert_eq!(s.get().map(String::as_str), Some("结果"));
    }

    #[test]
    fn finish_err_becomes_failed_holding_typed_error() {
        let mut s: LoadState<String> = LoadState::new();
        s.begin_load();
        s.finish(Err(ProviderError::RequestTimeout));
        assert!(s.is_failed());
        assert_eq!(s.error(), Some(&ProviderError::RequestTimeout));
    }

    #[test]
    fn finish_replaces_previous_state_entirely() {
        // Ready -> finish(Err) 覆盖；Failed -> finish(Ok) 覆盖；始终只持有一份数据
        let mut s = LoadState::Ready(1u32);
        s.finish(Err(ProviderError::NetworkUnavailable));
        assert!(s.is_failed());
        assert_eq!(s.get(), None);

        s.finish(Ok(2));
        assert!(s.is_ready());
        assert_eq!(s.get(), Some(&2));
        assert_eq!(s.error(), None);
    }

    #[test]
    fn reset_returns_to_idle_releasing_everything() {
        let mut s = LoadState::Ready(7u32);
        s.reset();
        assert!(s.is_idle());

        let mut s2: LoadState<u32> = LoadState::new();
        s2.finish(Err(ProviderError::InvalidData));
        s2.reset();
        assert!(s2.is_idle());
        assert_eq!(s2.error(), None);
    }

    #[test]
    fn status_text_matches_each_state() {
        let s: LoadState<u32> = LoadState::Idle;
        assert_eq!(s.status_text(Lang::Zh), "未加载");
        let s = LoadState::<u32>::Loading;
        assert_eq!(s.status_text(Lang::Zh), "加载中…");
        let s = LoadState::Ready(1u32);
        assert_eq!(s.status_text(Lang::Zh), "已加载");
        let s = LoadState::<u32>::Failed(ProviderError::RequestTimeout);
        assert_eq!(s.status_text(Lang::Zh), "加载失败");
    }

    #[test]
    fn message_is_human_readable_not_json() {
        // 失败提示可直接展示：人话、无 JSON、无内部堆栈
        let s = LoadState::<u32>::Failed(ProviderError::ContentUnavailable);
        let msg = s.message(Lang::Zh);
        assert!(msg.contains("加载失败"));
        assert!(!msg.contains('{'), "提示不应暴露 JSON: {}", msg);
        assert!(!msg.to_lowercase().contains("reqwest"));

        // 就绪不产生额外提示（数据即内容）
        assert_eq!(LoadState::Ready(1u32).message(Lang::Zh), "");
        // 空闲/加载中为状态标签
        assert_eq!(LoadState::<u32>::Idle.message(Lang::Zh), "未加载");
        assert_eq!(LoadState::<u32>::Loading.message(Lang::Zh), "加载中…");
    }

    #[test]
    fn status_hint_distinguishes_login_gate_from_plain_failure() {
        // 空态必须能区分"要登录"和"加载失败"，否则用户不知道该扫码还是重试
        let gate =
            LoadState::<u32>::Failed(ProviderError::AuthenticationRequired).status_hint(Lang::Zh);
        assert!(gate.contains("登录"), "需登录的空态要提示登录: {gate}");

        let fail = LoadState::<u32>::Failed(ProviderError::RequestTimeout).status_hint(Lang::Zh);
        assert!(fail.contains("加载失败"));
        assert!(!fail.contains("登录"));

        assert_eq!(LoadState::<u32>::Idle.status_hint(Lang::Zh), "— 尚未加载 —");
        assert_eq!(
            LoadState::<u32>::Loading.status_hint(Lang::Zh),
            "— 加载中 —"
        );
        assert_eq!(
            LoadState::Ready(vec![1u32]).status_hint(Lang::Zh),
            "— 没有内容 —"
        );
    }
}
