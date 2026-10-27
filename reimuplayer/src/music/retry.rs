//! 有限重试策略（`NETEASE_INTEGRATION_SPEC.md` 8.3）。
//!
//! 规则：
//! - 超时 / 网络不可用 / 数据服务不可用：允许**有限次**重试（指数退避）。
//! - 限流：优先遵守服务端重试建议，不自动重试（防止请求风暴）。
//! - 登录失效：要求重新认证，不重试。
//! - 版权 / VIP / 地区限制（不可播）：**不重试、不绕过**。
//! - 无效参数 / 用户取消：不得重试。
//!
//! 实现说明：只对 [`ProviderError::is_retryable`] 为真的错误重试，
//! 且总尝试次数 = 1 + `max_retries`，从机制上杜绝无限重试与请求风暴。

use std::future::Future;
use std::time::Duration;

use crate::music::provider::ProviderError;

/// 有限重试策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// 首次失败之后的最大重试次数（总尝试次数 = 1 + 该值）。
    pub max_retries: u32,
    /// 首次重试前的等待时长；之后按 2 的幂指数退避。
    pub base_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        // 规范 8.3「有限次数」：默认最多重试 2 次，300ms 起步指数退避。
        Self {
            max_retries: 2,
            base_delay: Duration::from_millis(300),
        }
    }
}

impl RetryPolicy {
    /// 禁用重试（用于不可重试语义已经明确的操作，或测试）。
    pub const NO_RETRY: Self = Self {
        max_retries: 0,
        base_delay: Duration::ZERO,
    };

    /// 执行 `op`，仅对可重试错误做有限次重试。
    ///
    /// `op` 每次被调用都应发起一次**全新**请求（不要复用已消费的 future）。
    pub async fn retry_limited<T, F, Fut>(&self, mut op: F) -> Result<T, ProviderError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, ProviderError>>,
    {
        let mut attempt = 0u32;
        loop {
            match op().await {
                Ok(value) => return Ok(value),
                Err(e) if e.is_retryable() && attempt < self.max_retries => {
                    // 指数退避：base * 2^attempt，指数封顶避免溢出
                    let delay = self.base_delay.saturating_mul(1u32 << attempt.min(4));
                    tracing::warn!(
                        "第 {} 次尝试失败（{}），{:?} 后重试（共允许 {} 次重试）",
                        attempt + 1,
                        e,
                        delay,
                        self.max_retries
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(e) => {
                    if e.is_retryable() {
                        tracing::error!("重试次数已用尽，最后错误: {}", e);
                    }
                    return Err(e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// 快速策略：测试不等待真实时间。
    fn fast(max_retries: u32) -> RetryPolicy {
        RetryPolicy {
            max_retries,
            base_delay: Duration::from_millis(1),
        }
    }

    #[tokio::test]
    async fn succeeds_first_try_does_not_retry() {
        let calls = RefCell::new(0u32);
        let out = fast(2)
            .retry_limited(|| {
                *calls.borrow_mut() += 1;
                async { Ok::<_, ProviderError>(42) }
            })
            .await
            .unwrap();
        assert_eq!(out, 42);
        assert_eq!(*calls.borrow(), 1);
    }

    #[tokio::test]
    async fn retryable_error_is_retried_until_success() {
        let calls = RefCell::new(0u32);
        let out = fast(2)
            .retry_limited(|| {
                let n = *calls.borrow_mut() + 1;
                *calls.borrow_mut() = n;
                async move {
                    if n < 3 {
                        Err(ProviderError::RequestTimeout)
                    } else {
                        Ok("ok")
                    }
                }
            })
            .await
            .unwrap();
        assert_eq!(out, "ok");
        assert_eq!(*calls.borrow(), 3, "应恰好尝试 3 次（1 + 2 次重试）");
    }

    #[tokio::test]
    async fn gives_up_after_max_retries() {
        let calls = RefCell::new(0u32);
        let err = fast(2)
            .retry_limited(|| {
                *calls.borrow_mut() += 1;
                async { Err::<(), _>(ProviderError::NetworkUnavailable) }
            })
            .await
            .unwrap_err();
        assert_eq!(err, ProviderError::NetworkUnavailable);
        assert_eq!(*calls.borrow(), 3, "总尝试次数 = 1 + max_retries");
    }

    #[tokio::test]
    async fn rate_limit_is_not_retried() {
        // 规范 8.3：限流优先遵守服务端建议，不自动重试
        let calls = RefCell::new(0u32);
        let err = fast(3)
            .retry_limited(|| {
                *calls.borrow_mut() += 1;
                async { Err::<(), _>(ProviderError::RateLimited) }
            })
            .await
            .unwrap_err();
        assert_eq!(err, ProviderError::RateLimited);
        assert_eq!(*calls.borrow(), 1, "限流不得触发任何重试");
    }

    #[tokio::test]
    async fn non_retryable_errors_are_not_retried() {
        // 规范 8.3：无效参数 / 取消不得重试；版权等不可播不重试、不绕过
        for e in [
            ProviderError::InvalidParameter,
            ProviderError::Cancelled,
            ProviderError::ContentUnavailable,
            ProviderError::PlaybackUrlUnavailable,
            ProviderError::AuthenticationRequired,
            ProviderError::AuthenticationExpired,
            ProviderError::EmptyResult,
            ProviderError::UnsupportedResponse,
        ] {
            let calls = RefCell::new(0u32);
            let got = fast(3)
                .retry_limited(|| {
                    *calls.borrow_mut() += 1;
                    let e = e.clone();
                    async move { Err::<(), _>(e) }
                })
                .await
                .unwrap_err();
            assert_eq!(got, e);
            assert_eq!(*calls.borrow(), 1, "{:?} 不得触发重试", e);
        }
    }

    #[tokio::test]
    async fn zero_max_retries_runs_exactly_once() {
        let calls = RefCell::new(0u32);
        let err = RetryPolicy::NO_RETRY
            .retry_limited(|| {
                *calls.borrow_mut() += 1;
                async { Err::<(), _>(ProviderError::RequestTimeout) }
            })
            .await
            .unwrap_err();
        assert_eq!(err, ProviderError::RequestTimeout);
        assert_eq!(*calls.borrow(), 1);
    }
}
