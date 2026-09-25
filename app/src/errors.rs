//! What the user reads when something fails: a short, plain message in a
//! toast. Status codes, response bodies and stream URLs go to the log only.
//! A login that can no longer be refreshed takes the user back to the login
//! page.

use adw::subclass::prelude::*;
use zeke_player::{ErrorKind, PlayerCommand};
use zeke_tidal::TidalError;

use crate::session::describe;
use crate::window::ZekeWindow;

pub const NETWORK: &str = "Can’t reach TIDAL. Check your internet connection.";
pub const LOGIN_EXPIRED: &str = "Your TIDAL login has expired. Sign in again.";

/// A TIDAL request's failure, for people.
pub fn tidal(e: &TidalError) -> &'static str {
    if e.is_auth_expired() {
        return LOGIN_EXPIRED;
    }
    match e {
        TidalError::Network(_) => NETWORK,
        TidalError::Api { status: 429, .. } => "TIDAL is getting too many requests. Try again in a minute.",
        TidalError::Api { status: 404 | 410, .. } => "TIDAL doesn’t have this anymore.",
        TidalError::Api { status: 401 | 403, .. } => "TIDAL doesn’t allow this for your account.",
        TidalError::Api { status: 500.., .. } => "TIDAL is having trouble. Try again later.",
        TidalError::Api { .. } => "TIDAL refused the request.",
        TidalError::Parse(_) => "TIDAL sent something Zeke can’t read.",
        _ => "Something went wrong. The details are in the log.",
    }
}

/// Why playback stopped, for people. `message` is the player's own text,
/// shown only where Zeke wrote it for people (bit-perfect's rate check).
pub fn player(kind: ErrorKind, message: &str) -> String {
    match kind {
        ErrorKind::Network => NETWORK.into(),
        ErrorKind::LoginExpired => LOGIN_EXPIRED.into(),
        ErrorKind::Unplayable => "TIDAL can’t play these tracks right now. Playback stopped.".into(),
        ErrorKind::DeviceBusy => {
            "The audio device is busy: another app is using it. Close that app, or turn off exclusive mode in Preferences."
                .into()
        }
        ErrorKind::UnsupportedRate => match rate_khz(message) {
            Some(khz) => format!(
                "The audio device can’t play {khz} kHz bit-perfect. Turn off bit-perfect mode in Preferences."
            ),
            None => "The audio device can’t play this track bit-perfect. Turn off bit-perfect mode in Preferences."
                .into(),
        },
        ErrorKind::Device => "The audio device stopped working or changed. Check the output in Preferences.".into(),
        ErrorKind::Other => "Couldn’t play this track. The details are in the log.".into(),
    }
}

/// "96" from the engine's "DAC doesn't support 96kHz — …".
fn rate_khz(message: &str) -> Option<&str> {
    let at = message.find("kHz")?;
    let digits = message[..at].rsplit(|c: char| !c.is_ascii_digit()).next()?;
    (!digits.is_empty()).then_some(digits)
}

impl ZekeWindow {
    /// Log a failed request and tell the user: a toast "Couldn’t {what}: …",
    /// or, if the login expired, back to the login page. Returns the message
    /// shown, `None` after the login page.
    pub fn report(&self, what: &str, e: &TidalError) -> Option<&'static str> {
        log::warn!("[app] couldn’t {what}: {}", describe(e));
        if e.is_auth_expired() {
            self.login_expired();
            return None;
        }
        let message = tidal(e);
        self.toast(&format!("Couldn’t {what}. {message}"));
        Some(message)
    }

    /// The session can't be refreshed: pause, and ask for a new login. The
    /// queue stays (the player saved it; a failed resume of a restored
    /// entry keeps its position).
    pub fn login_expired(&self) {
        let imp = self.imp();
        if imp.stack.visible_child_name().as_deref() == Some("login") {
            return;
        }
        log::warn!("[app] the TIDAL login expired; back to the login page");
        self.send(PlayerCommand::Pause);
        imp.sheet.set_open(false);
        self.show_login();
        self.toast(LOGIN_EXPIRED);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_never_carry_details() {
        let url = "https://sp-ad-cf.audio.tidal.com/mediatracks/abc/0.flac?token=secret";
        for e in [
            TidalError::Network(format!("error sending request for url ({url})")),
            TidalError::Api { status: 500, body: url.into() },
            TidalError::Api { status: 404, body: "{}".into() },
            TidalError::Parse(format!("expected value - Body: {url}")),
            TidalError::Io("/home/me/.config/zeke/queue.json: denied".into()),
        ] {
            let m = tidal(&e);
            assert!(!m.contains("http") && !m.contains("tidal.com/") && !m.contains("status"), "{m}");
        }
        assert_eq!(tidal(&TidalError::NotAuthenticated), LOGIN_EXPIRED);
        assert_eq!(tidal(&TidalError::Network("timed out".into())), NETWORK);
    }

    #[test]
    fn player_messages() {
        let rate = zeke_player::unsupported_rate_error(96000);
        assert_eq!(
            player(ErrorKind::UnsupportedRate, &rate),
            "The audio device can’t play 96 kHz bit-perfect. Turn off bit-perfect mode in Preferences."
        );
        assert!(player(ErrorKind::UnsupportedRate, "odd").contains("this track"));
        assert!(player(ErrorKind::DeviceBusy, "device_busy: Device or resource busy").starts_with("The audio device is busy"));
        let other = player(ErrorKind::Other, "playback_error: Internal data stream error (https://x/y)");
        assert!(!other.contains("https"));
        assert_eq!(rate_khz("DAC doesn't support 192kHz — x"), Some("192"));
        assert_eq!(rate_khz("no rate"), None);
    }
}
