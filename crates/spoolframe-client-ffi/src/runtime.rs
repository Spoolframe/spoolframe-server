//! The tokio runtime the SDK lives on. The SDK spawns its own tasks (LAN
//! discovery, each server's reconnects) on the runtime it was opened in,
//! and Swift's and Kotlin's executors aren't one, so every call runs here
//! and the foreign side awaits the result.

use std::future::Future;
use std::sync::LazyLock;

use tokio::runtime::{Builder, Runtime};

static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("spoolframe")
        .enable_all()
        .build()
        .expect("the runtime starts")
});

/// Run `work` on the SDK's runtime; the foreign caller awaits it from its
/// own executor. Dropping this future aborts `work`, but the UniFFI Swift
/// bindings never drop it when a Swift task is cancelled: a call that must
/// stop early needs its own cancel (as `PendingSignIn::cancel`).
pub(crate) async fn run<T, F>(work: F) -> T
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    struct AbortOnDrop(tokio::task::AbortHandle);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let task = RUNTIME.spawn(work);
    let _cancel = AbortOnDrop(task.abort_handle());
    match task.await {
        Ok(value) => value,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => panic!("the runtime dropped a call: {e}"),
    }
}

/// Spawn on the SDK's runtime, as the SDK's readers take it: their
/// fetches run there while a foreign thread waits on a read.
pub(crate) fn spawner() -> spoolframe_client_sdk::reader::Spawn {
    std::sync::Arc::new(|work| {
        RUNTIME.spawn(work);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelling_foreign_waiter_drops_delayed_runtime_work() {
        struct SignalDrop(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for SignalDrop {
            fn drop(&mut self) {
                let _ = self.0.take().unwrap().send(());
            }
        }
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let caller = tokio::spawn(run(async move {
            let _guard = SignalDrop(Some(dropped_tx));
            entered_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        }));
        entered_rx.await.unwrap();
        caller.abort();
        let _ = caller.await;
        tokio::time::timeout(std::time::Duration::from_secs(2), dropped_rx)
            .await
            .expect("cancelled work must not keep running")
            .unwrap();
    }
}
