# Writing a plugin

A plugin is compiled into Zeke. Each one is a crate under `crates/plugins`,
built only when its Cargo feature is on, and has a switch in Preferences ›
Plugins that is off by default. Zeke runs a plugin while its switch is on
and someone is signed in. It stops the plugin when the switch goes off, on
sign-out and at exit, and restarts it when the account changes. A start
always waits for the stop before it, so there is never more than one
instance. The app reaches a plugin only through the `Plugin` trait in
`crates/plugins/api`; the `cast` plugin is an example.

**1. The crate.** Create `crates/plugins/hello`, named `zeke-plugin-hello`,
and add it to the workspace `members` in the top-level `Cargo.toml`:

```toml
[package]
name = "zeke-plugin-hello"
version.workspace = true
edition.workspace = true
license.workspace = true
publish = false

[dependencies]
log = "0.4"
tokio = { version = "1", features = ["rt", "time"] }
zeke-plugin = { path = "../api" }
```

Add `gtk` and `adw` (same versions as `crates/plugins/cast`) if it has widgets.

**2. The trait.**

```rust
use std::time::Duration;

use zeke_plugin::{Plugin, PluginUi, Services};

pub struct Hello;

/// What a running Hello keeps.
pub struct Running {
    task: tokio::task::JoinHandle<()>,
}

impl Plugin for Hello {
    type Handle = Running;

    fn id(&self) -> &'static str {
        "hello"
    }

    fn name(&self) -> &'static str {
        "Hello"
    }

    async fn start(&self, _services: Services) -> Running {
        let task = tokio::spawn(async {
            loop {
                log::debug!("[hello] still here");
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
        Running { task }
    }

    fn ui(&self, _: &Running) -> PluginUi {
        PluginUi::default()
    }

    async fn stop(&self, running: &Running) {
        running.task.abort();
    }
}
```

- `id` keys the plugin's entry in the settings file, so never change it.
  `name` is the switch's title.
- `start` and `stop` run on the tokio runtime. `start` returns the handle
  that `ui` and `stop` get back. `stop` must end every task the plugin
  started; at exit, Zeke waits at most 2 s for the plugins to stop.
- `needs` lists the services the plugin gets in `Services`; it has a
  default that asks for none (see *The TIDAL session* below).
- `ui` runs on the GTK main loop, after `start`. `PluginUi::page` is shown
  in Preferences under the switch, and `PluginUi::output_section` in the
  player bar's output menu, under the local outputs. Zeke removes both when
  the plugin stops.

**3. The feature.** In `app/Cargo.toml`, add the optional dependency and a
feature named after the plugin:

```toml
[features]
hello = ["dep:zeke-plugin-hello"]

[dependencies]
zeke-plugin-hello = { path = "../crates/plugins/hello", optional = true }
```

Add it to `full` only when it is ready for users: `full` is in the default
build, and every plugin in it gets a switch.

**4. The registry.** Add it to `registry()` in `app/src/plugins/mod.rs`,
the only place in the app that names a plugin:

```rust
#[cfg(feature = "hello")]
Arc::new(zeke_plugin_hello::Hello),
```

**5. Try it.** `ZEKE_DEBUG=1 cargo run -p zeke --features hello`, sign in,
then turn it on in Preferences › Plugins. Before a merge, run `make check`,
and copy its `zeke-plugin-cast` lines in the `Makefile` for the new crate,
so a build without plugins is checked not to pull it in.

**The TIDAL session.** A plugin gets only what it lists in `needs`:

```rust
fn needs(&self) -> &'static [Need] {
    &[Need::Credential]
}
```

With `Need::Credential` in it, `Services::credential` is a `CredentialSource`:
- `current()` returns the access token, country and user id, or `None`
  when no one is signed in;
- `watch()` changes on sign-in, refresh, sign-out and account change;
- `fresh()` refreshes first when the token expires within 5 minutes; call
  it before handing the token to anything else;
- `refresh_now(&failed)` after TIDAL refuses a token: concurrent callers
  share one refresh.

`Credential`'s `Debug` hides the token, so logging one is safe; never log
`access_token` itself. The refresh token never leaves Zeke.

Not there yet: a plugin can't read or write its own settings (its entry
holds only the switch, as `enabled`), and `RemoteTargetFactory`, for
playing the queue somewhere else, has no methods yet (a plugin can already
offer one from `remote_target`).
