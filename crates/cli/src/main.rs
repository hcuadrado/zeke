//! Headless test tool (login, play, play-queue) used as a regression harness.
//!
//! Never prints tokens, client secrets or stream URLs: all log output and
//! error text goes through `zeke_tidal::redact`.

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::io::{BufRead, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use zeke_engine::audio::AudioPlayer;
use zeke_engine::audio;
use zeke_engine::events::{self, EngineEvent};
use zeke_engine::pipeline_probe::PipelineProbe;
use zeke_engine::{SignalPath, SignalPathTracker};
use zeke_player::{Config, Player, PlayerCommand, PlayerConfig, PlayerEvent, RepeatMode, Update};
use zeke_tidal::commands::{auth, playback};
use zeke_tidal::{logger, redact};
use zeke_tidal::{AppState, TidalError, Settings};

#[derive(Parser)]
#[command(name = "zeke-cli", about = "Zeke headless test tool")]
struct Cli {
    /// Debug logging for Zeke's crates (engine, TIDAL client)
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in with PKCE: opens the browser, then asks for the redirect URL
    Login,
    /// Forget the saved session
    Logout,
    /// Force a token refresh and save the new tokens
    Refresh,
    /// Show the decrypted settings, with tokens and secrets redacted
    Settings,
    /// List ALSA output devices for --device
    Devices,
    /// Search tracks (to find test tracks for track-info)
    Search {
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: u32,
    },
    /// Show what TIDAL serves for a track at a quality
    TrackInfo {
        id: u64,
        #[arg(long, default_value = "HI_RES_LOSSLESS")]
        quality: String,
    },
    /// Play one track to the end (or until Ctrl-C), logging engine events
    Play {
        id: u64,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Play a queue through the player (gapless), logging every transition
    PlayQueue {
        #[arg(required = true, num_args = 1..)]
        ids: Vec<u64>,
        /// The tracks are this album, in order: album ReplayGain (checked
        /// against each track's metadata)
        #[arg(long, value_name = "ALBUM_ID")]
        album: Option<u64>,
        #[arg(long)]
        shuffle: bool,
        #[arg(long, value_enum, default_value_t = RepeatArg::Off)]
        repeat: RepeatArg,
        /// Index of the first track to play (default: the first, or a
        /// random one with --shuffle)
        #[arg(long)]
        start: Option<usize>,
        /// Once each track plays, seek to this many seconds before its end
        /// (to reach transitions quickly; not for gapless listening tests)
        #[arg(long, value_name = "SECS")]
        jump: Option<f64>,
        /// Only --jump in the first track, then play through
        #[arg(long, requires = "jump")]
        jump_first: bool,
        #[command(flatten)]
        output: OutputArgs,
    },
}

/// Output options shared by `play` and `play-queue`.
#[derive(Args)]
struct OutputArgs {
    /// Quality ceiling (default: the saved max_quality)
    #[arg(long)]
    quality: Option<String>,
    /// ALSA device for exclusive mode, e.g. hw:0,0 (default: the saved one)
    #[arg(long)]
    device: Option<String>,
    /// Exclusive ALSA output (default: the saved setting, on)
    #[arg(long, overrides_with = "no_exclusive")]
    exclusive: bool,
    /// Normal output through the system mixer (PipeWire)
    #[arg(long)]
    no_exclusive: bool,
    /// Bit-perfect output: no resampling or format conversion
    #[arg(long, overrides_with = "no_bit_perfect")]
    bit_perfect: bool,
    /// Resample and convert as needed (overrides a saved bit_perfect)
    #[arg(long)]
    no_bit_perfect: bool,
    /// Stop after this many seconds
    #[arg(long, value_name = "SECS")]
    stop_after: Option<u64>,
}

impl OutputArgs {
    /// These flags over the saved settings.
    fn resolve(self, saved: &Settings) -> PlayOptions {
        PlayOptions {
            quality: self.quality.unwrap_or(saved.max_quality.clone()),
            device: self.device.or(saved.exclusive_device.clone()),
            exclusive: if self.no_exclusive {
                false
            } else {
                self.exclusive || saved.exclusive_mode
            },
            bit_perfect: !self.no_bit_perfect && (self.bit_perfect || saved.bit_perfect),
            stop_after: self.stop_after.map(Duration::from_secs),
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum RepeatArg {
    Off,
    All,
    One,
}

impl From<RepeatArg> for RepeatMode {
    fn from(r: RepeatArg) -> Self {
        match r {
            RepeatArg::Off => RepeatMode::Off,
            RepeatArg::All => RepeatMode::All,
            RepeatArg::One => RepeatMode::One,
        }
    }
}

/// Errors are shown redacted, without response bodies: `log_safe` drops
/// them from API errors, and parse errors are cut where the body or
/// manifest (which holds stream URLs) is appended.
fn show(e: &TidalError) -> String {
    let msg = e.log_safe();
    let msg = match e {
        TidalError::Parse(_) => [" - Body:", " - Manifest:"]
            .iter()
            .filter_map(|cut| msg.find(cut))
            .min()
            .map_or(msg.clone(), |at| format!("{} (response body not shown)", &msg[..at])),
        _ => msg,
    };
    redact::redact(&msg)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    logger::init(cli.verbose);
    // Opened before the runtime starts: the master key comes from the
    // keyring over blocking D-Bus calls, which must not run on a tokio thread.
    let state = match cli.command {
        Command::Devices => None,
        _ => match AppState::new(&zeke_tidal::config_dir()) {
            Ok(state) => Some(Arc::new(state)),
            Err(e) => {
                eprintln!("error: {}", show(&e));
                return ExitCode::FAILURE;
            }
        },
    };
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    match rt.block_on(run(cli.command, state)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("error: {}", redact::redact(&msg));
            ExitCode::FAILURE
        }
    }
}

async fn run(command: Command, state: Option<Arc<AppState>>) -> Result<(), String> {
    let Some(state) = state else {
        return devices();
    };
    match command {
        Command::Login => login(&state).await,
        Command::Logout => {
            auth::logout(&state).await.map_err(|e| show(&e))?;
            println!("Logged out.");
            Ok(())
        }
        Command::Refresh => refresh(&state).await,
        Command::Settings => print_settings(&state),
        Command::Devices => unreachable!("handled above"),
        Command::Search { query, limit } => search(&state, &query, limit).await,
        Command::TrackInfo { id, quality } => track_info(&state, id, &quality).await,
        Command::Play { id, output } => {
            let opts = output.resolve(&state.load_settings().unwrap_or_default());
            play(&state, id, opts).await
        }
        Command::PlayQueue { ids, album, shuffle, repeat, start, jump, jump_first, output } => {
            let opts = output.resolve(&state.load_settings().unwrap_or_default());
            let queue = QueueOptions { ids, album, shuffle, repeat: repeat.into(), start, jump, jump_first };
            play_queue(state, queue, opts).await
        }
    }
}

async fn require_session(state: &AppState) -> Result<(), String> {
    match auth::load_saved_auth(state).await.map_err(|e| show(&e))? {
        Some(_) => Ok(()),
        None => Err("not logged in: run `zeke-cli login`".into()),
    }
}

async fn login(state: &AppState) -> Result<(), String> {
    let params = auth::start_pkce_browser_login().map_err(|e| show(&e))?;
    println!("Sign in to TIDAL in your browser. If it didn't open, open this URL:\n");
    println!("{}\n", params.authorize_url);
    let _ = std::process::Command::new("xdg-open")
        .arg(&params.authorize_url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    print!("After signing in, paste the address-bar URL (https://tidal.com/android/login/auth?code=…): ");
    std::io::stdout().flush().ok();
    let mut pasted = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut pasted)
        .map_err(|e| e.to_string())?;
    let code = auth::extract_pkce_code(&pasted)
        .ok_or("could not find an authorization code in the pasted text")?;
    let tokens = auth::finish_embedded_pkce(
        state,
        code,
        params.code_verifier,
        params.client_unique_key,
    )
    .await
    .map_err(|e| show(&e))?;
    let method = state.load_settings().map(|s| s.auth_method);
    println!(
        "Logged in (user id {}), auth_method = {:?}.",
        tokens.user_id.map_or("?".into(), |u| u.to_string()),
        method
    );
    Ok(())
}

async fn refresh(state: &AppState) -> Result<(), String> {
    require_session(state).await?;
    let before = state.load_settings().and_then(|s| s.auth_tokens);
    let tokens = auth::refresh_tidal_auth(state).await.map_err(|e| show(&e))?;
    let after = state.load_settings().and_then(|s| s.auth_tokens);
    let changed = match (&before, &after) {
        (Some(b), Some(a)) => b.access_token != a.access_token,
        _ => false,
    };
    let saved = after.is_some_and(|a| a.access_token == tokens.access_token);
    println!(
        "Refreshed: expires_in={}s, stored access token replaced={changed}, new token saved={saved}.",
        tokens.expires_in
    );
    Ok(())
}

fn print_settings(state: &AppState) -> Result<(), String> {
    let Some(s) = state.load_settings() else {
        return Err(format!("no readable settings at {}", state.settings_path.display()));
    };
    let mut v = serde_json::to_value(&s).map_err(|e| e.to_string())?;
    if let Some(t) = s.auth_tokens {
        v["auth_tokens"] = serde_json::json!({
            "access_token": "<redacted>",
            "refresh_token": "<redacted>",
            "expires_in": t.expires_in,
            "token_type": t.token_type,
            "user_id": t.user_id,
        });
    }
    for key in ["client_id", "client_secret"] {
        if v[key].as_str().is_some_and(|s| !s.is_empty()) {
            v[key] = "<redacted>".into();
        }
    }
    if v["proxy"]["password"].as_str().is_some_and(|p| !p.is_empty()) {
        v["proxy"]["password"] = "<redacted>".into();
    }
    println!("{}", state.settings_path.display());
    println!("{}", serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?);
    Ok(())
}

fn devices() -> Result<(), String> {
    for d in zeke_engine::audio::list_alsa_devices()? {
        println!("{:<10} {}", d.id, d.name);
    }
    Ok(())
}

async fn search(state: &AppState, query: &str, limit: u32) -> Result<(), String> {
    require_session(state).await?;
    let results = state
        .tidal_client
        .lock()
        .await
        .search(query, limit)
        .await
        .map_err(|e| show(&e))?;
    for t in results.tracks {
        let artist = t.artist.as_ref().map_or("?", |a| a.name.as_str());
        let album = t.album.as_ref().map_or("?", |a| a.title.as_str());
        println!(
            "{:<11} {:<16} {artist} - {} [{album}]",
            t.id,
            t.audio_quality.as_deref().unwrap_or("-"),
            t.title
        );
    }
    Ok(())
}

async fn track_info(state: &AppState, id: u64, quality: &str) -> Result<(), String> {
    require_session(state).await?;
    let mut client = state.tidal_client.lock().await;
    let meta = client.get_track(id).await.map_err(|e| show(&e))?;
    let info = client
        .get_stream_url(id, quality)
        .await
        .map_err(|e| show(&e))?;
    drop(client);

    let artist = meta["artist"]["name"].as_str().unwrap_or("?");
    let title = meta["title"].as_str().unwrap_or("?");
    let tags = meta["mediaMetadata"]["tags"].to_string();
    let na = || "-".to_string();
    println!("track        {id}: {artist} - {title}");
    println!(
        "album        {}: {} (track {} of volume {})",
        meta["album"]["id"].as_u64().map_or("?".into(), |a| a.to_string()),
        meta["album"]["title"].as_str().unwrap_or("?"),
        meta["trackNumber"].as_u64().map_or("?".into(), |n| n.to_string()),
        meta["volumeNumber"].as_u64().map_or("?".into(), |n| n.to_string()),
    );
    println!(
        "duration     {}s",
        meta["duration"].as_u64().map_or("?".into(), |d| d.to_string())
    );
    println!("catalog tags {tags}");
    println!("requested    {quality}");
    println!("audioQuality {}", info.audio_quality.clone().unwrap_or_else(na));
    println!("bitDepth     {}", info.bit_depth.map_or_else(na, |b| b.to_string()));
    println!("sampleRate   {}", info.sample_rate.map_or_else(na, |r| r.to_string()));
    println!("codec        {}", info.codec.clone().unwrap_or_else(na));
    println!("manifest     {}", info.manifest_mime_type.clone().unwrap_or_else(na));
    println!("encryption   {}", info.encryption_type.clone().unwrap_or_else(na));
    Ok(())
}

struct PlayOptions {
    quality: String,
    device: Option<String>,
    exclusive: bool,
    bit_perfect: bool,
    stop_after: Option<Duration>,
}

/// One line per distinct signal-path state.
fn signal_path_line(p: &SignalPath) -> String {
    let fmt = |f: &Option<String>, r: Option<u32>, c: Option<u32>| match (f, r) {
        (Some(f), Some(r)) => format!("{f} {r} Hz {}ch", c.unwrap_or(0)),
        _ => "-".into(),
    };
    let mut line = format!(
        "backend={} exclusive={} bit_perfect={} decoded=[{}] output=[{}]",
        p.backend.as_deref().unwrap_or("-"),
        p.exclusive_mode,
        p.bit_perfect,
        fmt(&p.decoded_format, p.decoded_rate, p.decoded_channels),
        fmt(&p.output_format, p.output_rate, p.output_channels),
    );
    if let (Some(from), Some(to)) = (p.resampled_from, p.resampled_to) {
        line += &format!(" resampled={from}->{to} (in engine)");
    }
    if let (Some(from), Some(to)) = (&p.promoted_from, &p.promoted_to) {
        line += &format!(" promoted={from}->{to}");
    }
    if let (Some(from), Some(to)) = (&p.format_fallback_from, &p.format_fallback_to) {
        line += &format!(" format_fallback={from}->{to}");
    }
    if let Some(d) = &p.dac {
        line += &format!(
            " dac=[{} {} {} Hz {}ch {:?}]",
            d.card_name, d.format, d.rate, d.channels, d.state
        );
    }
    line
}

fn check_output(opts: &PlayOptions) -> Result<(), String> {
    if opts.bit_perfect && !opts.exclusive {
        return Err("bit-perfect needs exclusive mode".into());
    }
    if opts.exclusive && opts.device.is_none() {
        return Err("exclusive mode needs --device (see `zeke-cli devices`), e.g. hw:0,0".into());
    }
    Ok(())
}

type Engine = (Arc<AudioPlayer>, Arc<SignalPathTracker>, async_channel::Receiver<EngineEvent>);

/// The engine, configured for `opts`, its signal path and its event stream.
fn start_engine(opts: &PlayOptions, saved: &Settings) -> Result<Engine, String> {
    log::info!(
        "[cli] output: exclusive={} bit_perfect={} device={}",
        opts.exclusive,
        opts.bit_perfect,
        opts.device.as_deref().unwrap_or("system mixer")
    );
    let (tx, rx) = events::channel();
    let signal_path = Arc::new(SignalPathTracker::new(tx.clone()));
    signal_path.set_audio_modes(opts.exclusive, opts.bit_perfect);
    signal_path.set_normalization_enabled(saved.volume_normalization);
    let player = Arc::new(AudioPlayer::new(tx, Arc::clone(&signal_path), saved.proxy.clone()));
    player.set_exclusive_mode(opts.exclusive, opts.device.clone())?;
    player.set_bit_perfect(opts.bit_perfect)?;
    player.set_volume(saved.volume)?;
    Ok((player, signal_path, rx))
}

async fn play(state: &AppState, id: u64, opts: PlayOptions) -> Result<(), String> {
    require_session(state).await?;
    let saved = state.load_settings().unwrap_or_default();
    check_output(&opts)?;

    let (stream, uri, norm_gain, _rg, _peak, is_dash) = playback::resolve_play_uri(
        &state.tidal_client,
        &opts.quality,
        saved.volume_normalization,
        id,
        true,
    )
    .await
    .map_err(|e| show(&e))?;
    log::info!(
        "[cli] track {id}: audioQuality={} bitDepth={} sampleRate={} codec={} manifest={} encryption={} dash={is_dash}",
        stream.audio_quality.as_deref().unwrap_or("-"),
        stream.bit_depth.map_or("-".into(), |b| b.to_string()),
        stream.sample_rate.map_or("-".into(), |r| r.to_string()),
        stream.codec.as_deref().unwrap_or("-"),
        stream.manifest_mime_type.as_deref().unwrap_or("-"),
        stream.encryption_type.as_deref().unwrap_or("-"),
    );
    let (player, signal_path, rx) = start_engine(&opts, &saved)?;
    let probe = PipelineProbe::new(signal_path, Arc::clone(&player));
    player.set_normalization_gain(norm_gain)?;
    player.play_url(&uri, None)?;

    let deadline = opts.stop_after.map(|d| tokio::time::Instant::now() + d);
    let stop_at = tokio::time::sleep_until(deadline.unwrap_or_else(|| {
        tokio::time::Instant::now() + Duration::from_secs(u64::from(u32::MAX))
    }));
    tokio::pin!(stop_at);
    // One future for the whole loop, so a Ctrl-C that lands while a branch
    // body runs is still seen on the next pass.
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let mut last_path = String::new();
    let outcome = loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(EngineEvent::SignalPathChanged(p)) => {
                    let line = signal_path_line(&p);
                    if line != last_path {
                        log::info!("[signal-path] {line}");
                        last_path = line;
                    }
                }
                Ok(EngineEvent::AudioResampled { from, to }) => {
                    log::info!("[event] audio-resampled {from} -> {to} (engine resampler)");
                }
                Ok(EngineEvent::AudioBitDepthChanged { from, to }) => {
                    log::info!("[event] audio-bit-depth-changed {from} -> {to}");
                }
                Ok(EngineEvent::AudioError { kind, message }) => {
                    let msg = message.map(|m| format!(": {m}")).unwrap_or_default();
                    log::error!("[event] audio-error {kind}{msg}");
                    break Err(format!("{kind}{msg}"));
                }
                Ok(EngineEvent::TrackFinished) => {
                    log::info!("[event] track-finished");
                    break Ok(());
                }
                Ok(EngineEvent::TrackAdvanced { track_id, .. }) => {
                    log::info!("[event] track-advanced to {track_id}");
                }
                Err(_) => break Err("engine event channel closed".into()),
            },
            _ = tick.tick() => {
                let probe = &probe;
                tokio::task::block_in_place(|| probe.refresh());
                let pos = player.get_position().unwrap_or(0.0);
                log::info!("[cli] position {pos:.1}s");
            }
            _ = &mut stop_at, if deadline.is_some() => {
                log::info!("[cli] --stop-after reached");
                break Ok(());
            }
            _ = &mut ctrl_c => {
                log::info!("[cli] interrupted");
                break Ok(());
            }
        }
    };
    player.stop().ok();
    outcome
}

struct QueueOptions {
    ids: Vec<u64>,
    album: Option<u64>,
    shuffle: bool,
    repeat: RepeatMode,
    start: Option<usize>,
    jump: Option<f64>,
    jump_first: bool,
}

/// The ALSA writer's silence writes and xruns so far, for the log.
fn writer_counters() -> String {
    let (silence, xruns) = audio::writer_counters();
    format!("writer: silence writes {silence}, xruns {xruns}")
}

fn secs(x: Option<f64>) -> String {
    x.map_or("unknown".into(), |s| format!("{s:.3} s"))
}

/// Plays a queue through `crates/player` and logs every
/// state change, prefetch step and transition.
async fn play_queue(state: Arc<AppState>, mut q: QueueOptions, opts: PlayOptions) -> Result<(), String> {
    require_session(&state).await?;
    check_output(&opts)?;
    if let Some(start) = q.start.filter(|&s| s >= q.ids.len()) {
        return Err(format!("--start {start} is past the end of a {}-track queue", q.ids.len()));
    }
    let saved = state.load_settings().unwrap_or_default();

    // Show the queue, and hold --album to its word: album gain is only right
    // for tracks that really are that album.
    {
        let mut client = state.tidal_client.lock().await;
        for (i, &id) in q.ids.iter().enumerate() {
            let meta = client.get_track(id).await.map_err(|e| show(&e))?;
            let album_id = meta["album"]["id"].as_u64();
            log::info!(
                "[cli] queue {i}: {id} {} - {} [{}] {}s",
                meta["artist"]["name"].as_str().unwrap_or("?"),
                meta["title"].as_str().unwrap_or("?"),
                meta["album"]["title"].as_str().unwrap_or("?"),
                meta["duration"].as_u64().map_or("?".into(), |d| d.to_string()),
            );
            if let Some(want) = q.album.filter(|&want| album_id != Some(want)) {
                return Err(format!(
                    "track {id} is not on album {want} (TIDAL says album {})",
                    album_id.map_or("?".into(), |a| a.to_string())
                ));
            }
        }
    }
    log::info!(
        "[cli] shuffle={} repeat={:?} start={:?} gain={} gapless={} normalization={}",
        q.shuffle,
        q.repeat,
        q.start,
        if q.album.is_some() { "album" } else { "track" },
        saved.gapless,
        saved.volume_normalization
    );

    let (engine, _signal_path, rx) = start_engine(&opts, &saved)?;
    engine.set_gapless(saved.gapless)?;
    let player = Player::spawn(
        Arc::clone(&state),
        Arc::clone(&engine),
        rx,
        PlayerConfig {
            core: Config {
                gapless: saved.gapless,
                normalization: saved.volume_normalization,
                max_quality: opts.quality.clone(),
                bit_perfect: opts.bit_perfect,
                ..Config::default()
            },
            seed: None,
            // The app's saved queue is the app's; the harness leaves it alone.
            queue_file: None,
        },
    );
    // The engine is configured already; this gives the player the output
    // (and, in bit-perfect mode, the device's rates for its early check).
    player
        .commands
        .send(PlayerCommand::SetOutput {
            exclusive: opts.exclusive,
            device: opts.device.clone(),
            bit_perfect: opts.bit_perfect,
        })
        .await
        .map_err(|_| "player stopped".to_string())?;
    player
        .commands
        .send(PlayerCommand::Load {
            tracks: zeke_player::QueueTrack::from_ids(&q.ids),
            start: q.start,
            album_mode: q.album.is_some(),
            shuffle: q.shuffle,
            repeat: q.repeat,
        })
        .await
        .map_err(|_| "player stopped".to_string())?;

    let deadline = opts.stop_after.map(|d| tokio::time::Instant::now() + d);
    let stop_at = tokio::time::sleep_until(deadline.unwrap_or_else(|| {
        tokio::time::Instant::now() + Duration::from_secs(u64::from(u32::MAX))
    }));
    tokio::pin!(stop_at);
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    let mut last_path = String::new();
    // --jump: the seek target for the track now playing, sent once it plays
    // (a seek before the pipeline has prerolled is dropped by GStreamer).
    let mut jump_to: Option<f64> = None;
    let mut jump_check = tokio::time::interval(Duration::from_millis(500));
    let outcome = loop {
        tokio::select! {
            upd = player.updates.recv() => match upd {
                Ok(Update::Player(ev)) => match ev {
                    PlayerEvent::State(s) => log::info!("[player] state {s:?}"),
                    PlayerEvent::TrackStarted { item, index, len, via, summary, duration, .. } => {
                        log::info!(
                            "[player] now playing {} ({}/{len}, qid {}) via {via:?}: {} length {} ({})",
                            item.track_id,
                            index + 1,
                            item.qid,
                            summary.as_deref().unwrap_or("-"),
                            secs(duration),
                            writer_counters(),
                        );
                        jump_to = q.jump.zip(duration).map(|(j, d)| (d - j).max(0.0));
                        if q.jump_first {
                            q.jump = None;
                        }
                    }
                    PlayerEvent::PrefetchStarted { item, remaining } => log::info!(
                        "[player] resolving next track {} now, {} before the current one ends",
                        item.track_id,
                        secs(remaining)
                    ),
                    PlayerEvent::PrefetchArmed { item, remaining, summary } => log::info!(
                        "[player] next track {} resolved and armed ({summary}), {} before the current one ends",
                        item.track_id,
                        secs(remaining)
                    ),
                    PlayerEvent::PrefetchCleared { item, reason } => {
                        log::info!("[player] next-track slot for {} cleared: {reason}", item.track_id)
                    }
                    PlayerEvent::PrefetchFailed { item, error } => {
                        log::warn!("[player] could not resolve next track {}: {error}", item.track_id)
                    }
                    PlayerEvent::PrefetchNotArmed { item, reason } => {
                        log::info!("[player] next track {} resolved, not armed: {reason}", item.track_id)
                    }
                    PlayerEvent::QueueChanged { .. } | PlayerEvent::Position(_) => {}
                    PlayerEvent::Skipped { item, error } => {
                        log::warn!("[player] skipped unplayable track {}: {error}", item.track_id)
                    }
                    PlayerEvent::QueueEnded => {
                        log::info!("[player] end of queue ({})", writer_counters());
                        break Ok(());
                    }
                    PlayerEvent::Error { kind, message } => {
                        log::error!("[player] stopped on error ({kind:?}): {message}");
                        break Err(message);
                    }
                },
                Ok(Update::Engine(EngineEvent::SignalPathChanged(p))) => {
                    let line = signal_path_line(&p);
                    if line != last_path {
                        log::info!("[signal-path] {line}");
                        last_path = line;
                    }
                }
                Ok(Update::Engine(EngineEvent::AudioResampled { from, to })) => {
                    log::info!("[event] audio-resampled {from} -> {to} (engine resampler)");
                }
                Ok(Update::Engine(EngineEvent::AudioBitDepthChanged { from, to })) => {
                    log::info!("[event] audio-bit-depth-changed {from} -> {to}");
                }
                Ok(Update::Engine(_)) => {}
                Err(_) => break Err("player stopped".into()),
            },
            _ = tick.tick() => {
                let engine = &engine;
                let pos = tokio::task::block_in_place(|| engine.get_position()).unwrap_or(0.0);
                log::info!("[cli] position {pos:.1}s ({})", writer_counters());
            }
            _ = jump_check.tick(), if jump_to.is_some() => {
                let engine = &engine;
                let pos = tokio::task::block_in_place(|| engine.get_position()).unwrap_or(0.0);
                if pos > 1.0 {
                    let to = jump_to.take().expect("guarded");
                    log::info!("[cli] --jump: seeking to {to:.1}s");
                    let _ = player.commands.send(PlayerCommand::Seek(to)).await;
                }
            }
            _ = &mut stop_at, if deadline.is_some() => {
                log::info!("[cli] --stop-after reached");
                break Ok(());
            }
            _ = &mut ctrl_c => {
                log::info!("[cli] interrupted");
                break Ok(());
            }
        }
    };
    player.shutdown().await;
    log::info!("[cli] engine stopped, device released");
    outcome
}
