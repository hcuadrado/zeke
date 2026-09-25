//! Login (PKCE, browser + paste) and logout.

use std::sync::Arc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::gio;
use zeke_player::PlayerCommand;
use zeke_tidal::commands::auth;
use zeke_tidal::TidalError;

use crate::runtime;
use crate::session::describe;
use crate::window::ZekeWindow;

impl ZekeWindow {
    pub fn show_login(&self) {
        let imp = self.imp();
        imp.pkce.take();
        imp.login_entry.set_text("");
        imp.login_entry.set_sensitive(false);
        imp.login_spinner.set_visible(false);
        imp.open_login_button.set_sensitive(true);
        imp.stack.set_visible_child_name("login");
        imp.open_login_button.grab_focus();
    }

    pub fn show_main(&self) {
        self.imp().stack.set_visible_child_name("main");
    }

    /// Put the saved tokens into the TIDAL client (network: on tokio),
    /// then open Home.
    pub fn restore_session(&self) {
        let state = Arc::clone(&self.session().state);
        let window = self.downgrade();
        runtime::spawn(async move { auth::load_saved_auth(&state).await }, move |result| {
            let Some(window) = window.upgrade() else { return };
            if let Ok(Some(tokens)) = &result {
                window.imp().account.set(tokens.user_id);
            }
            if let Err(e) = result
                && window.report("restore the TIDAL session", &e).is_none()
            {
                return; // the login expired: the login page is up
            }
            // Home needs the session's tokens; it shows its own error if
            // they couldn't be restored.
            window.start_browsing();
        });
    }

    /// Build the PKCE authorize URL, keep its verifier, open the browser.
    pub fn open_login(&self) {
        let params = match auth::start_pkce_browser_login() {
            Ok(p) => p,
            Err(e) => {
                log::error!("[app] building the TIDAL login URL: {}", describe(&e));
                return self.toast("Couldn’t start the TIDAL login. The details are in the log.");
            }
        };
        let imp = self.imp();
        let url = params.authorize_url.clone();
        imp.pkce.replace(Some(params));
        imp.login_entry.set_sensitive(true);
        imp.login_entry.grab_focus();
        let window = self.downgrade();
        gtk::UriLauncher::new(&url).launch(Some(self), gio::Cancellable::NONE, move |result| {
            if let (Some(window), Err(e)) = (window.upgrade(), result) {
                log::warn!("[app] opening the browser failed: {e}");
                window.toast("Couldn’t open a browser for the TIDAL login");
            }
        });
    }

    /// Take the code from the pasted redirect and exchange it for tokens.
    pub fn finish_login(&self) {
        let imp = self.imp();
        let Some(code) = auth::extract_pkce_code(&imp.login_entry.text()) else {
            return self.toast("That doesn’t contain an authorization code. Paste the whole address.");
        };
        let Some(params) = imp.pkce.borrow().clone() else {
            return self.toast("Open the TIDAL login first");
        };
        // Drop focus first: disabling a focused entry makes GTK warn.
        gtk::prelude::GtkWindowExt::set_focus(self, gtk::Widget::NONE);
        imp.login_entry.set_sensitive(false);
        imp.open_login_button.set_sensitive(false);
        imp.login_spinner.set_visible(true);
        let state = Arc::clone(&self.session().state);
        let window = self.downgrade();
        runtime::spawn(
            async move { auth::finish_embedded_pkce(&state, code, params.code_verifier, params.client_unique_key).await },
            move |result| {
                let Some(window) = window.upgrade() else { return };
                let imp = window.imp();
                imp.login_spinner.set_visible(false);
                imp.open_login_button.set_sensitive(true);
                match result {
                    Ok(tokens) => {
                        log::info!("[app] signed in (user id {})", tokens.user_id.map_or("?".into(), |u| u.to_string()));
                        // Back after an expired login, as someone else: the
                        // queue was the other account's.
                        let previous = imp.account.replace(tokens.user_id);
                        if previous.is_some() && previous != tokens.user_id {
                            log::info!("[app] another account signed in; clearing the queue");
                            window.clear_queue();
                        }
                        imp.pkce.take();
                        imp.login_entry.set_text("");
                        window.show_main();
                        window.start_browsing();
                        window.toast("Signed in to TIDAL");
                    }
                    Err(e) => {
                        log::warn!("[app] sign-in failed: {}", describe(&e));
                        imp.login_entry.set_sensitive(true);
                        let why = match &e {
                            TidalError::Api { status: 400 | 401, .. } => {
                                "TIDAL didn’t accept that code. A code works once: open the login again."
                            }
                            other => crate::errors::tidal(other),
                        };
                        window.toast(&format!("Sign-in failed. {why}"));
                    }
                }
            },
        );
    }

    /// Stop playback and empty the queue (the saved one too).
    pub fn clear_queue(&self) {
        self.send(PlayerCommand::Load {
            tracks: Vec::new(),
            start: None,
            album_mode: false,
            shuffle: false,
            repeat: zeke_player::RepeatMode::Off,
        });
    }

    /// Stop playback, forget the session, back to the login page.
    pub fn logout(&self) {
        self.clear_queue();
        self.imp().account.set(None);
        self.imp().sheet.set_open(false);
        let state = Arc::clone(&self.session().state);
        let window = self.downgrade();
        runtime::spawn(async move { auth::logout(&state).await }, move |result| {
            let Some(window) = window.upgrade() else { return };
            window.clear_now_playing();
            window.clear_favorites();
            window.stop_browsing();
            window.show_login();
            match result {
                Ok(()) => window.toast("Logged out"),
                Err(e) => {
                    log::error!("[app] removing the saved session: {}", describe(&e));
                    window.toast("Logged out, but the saved session couldn’t be removed. The details are in the log.");
                }
            }
        });
    }
}
