//! Cast: playing the queue on network devices. For now an empty plugin: a
//! task that runs while the plugin is on and follows the credential, an
//! empty Preferences page and an empty output-menu section.

use std::sync::atomic::{AtomicUsize, Ordering};

use adw::prelude::*;
// Named apart: the plugin is also called `Cast`.
use gtk::glib::object::Cast as _;
use zeke_plugin::{Need, Plugin, PluginUi, Services};

pub struct Cast;

/// A running Cast.
pub struct Running {
    task: tokio::task::JoinHandle<()>,
}

/// Instances alive now, for the log: there should never be more than one.
static LIVE: AtomicUsize = AtomicUsize::new(0);

impl Plugin for Cast {
    type Handle = Running;

    fn id(&self) -> &'static str {
        "cast"
    }

    fn name(&self) -> &'static str {
        "Cast"
    }

    fn needs(&self) -> &'static [Need] {
        &[Need::Credential]
    }

    async fn start(&self, services: Services) -> Running {
        let live = LIVE.fetch_add(1, Ordering::SeqCst) + 1;
        log::debug!("[cast] up ({live} running)");
        let task = tokio::spawn(async move {
            let Some(credential) = services.credential else {
                return std::future::pending().await;
            };
            let mut watch = credential.watch();
            log::debug!("[cast] credential: {:?}", *watch.borrow_and_update());
            while watch.changed().await.is_ok() {
                log::debug!("[cast] credential changed: {:?}", *watch.borrow_and_update());
            }
        });
        Running { task }
    }

    fn ui(&self, _: &Running) -> PluginUi {
        let page = adw::PreferencesGroup::builder().description("Nothing to set up yet.").build();
        let section = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
        section.append(&gtk::Label::builder().label("Cast").xalign(0.0).css_classes(["heading"]).build());
        section.append(&gtk::Label::builder().label("No devices").xalign(0.0).css_classes(["dim-label"]).build());
        PluginUi { page: Some(page.upcast()), output_section: Some(section.upcast()) }
    }

    async fn stop(&self, running: &Running) {
        running.task.abort();
        let live = LIVE.fetch_sub(1, Ordering::SeqCst) - 1;
        log::debug!("[cast] down ({live} running)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn its_task_runs_until_stop() {
        let running = Cast.start(Services::default()).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!running.task.is_finished(), "the task keeps running");
        Cast.stop(&running).await;
        tokio::task::yield_now().await;
        assert!(running.task.is_finished());
    }

    #[test]
    fn it_asks_for_the_credential() {
        assert_eq!(Cast.needs(), [Need::Credential]);
        assert_eq!(Cast.id(), "cast");
    }
}
