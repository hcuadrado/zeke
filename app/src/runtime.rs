//! The one way to run async work: a tokio runtime on background threads, with
//! results handed back to the GTK main loop over an `async-channel`.
//!
//! GTK widgets may only be touched on the main thread, and reqwest needs a
//! tokio reactor, so work runs on tokio and only its result crosses over.

use std::future::Future;
use std::sync::OnceLock;

use gtk::glib;
use tokio::runtime::Runtime;

/// The app-wide tokio runtime, started on first use.
pub fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("zeke-tokio")
            .enable_all()
            .build()
            .expect("failed to start the tokio runtime")
    })
}

/// Runs `task` on tokio, then calls `on_done` with its output on the GTK main
/// loop. Must be called from the main thread.
///
/// If the receiving side goes away first (e.g. the app is quitting), the
/// result is dropped and `on_done` never runs.
pub fn spawn<T, F>(task: F, on_done: impl FnOnce(T) + 'static)
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    spawn_abortable(task, on_done);
}

/// As `spawn`, and the task can be aborted: its future is dropped at its
/// next await (a request in flight is cancelled, a lock it holds is
/// released) and `on_done` never runs.
pub fn spawn_abortable<T, F>(task: F, on_done: impl FnOnce(T) + 'static) -> tokio::task::AbortHandle
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = async_channel::bounded(1);
    let handle = runtime().spawn(async move {
        // Err only means the receiver was dropped; nothing is left to notify.
        let _ = tx.send(task.await).await;
    });
    glib::spawn_future_local(async move {
        if let Ok(value) = rx.recv().await {
            on_done(value);
        }
    });
    handle.abort_handle()
}
