//! Zeke: GTK4 + libadwaita front end. It observes player state; it never owns it.

mod badge;
mod browse;
mod covers;
mod errors;
mod hearts;
mod login;
#[cfg(feature = "webview")]
mod login_window;
mod mpris;
mod now_playing;
mod preferences;
mod queue_row;
mod runtime;
mod session;
mod window;

use std::cell::OnceCell;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::{gio, glib};
use zeke_tidal::{AppState, ColorScheme};

use session::Session;
use window::ZekeWindow;

const APP_ID: &str = "io.github.hcuadrado.Zeke";

fn main() -> glib::ExitCode {
    // ZEKE_DEBUG=1 for debug logging of Zeke's crates.
    zeke_tidal::logger::init(std::env::var_os("ZEKE_DEBUG").is_some());
    gio::resources_register_include!("zeke.gresource").expect("failed to register resources");

    // Before the main loop starts: the master key comes from the keyring over
    // blocking D-Bus calls, and the color scheme must be known
    // before the first frame. Only local reads; no network, no engine.
    let state = match AppState::new(&zeke_tidal::config_dir()) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("Zeke can't open its settings: {}", session::describe(&e));
            return glib::ExitCode::FAILURE;
        }
    };
    let settings = state.load_settings().unwrap_or_default();

    let app = adw::Application::builder().application_id(APP_ID).build();
    let session: Rc<OnceCell<Rc<Session>>> = Rc::default();
    let pending = Rc::new(std::cell::RefCell::new(Some((state, settings.clone()))));

    app.connect_startup(glib::clone!(
        #[strong]
        session,
        move |app| {
            gtk::Window::set_default_icon_name(APP_ID);
            app.style_manager().set_color_scheme(adw_scheme(settings.color_scheme));
            setup_actions(app, settings.color_scheme, Rc::clone(&session));
        }
    ));
    app.connect_activate(glib::clone!(
        #[strong]
        session,
        move |app| {
            if let Some(window) = app.active_window() {
                return window.present();
            }
            let Some((state, settings)) = pending.take() else { return };
            let (s, events) = Session::start(state, settings);
            let s = Rc::new(s);
            session.set(Rc::clone(&s)).expect("activated once");
            ZekeWindow::new(app, s, events).present();
            if std::env::var_os("ZEKE_STALLS").is_some() {
                watch_stalls();
            }
        }
    ));
    // Ctrl-C or SIGTERM from a terminal: quit normally, so the player stops
    // and releases the device.
    let (quit_tx, quit_rx) = async_channel::bounded::<()>(1);
    runtime::runtime().spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let (Ok(mut int), Ok(mut term)) = (signal(SignalKind::interrupt()), signal(SignalKind::terminate())) else {
            return;
        };
        tokio::select! {
            _ = int.recv() => {}
            _ = term.recv() => {}
        }
        let _ = quit_tx.send(()).await;
    });
    glib::spawn_future_local(glib::clone!(
        #[weak]
        app,
        async move {
            if quit_rx.recv().await.is_ok() {
                app.quit();
            }
        }
    ));
    app.connect_shutdown(move |_| {
        if let Some(s) = session.get() {
            s.shutdown();
        }
    });
    app.run()
}

/// ZEKE_STALLS=1: log every main-loop stall over a frame (16 ms), measured
/// as the gap between runs of a 4 ms timer. For profiling only: the timer
/// wakes the loop 250 times a second.
fn watch_stalls() {
    let last = std::cell::Cell::new(std::time::Instant::now());
    glib::timeout_add_local(std::time::Duration::from_millis(4), move || {
        let now = std::time::Instant::now();
        let gap = now - last.replace(now);
        if gap > std::time::Duration::from_millis(16) {
            log::info!("[stall] the main loop was blocked for {:.1} ms", gap.as_secs_f64() * 1000.0);
        }
        glib::ControlFlow::Continue
    });
    log::info!("[stall] watching main-loop stalls over 16 ms");
}

fn adw_scheme(scheme: ColorScheme) -> adw::ColorScheme {
    match scheme {
        ColorScheme::System => adw::ColorScheme::Default,
        ColorScheme::Light => adw::ColorScheme::ForceLight,
        ColorScheme::Dark => adw::ColorScheme::ForceDark,
    }
}

fn setup_actions(app: &adw::Application, initial: ColorScheme, session: Rc<OnceCell<Rc<Session>>>) {
    let name = |c: ColorScheme| match c {
        ColorScheme::System => "system",
        ColorScheme::Light => "light",
        ColorScheme::Dark => "dark",
    };
    // "system" | "light" | "dark": radio items in the primary menu and the
    // preferences' Appearance page. Saved in the settings.
    let color_scheme = gio::ActionEntry::builder("color-scheme")
        .parameter_type(Some(glib::VariantTy::STRING))
        .state(name(initial).to_variant())
        .activate(move |app: &adw::Application, action, param| {
            let Some(value) = param.and_then(|p| p.get::<String>()) else {
                return;
            };
            let scheme = match value.as_str() {
                "light" => ColorScheme::Light,
                "dark" => ColorScheme::Dark,
                _ => ColorScheme::System,
            };
            app.style_manager().set_color_scheme(adw_scheme(scheme));
            action.set_state(&name(scheme).to_variant());
            if let Some(s) = session.get() {
                s.set_color_scheme(scheme);
            }
        })
        .build();
    let quit = gio::ActionEntry::builder("quit")
        .activate(|app: &adw::Application, _, _| app.quit())
        .build();
    app.add_action_entries([color_scheme, quit]);
    app.set_accels_for_action("app.quit", &["<Control>q"]);
}
