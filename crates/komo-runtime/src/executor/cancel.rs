//! 把 kernel 的 [`CancelToken`] 变成一个可以 await 的东西。
//!
//! kernel 不依赖 tokio（§13.4），所以那个 token 只有一个 `AtomicBool`：**查询**取消
//! 在哪一层都行，**等待**取消发生是运行时的事。这里就是那件事——轮询，间隔短到人
//! 察觉不出，长到不值一提。

use std::time::Duration;

use komo_kernel::types::tool::CancelToken;

/// 轮询间隔。取消是人按下的，25ms 的延迟看不出来；而 loop 的每个 await 都要和它
/// 赛跑，所以它必须便宜。
const POLL: Duration = Duration::from_millis(25);

/// 一直等到这个 token 被取消。**不会返回"没取消"**——它只在取消时就绪。
pub async fn cancelled(token: &CancelToken) {
    loop {
        if token.is_cancelled() {
            return;
        }
        tokio::time::sleep(POLL).await;
    }
}

/// 让一个 future 和取消赛跑。`None` = 取消赢了。
pub async fn race<F: Future>(token: &CancelToken, future: F) -> Option<F::Output> {
    if token.is_cancelled() {
        return None;
    }
    tokio::select! {
        biased;
        () = cancelled(token) => None,
        value = future => Some(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_future_that_finishes_first_wins() {
        let token = CancelToken::new();
        assert_eq!(race(&token, async { 7 }).await, Some(7));
    }

    #[tokio::test]
    async fn a_token_cancelled_beforehand_never_starts_the_future() {
        let token = CancelToken::new();
        token.cancel();
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = ran.clone();
        let outcome = race(&token, async move {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .await;
        assert!(outcome.is_none());
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancelling_mid_flight_stops_the_wait() {
        let token = CancelToken::new();
        let watcher = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            watcher.cancel();
        });
        let outcome = race(&token, tokio::time::sleep(Duration::from_secs(30))).await;
        assert!(outcome.is_none());
    }
}
