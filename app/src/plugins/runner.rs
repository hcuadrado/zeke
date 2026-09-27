//! One plugin's instance, started and stopped in order on tokio. Nothing
//! here touches GTK.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use zeke_plugin::{DynPlugin, Handle, Services};

/// What the worker reports back.
pub enum Event {
    /// An instance is up. `epoch` is the start command's.
    Started { epoch: u64, handle: Handle },
    /// The instance started with `epoch` has stopped.
    Stopped { epoch: u64 },
}

enum Command {
    Start { epoch: u64, services: Services },
    Stop,
}

/// Runs one plugin's commands one at a time, so a start waits for the stop
/// before it and a quick off and on never runs two instances.
pub struct Runner {
    commands: mpsc::UnboundedSender<Command>,
    /// Signalled when the worker has ended.
    done: std::sync::mpsc::Receiver<()>,
}

impl Runner {
    pub fn spawn(
        runtime: &tokio::runtime::Handle,
        plugin: Arc<dyn DynPlugin>,
        events: impl Fn(Event) + Send + Sync + 'static,
    ) -> Self {
        let (commands, rx) = mpsc::unbounded_channel();
        let (done_tx, done) = std::sync::mpsc::channel();
        runtime.spawn(async move {
            work(plugin, rx, events).await;
            let _ = done_tx.send(());
        });
        Self { commands, done }
    }

    pub fn start(&self, epoch: u64, services: Services) {
        let _ = self.commands.send(Command::Start { epoch, services });
    }

    pub fn stop(&self) {
        let _ = self.commands.send(Command::Stop);
    }

    /// Stop the instance and end the worker, without waiting.
    pub fn close(self) -> std::sync::mpsc::Receiver<()> {
        self.done
    }
}

async fn work(plugin: Arc<dyn DynPlugin>, mut commands: mpsc::UnboundedReceiver<Command>, events: impl Fn(Event) + Sync) {
    let mut running: Option<(u64, Handle)> = None;
    while let Some(command) = commands.recv().await {
        match command {
            Command::Start { epoch, services } => {
                if running.is_some() {
                    continue;
                }
                let handle = Arc::clone(&plugin).start(services).await;
                log::info!("[plugins] {} started", plugin.id());
                events(Event::Started { epoch, handle: handle.clone() });
                running = Some((epoch, handle));
            }
            Command::Stop => stop(&plugin, &mut running, &events).await,
        }
    }
    // The app is quitting.
    stop(&plugin, &mut running, &events).await;
}

async fn stop(plugin: &Arc<dyn DynPlugin>, running: &mut Option<(u64, Handle)>, events: &(impl Fn(Event) + Sync)) {
    if let Some((epoch, handle)) = running.take() {
        Arc::clone(plugin).stop(handle).await;
        log::info!("[plugins] {} stopped", plugin.id());
        events(Event::Stopped { epoch });
    }
}

/// Wait for closed runners' workers, all within `limit`.
pub fn wait_all(done: Vec<std::sync::mpsc::Receiver<()>>, limit: Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    done.into_iter().all(|rx| {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        rx.recv_timeout(left).is_ok()
    })
}

/// The account an instance runs for. `None` inside is an account whose id
/// TIDAL didn't give.
pub type Account = Option<u64>;

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Nothing,
    Start,
    Stop,
    /// Stop, then start for the new account.
    Restart,
}

/// What a plugin should do: it runs while its switch is on and someone is
/// signed in, for the account signed in. `running_for` is the account the
/// current instance was started for, `None` when none was.
pub fn reconcile(enabled: bool, signed_in: Option<Account>, running_for: Option<Account>) -> Action {
    match (enabled, signed_in, running_for) {
        (true, Some(now), Some(then)) if now != then => Action::Restart,
        (true, Some(_), Some(_)) => Action::Nothing,
        (true, Some(_), None) => Action::Start,
        (_, _, Some(_)) => Action::Stop,
        (_, _, None) => Action::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use zeke_plugin::{Plugin, PluginUi};

    use super::*;

    /// Counts its live instances; start and stop take a while.
    #[derive(Default)]
    struct Probe {
        live: AtomicUsize,
        most: AtomicUsize,
        starts: AtomicUsize,
    }

    impl Plugin for Probe {
        type Handle = ();

        fn id(&self) -> &'static str {
            "probe"
        }

        fn name(&self) -> &'static str {
            "Probe"
        }

        async fn start(&self, _: Services) {
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.starts.fetch_add(1, Ordering::SeqCst);
            let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.most.fetch_max(live, Ordering::SeqCst);
        }

        fn ui(&self, _: &()) -> PluginUi {
            PluginUi::default()
        }

        async fn stop(&self, _: &()) {
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.live.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// `(started?, epoch)` in the order the worker reported them.
    type Log = Arc<Mutex<Vec<(bool, u64)>>>;

    fn runner(probe: &Arc<Probe>) -> (Runner, Log) {
        let log: Log = Arc::default();
        let events = {
            let log = Arc::clone(&log);
            move |e: Event| {
                log.lock().unwrap().push(match e {
                    Event::Started { epoch, .. } => (true, epoch),
                    Event::Stopped { epoch } => (false, epoch),
                })
            }
        };
        let plugin: Arc<dyn DynPlugin> = probe.clone();
        (Runner::spawn(&tokio::runtime::Handle::current(), plugin, events), log)
    }

    async fn settle(log: &Log, len: usize) {
        for _ in 0..200 {
            if log.lock().unwrap().len() >= len {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the worker reported {:?}", log.lock().unwrap());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_quick_off_and_on_never_runs_two_instances() {
        let probe = Arc::new(Probe::default());
        let (runner, log) = runner(&probe);
        for epoch in 1..=3 {
            runner.start(epoch, Services::default());
            runner.stop();
        }
        runner.start(4, Services::default());
        settle(&log, 7).await;
        assert_eq!(probe.most.load(Ordering::SeqCst), 1, "never two at once");
        assert_eq!(probe.live.load(Ordering::SeqCst), 1);
        assert_eq!(probe.starts.load(Ordering::SeqCst), 4);
        assert_eq!(
            *log.lock().unwrap(),
            [(true, 1), (false, 1), (true, 2), (false, 2), (true, 3), (false, 3), (true, 4)]
        );
        // Quitting stops the last one.
        let done = runner.close();
        assert!(tokio::task::spawn_blocking(move || wait_all(vec![done], Duration::from_secs(2))).await.unwrap());
        assert_eq!(probe.live.load(Ordering::SeqCst), 0);
        assert_eq!(log.lock().unwrap().last(), Some(&(false, 4)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_start_while_running_is_ignored() {
        let probe = Arc::new(Probe::default());
        let (runner, log) = runner(&probe);
        runner.start(1, Services::default());
        runner.start(2, Services::default());
        runner.stop();
        settle(&log, 2).await;
        assert_eq!(*log.lock().unwrap(), [(true, 1), (false, 1)]);
        assert_eq!(probe.starts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn it_runs_while_on_and_signed_in_and_restarts_for_another_account() {
        use Action::*;
        let (a, b) = (Some(1), Some(2));
        // Switched on, or signed in with the switch on.
        assert_eq!(reconcile(true, Some(a), None), Start);
        // Switched on while signed out: waits for a sign-in.
        assert_eq!(reconcile(true, None, None), Nothing);
        // Switched off, or signed out.
        assert_eq!(reconcile(false, Some(a), Some(a)), Stop);
        assert_eq!(reconcile(true, None, Some(a)), Stop);
        // Another account.
        assert_eq!(reconcile(true, Some(b), Some(a)), Restart);
        assert_eq!(reconcile(true, Some(None), Some(a)), Restart);
        // The same account again (a login after it expired).
        assert_eq!(reconcile(true, Some(a), Some(a)), Nothing);
        assert_eq!(reconcile(false, Some(a), None), Nothing);
    }
}
