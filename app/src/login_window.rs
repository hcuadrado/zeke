//! TIDAL's login page in a window of Zeke's own (WebKitGTK), the way the
//! TIDAL desktop app and SONE do it. The login ends in a redirect to an
//! Android deep link that carries the authorization code; the window catches
//! that navigation instead of following it, so there is nothing to copy.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use webkit6::prelude::*;
use zeke_tidal::commands::auth;

pub enum Outcome {
    /// The redirect arrived; the window is closing.
    Code(String),
    /// The user closed the window first.
    Cancelled,
    /// The page failed to load (message from WebKit).
    Failed(String),
}

/// Open TIDAL's login at `authorize_url`; `done` runs once, on the main
/// thread, when the login ends one way or another.
pub fn open(parent: &impl IsA<gtk::Window>, authorize_url: &str, done: impl Fn(Outcome) + 'static) {
    // Ephemeral: cookies live as long as the window. Signing in again asks
    // for the password again, and nothing of TIDAL's site stays on disk.
    let session = webkit6::NetworkSession::new_ephemeral();
    // No media capture: TIDAL's human check probes for a camera, and the
    // probe alone makes the desktop ask "Zeke wants to access your camera"
    // before any permission request reaches us. The login works without.
    let settings = webkit6::Settings::builder()
        .enable_media_stream(false)
        .enable_webrtc(false)
        .build();
    let webview = webkit6::WebView::builder().network_session(&session).settings(&settings).build();
    // Anything else a page may ask for (notifications, location) is denied.
    webview.connect_permission_request(|_, request| {
        request.deny();
        true
    });

    let window = adw::Window::builder()
        .title("Sign in to TIDAL")
        .transient_for(parent)
        .modal(true)
        .default_width(560)
        .default_height(760)
        .build();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&webview));
    window.set_content(Some(&view));

    // `done` runs once: the first of redirect, load failure or close wins.
    let done = Rc::new(done);
    let finished = Rc::new(Cell::new(false));
    let finish = {
        let window = window.downgrade();
        move |outcome: Outcome| {
            if finished.replace(true) {
                return;
            }
            done(outcome);
            // Closing from inside a WebKit signal handler tears the view
            // down under the signal; do it from the main loop instead.
            let window = window.clone();
            glib::idle_add_local_once(move || {
                if let Some(window) = window.upgrade() {
                    window.close();
                }
            });
        }
    };

    webview.connect_decide_policy({
        let finish = finish.clone();
        move |_, decision, kind| {
            if kind != webkit6::PolicyDecisionType::NavigationAction {
                return false;
            }
            let Some(navigation) = decision.downcast_ref::<webkit6::NavigationPolicyDecision>() else {
                return false;
            };
            let uri = navigation.navigation_action().and_then(|a| a.request()).and_then(|r| r.uri());
            let Some(code) = uri.as_deref().and_then(auth::redirect_code) else {
                return false; // any other page: WebKit's default (load it)
            };
            decision.ignore();
            finish(Outcome::Code(code));
            true
        }
    });
    webview.connect_load_failed({
        let finish = finish.clone();
        move |_, _, uri, error| {
            // An interrupted load isn't a failure: our ignore() above
            // reports as a policy change, a link that opens elsewhere as
            // cancelled.
            if error.matches(webkit6::NetworkError::Cancelled)
                || error.matches(webkit6::PolicyError::FrameLoadInterruptedByPolicyChange)
            {
                return false;
            }
            finish(Outcome::Failed(format!("{uri}: {error}")));
            true
        }
    });
    window.connect_close_request(move |_| {
        finish(Outcome::Cancelled);
        glib::Propagation::Proceed
    });

    webview.load_uri(authorize_url);
    window.present();
}
