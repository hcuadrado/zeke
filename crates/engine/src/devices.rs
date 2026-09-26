//! ALSA device names for exclusive mode (Zeke).
//!
//! The device monitor reports devices as `hw:sofhdadsp` or `hw:sofhdadsp,3`,
//! and users type `hw:0,0`. Card numbers can change between boots when a USB
//! DAC is plugged in, so settings store the stable `hw:CARD=<id>,DEV=<n>`
//! form, which ALSA opens directly.

use crate::audio::AudioDevice;

/// A card as an ALSA name gives it: by number or by id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardRef {
    Index(u32),
    Id(String),
}

/// Split an ALSA device name of the `hw` family (`hw`, `plughw`) into card
/// and device: `hw:0,0`, `hw:1`, `hw:sofhdadsp,3`, `hw:CARD=x,DEV=3`,
/// `hw:CARD=x`. The device defaults to 0, as in ALSA. Other plugins
/// (`default`, `dmix`, …) return `None`.
pub fn parse_hw(device: &str) -> Option<(CardRef, u32)> {
    let (plugin, body) = device.split_once(':')?;
    if !matches!(plugin, "hw" | "plughw") {
        return None;
    }
    let mut card = None;
    let mut dev = 0;
    for (i, part) in body.split(',').enumerate() {
        let (key, value) = match part.split_once('=') {
            Some((k, v)) => (Some(k), v),
            None => (None, part),
        };
        match (key, i) {
            (Some("CARD"), _) | (None, 0) => {
                if value.is_empty() {
                    return None;
                }
                card = Some(match value.parse() {
                    Ok(n) => CardRef::Index(n),
                    Err(_) => CardRef::Id(value.to_string()),
                });
            }
            (Some("DEV"), _) | (None, 1) => dev = value.parse().ok()?,
            (Some("SUBDEV"), _) | (None, 2) => {}
            _ => return None,
        }
    }
    Some((card?, dev))
}

/// The device number of a `hw`-family name (`hw:0,31` → 31).
pub fn device_number(device: &str) -> Option<u32> {
    parse_hw(device).map(|(_, dev)| dev)
}

/// Card ids by number, from `/proc/asound/cards` lines like
/// ` 0 [sofhdadsp      ]: sof-hda-dsp - sof-hda-dsp`.
fn parse_cards(cards: &str) -> Vec<(u32, String)> {
    cards
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (num, rest) = line.split_once(' ')?;
            let num = num.parse().ok()?;
            let rest = rest.trim_start().strip_prefix('[')?;
            let id = rest[..rest.find(']')?].trim();
            Some((num, id.to_string()))
        })
        .collect()
}

/// `hw:CARD=<id>,DEV=<n>` for a `hw`-family name, looking card numbers up in
/// `cards` (the contents of `/proc/asound/cards`).
fn stable_name_with(device: &str, cards: &str) -> Option<String> {
    let (card, dev) = parse_hw(device)?;
    let id = match card {
        CardRef::Id(id) => id,
        CardRef::Index(n) => parse_cards(cards).into_iter().find(|(i, _)| *i == n)?.1,
    };
    Some(format!("hw:CARD={id},DEV={dev}"))
}

/// The stable form of an ALSA `hw` name (see the module doc), or `None` if
/// it isn't one or its card is not present.
pub fn stable_name(device: &str) -> Option<String> {
    let cards = std::fs::read_to_string("/proc/asound/cards").unwrap_or_default();
    stable_name_with(device, &cards)
}

/// The card number of a `hw`-family name, looking card ids up in `cards`
/// (the contents of `/proc/asound/cards`).
fn card_index_with(device: &str, cards: &str) -> Option<u32> {
    match parse_hw(device)?.0 {
        CardRef::Index(n) => Some(n),
        CardRef::Id(id) => parse_cards(cards).into_iter().find(|(_, c)| *c == id).map(|(n, _)| n),
    }
}

/// The card number of a `hw`-family name (`hw:CARD=DAC,DEV=0` → 1), or
/// `None` if it isn't one or its card is not present.
pub fn card_index(device: &str) -> Option<u32> {
    match parse_hw(device)?.0 {
        CardRef::Index(n) => Some(n),
        CardRef::Id(_) => card_index_with(device, &std::fs::read_to_string("/proc/asound/cards").ok()?),
    }
}

/// The `/proc/asound` directory of a `hw`-family name's card (`card0`), which
/// holds its PCMs' `info` and `hw_params`.
pub fn proc_card_dir(device: &str) -> Option<String> {
    card_index(device).map(|index| format!("/proc/asound/card{index}"))
}

/// The `id:` line of a PCM's `/proc/asound/cardN/pcmDp/info`, e.g.
/// `HDA Analog (*)` or `HDMI1 (*)`.
fn parse_pcm_id(info: &str) -> Option<&str> {
    info.lines().find_map(|l| l.strip_prefix("id:")).map(str::trim)
}

/// Whether a PCM id names a digital output to a display or receiver (HDMI,
/// DisplayPort, S/PDIF) rather than the speaker/headphone path or a DAC.
fn is_digital_link(pcm_id: &str) -> bool {
    let id = pcm_id.to_ascii_uppercase();
    ["HDMI", "DISPLAYPORT", "DP", "IEC958", "SPDIF", "S/PDIF", "DIGITAL"]
        .iter()
        .any(|k| id.split(|c: char| !c.is_ascii_alphanumeric() && c != '/').any(|w| w.starts_with(k)))
}

/// Default exclusive device: the first of `devices` (as `list_alsa_devices`
/// returns them) whose PCM is analog, in its stable form. `read_info` reads
/// a PCM's `info` file given the device's stable name.
fn pick_analog_with(
    devices: &[AudioDevice],
    stable: impl Fn(&str) -> Option<String>,
    read_info: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    devices.iter().find_map(|d| {
        let name = stable(&d.id)?;
        let info = read_info(&name)?;
        let analog = parse_pcm_id(&info).is_some_and(|id| !is_digital_link(id));
        analog.then_some(name)
    })
}

/// The default exclusive device: the first analog playback device of
/// `devices`, as `hw:CARD=<id>,DEV=<n>`.
pub fn pick_default_device(devices: &[AudioDevice]) -> Option<String> {
    pick_analog_with(devices, stable_name, |name| {
        let dir = proc_card_dir(name)?;
        let dev = device_number(name)?;
        std::fs::read_to_string(format!("{dir}/pcm{dev}p/info")).ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARDS: &str = " 0 [sofhdadsp      ]: sof-hda-dsp - sof-hda-dsp\n                      LENOVO-21KDCTO1WW-ThinkPadX1CarbonGen12\n 1 [DAC            ]: USB-Audio - USB DAC\n";

    #[test]
    fn parses_hw_names() {
        assert_eq!(parse_hw("hw:0,0"), Some((CardRef::Index(0), 0)));
        assert_eq!(parse_hw("hw:1"), Some((CardRef::Index(1), 0)));
        assert_eq!(parse_hw("hw:sofhdadsp"), Some((CardRef::Id("sofhdadsp".into()), 0)));
        assert_eq!(parse_hw("hw:sofhdadsp,3"), Some((CardRef::Id("sofhdadsp".into()), 3)));
        assert_eq!(parse_hw("hw:CARD=sofhdadsp,DEV=31"), Some((CardRef::Id("sofhdadsp".into()), 31)));
        assert_eq!(parse_hw("plughw:CARD=DAC"), Some((CardRef::Id("DAC".into()), 0)));
        assert_eq!(parse_hw("hw:0,1,0"), Some((CardRef::Index(0), 1)));
        assert_eq!(parse_hw("default"), None);
        assert_eq!(parse_hw("dmix:0,0"), None);
        assert_eq!(parse_hw("hw:"), None);
        assert_eq!(parse_hw("hw:0,x"), None);
    }

    #[test]
    fn stable_names_use_the_card_id() {
        assert_eq!(stable_name_with("hw:0,0", CARDS).as_deref(), Some("hw:CARD=sofhdadsp,DEV=0"));
        assert_eq!(stable_name_with("hw:1", CARDS).as_deref(), Some("hw:CARD=DAC,DEV=0"));
        assert_eq!(stable_name_with("hw:sofhdadsp,3", CARDS).as_deref(), Some("hw:CARD=sofhdadsp,DEV=3"));
        assert_eq!(
            stable_name_with("hw:CARD=sofhdadsp,DEV=0", CARDS).as_deref(),
            Some("hw:CARD=sofhdadsp,DEV=0")
        );
        assert_eq!(stable_name_with("hw:7,0", CARDS), None, "no such card");
        assert_eq!(stable_name_with("default", CARDS), None);
    }

    #[test]
    fn card_index_resolves_ids_and_numbers() {
        assert_eq!(card_index_with("hw:CARD=DAC,DEV=0", CARDS), Some(1));
        assert_eq!(card_index_with("hw:sofhdadsp,3", CARDS), Some(0));
        assert_eq!(card_index_with("hw:0,3", CARDS), Some(0));
        assert_eq!(card_index_with("hw:7,0", ""), Some(7), "numbers need no lookup");
        assert_eq!(card_index_with("hw:CARD=Nope,DEV=0", CARDS), None, "unknown id");
        assert_eq!(card_index_with("default", CARDS), None);
    }

    #[test]
    fn digital_links_are_recognised() {
        for id in ["HDMI1 (*)", "HDMI 0", "DisplayPort 2", "DP-1", "IEC958 (S/PDIF)", "HDA Digital (*)"] {
            assert!(is_digital_link(id), "{id}");
        }
        for id in ["HDA Analog (*)", "Deepbuffer HDA Analog (*)", "USB Audio", "ALC287 Analog"] {
            assert!(!is_digital_link(id), "{id}");
        }
    }

    #[test]
    fn picks_the_first_analog_device() {
        let dev = |id: &str| AudioDevice { id: id.into(), name: id.into() };
        // As the device monitor lists them on the target laptop.
        let devices = [dev("hw:sofhdadsp,3"), dev("hw:sofhdadsp"), dev("hw:sofhdadsp,4")];
        let info = |name: &str| {
            Some(match name {
                "hw:CARD=sofhdadsp,DEV=0" => "card: 0\ndevice: 0\nid: HDA Analog (*)\nname: \n".to_string(),
                _ => "card: 0\ndevice: 3\nid: HDMI1 (*)\nname: HDMI 1\n".to_string(),
            })
        };
        let stable = |d: &str| stable_name_with(d, CARDS);
        assert_eq!(pick_analog_with(&devices, stable, info).as_deref(), Some("hw:CARD=sofhdadsp,DEV=0"));
        assert_eq!(pick_analog_with(&devices[2..], stable, info), None, "HDMI only");
        assert_eq!(pick_analog_with(&[], stable, info), None);
    }
}
