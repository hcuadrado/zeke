//! What a plugin is, and what the app gives it.
//!
//! Plugins are compiled in, each behind a Cargo feature of the app, and each
//! has a switch in Preferences, off by default. A plugin runs in two halves:
//! `start` on the tokio runtime (its tasks and listeners), and `ui` on the
//! GTK main loop (its widgets). The app starts it when its switch is on and
//! someone is signed in, and stops it when the switch goes off, on sign-out,
//! and at exit; an account change stops it and starts it again. A start
//! always waits for the stop before it, so there is never more than one
//! instance. The app reaches a plugin only through this trait.

use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub use zeke_tidal::credential::{Credential, CredentialSource};

/// A service a plugin asks for; it gets only what it lists in `needs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// The TIDAL session's access token (`Services::credential`).
    Credential,
}

/// What the app hands a plugin at start.
#[derive(Debug, Default)]
pub struct Services {
    /// Set only for a plugin whose `needs` lists `Need::Credential`.
    pub credential: Option<CredentialSource>,
}

/// Makes remote output targets: somewhere other than this machine's own
/// outputs to play the queue. The player's remote mode will give it its
/// methods; for now a plugin can register one, and the app keeps it while
/// the plugin runs.
pub trait RemoteTargetFactory: Send + Sync {}

/// A plugin's widgets, built on the GTK main loop while it runs. The app
/// removes them when the plugin stops.
#[derive(Default)]
pub struct PluginUi {
    /// Shown in Preferences, under the plugin's switch.
    pub page: Option<gtk::Widget>,
    /// A section of the player bar's output menu, under the local outputs.
    pub output_section: Option<gtk::Widget>,
}

pub trait Plugin: Send + Sync + 'static {
    /// What a running instance keeps; `ui` and `stop` get it back.
    type Handle: Send + Sync + 'static;

    /// Stable: it keys the plugin's settings entry.
    fn id(&self) -> &'static str;

    /// For people: the switch's title.
    fn name(&self) -> &'static str;

    fn needs(&self) -> &'static [Need] {
        &[]
    }

    /// Start an instance, on the tokio runtime.
    fn start(&self, services: Services) -> impl Future<Output = Self::Handle> + Send;

    /// The instance's widgets, on the GTK main loop.
    fn ui(&self, handle: &Self::Handle) -> PluginUi;

    /// The instance's remote target factory, if it offers one.
    fn remote_target(&self, _handle: &Self::Handle) -> Option<Arc<dyn RemoteTargetFactory>> {
        None
    }

    /// Stop the instance, on the tokio runtime: end its tasks and let go of
    /// what it holds. Its data stays.
    fn stop(&self, handle: &Self::Handle) -> impl Future<Output = ()> + Send;
}

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// A running instance of some plugin, as the app holds it.
#[derive(Clone)]
pub struct Handle(Arc<dyn Any + Send + Sync>);

/// `Plugin` for a list of plugins of different types: what the app calls.
/// Every `Plugin` is one.
pub trait DynPlugin: Send + Sync {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    fn needs(&self) -> &'static [Need];
    fn start(self: Arc<Self>, services: Services) -> BoxFuture<Handle>;
    fn ui(&self, handle: &Handle) -> PluginUi;
    fn remote_target(&self, handle: &Handle) -> Option<Arc<dyn RemoteTargetFactory>>;
    fn stop(self: Arc<Self>, handle: Handle) -> BoxFuture<()>;
}

impl<P: Plugin> DynPlugin for P {
    fn id(&self) -> &'static str {
        Plugin::id(self)
    }

    fn name(&self) -> &'static str {
        Plugin::name(self)
    }

    fn needs(&self) -> &'static [Need] {
        Plugin::needs(self)
    }

    fn start(self: Arc<Self>, services: Services) -> BoxFuture<Handle> {
        Box::pin(async move { Handle(Arc::new(Plugin::start(&*self, services).await)) })
    }

    fn ui(&self, handle: &Handle) -> PluginUi {
        Plugin::ui(self, own(handle))
    }

    fn remote_target(&self, handle: &Handle) -> Option<Arc<dyn RemoteTargetFactory>> {
        Plugin::remote_target(self, own(handle))
    }

    fn stop(self: Arc<Self>, handle: Handle) -> BoxFuture<()> {
        Box::pin(async move { Plugin::stop(&*self, own::<P::Handle>(&handle)).await })
    }
}

/// A handle is only ever given back to the plugin that made it.
fn own<H: 'static>(handle: &Handle) -> &H {
    handle.0.downcast_ref().expect("a handle goes back to the plugin that made it")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[derive(Default)]
    struct Counter {
        stops: AtomicUsize,
    }

    struct Target;
    impl RemoteTargetFactory for Target {}

    impl Plugin for Counter {
        type Handle = u32;

        fn id(&self) -> &'static str {
            "counter"
        }

        fn name(&self) -> &'static str {
            "Counter"
        }

        async fn start(&self, services: Services) -> u32 {
            assert!(services.credential.is_none());
            42
        }

        fn ui(&self, _: &u32) -> PluginUi {
            PluginUi::default()
        }

        fn remote_target(&self, handle: &u32) -> Option<Arc<dyn RemoteTargetFactory>> {
            (*handle == 42).then(|| Arc::new(Target) as Arc<dyn RemoteTargetFactory>)
        }

        async fn stop(&self, handle: &u32) {
            assert_eq!(*handle, 42);
            self.stops.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn a_plugin_gets_its_own_handle_back() {
        let counter = Arc::new(Counter::default());
        let plugin: Arc<dyn DynPlugin> = counter.clone();
        assert_eq!((plugin.id(), plugin.name(), plugin.needs()), ("counter", "Counter", &[][..]));
        let handle = Arc::clone(&plugin).start(Services::default()).await;
        assert!(plugin.ui(&handle).page.is_none());
        assert!(plugin.remote_target(&handle).is_some());
        plugin.stop(handle).await;
        assert_eq!(counter.stops.load(Ordering::SeqCst), 1);
    }
}
