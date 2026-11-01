// SPDX-FileCopyrightText: 2025-2026 Mitchel Stewart
// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! The Android core: the state `livi-ui` shows and the actions it asks for. It speaks the same
//! protocol as `livi-core` over an in-process stream, so the UI cannot tell the two apart. Behind
//! it run the Android Auto stack, an in-process helper for USB and the media playback.
//!
//! The hub mirrors `livi-core`'s hub without its desktop services.

use std::io;
use std::os::unix::net::UnixStream as StdStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use livi_aa_stack::bridge::NowPlaying as AaNowPlaying;
use livi_aa_stack::commands::{Command, TouchAction, TouchItem};
use livi_aa_stack::config::{AaConfig, Addresses, Codecs, from_livi};
use livi_aa_stack::driver::SessionCmd;
use livi_aa_stack::helper_sock::AaHelperSock;
use livi_aa_stack::manager::{self, AaCmd, AaEvent, AaHandle, SessionId};
use livi_aa_stack::sensors::Sensor;
use livi_core_proto::PROTOCOL;
use livi_core_proto::config::{Config, apply_patch, defaults};
use livi_core_proto::frame::{Decoder, FrameError, encode};
use livi_core_proto::input::{Input, Phase};
use livi_core_proto::message::{Action, FromCore, MediaControl, ToCore};
use livi_core_proto::patch::diff;
use livi_core_proto::state::{
    DeviceStatus, DeviceView, Front, NowPlaying, PerScreen, Protocol, Screen, Sessions, State,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, watch};

use crate::android::java::Java;
use crate::session::helper::Helper;
use crate::stream::media::{AndroidMedia, feed_path_in};
use crate::stream::video;
use crate::wireless;
use crate::wireless::devices;

const PATCH_BACKLOG: usize = 256;

type Frame = Arc<Vec<u8>>;

struct Inner {
    state: State,
    value: Value,
    rev: u64,
}

struct Hub {
    inner: Mutex<Inner>,
    patches: broadcast::Sender<Frame>,
    config_file: PathBuf,
    aa_config: watch::Sender<AaConfig>,
    /// What the wireless mode is asked to be, see `wireless::watch`.
    wireless: watch::Sender<wireless::Plan>,
    /// Whether H.265 is offered to phones. It is once the platform is found to have a hardware
    /// decoder for it.
    hevc: AtomicBool,
}

/// What the stack is told about the screen and the codecs, derived from the settings. H.264 is
/// always offered. H.265 is offered only with a hardware decoder, since the phone chooses the
/// codec and a software one may not keep up at high resolutions. It has not been tried.
fn aa_config(config: &Config, hevc: bool) -> AaConfig {
    from_livi(
        config,
        Codecs {
            hevc,
            ..Codecs::default()
        },
        config.appearance_mode.night_mode(),
        &Addresses::default(),
    )
}

impl Hub {
    fn new(state: State, config_file: PathBuf) -> Self {
        let value = serde_json::to_value(&state).unwrap_or(Value::Null);
        let aa_config = watch::channel(aa_config(&state.config, false)).0;
        let wireless = watch::channel(wireless::Plan::of(&state.config)).0;
        Self {
            inner: Mutex::new(Inner {
                state,
                value,
                rev: 0,
            }),
            patches: broadcast::channel(PATCH_BACKLOG).0,
            config_file,
            aa_config,
            wireless,
            hevc: AtomicBool::new(false),
        }
    }

    /// Offers H.265 to phones from now on.
    fn offer_hevc(&self) {
        if !self.hevc.swap(true, Ordering::Relaxed) {
            self.aa_config.send_replace(aa_config(&self.config(), true));
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn welcome(&self) -> (FromCore, broadcast::Receiver<Frame>) {
        let inner = self.lock();
        let msg = FromCore::Welcome {
            protocol: PROTOCOL,
            version: env!("CARGO_PKG_VERSION").into(),
            rev: inner.rev,
            state: Box::new(inner.state.clone()),
        };
        (msg, self.patches.subscribe())
    }

    fn config(&self) -> Config {
        self.lock().state.config.clone()
    }

    fn front(&self) -> Front {
        self.lock().state.front.main
    }

    fn update(&self, change: impl FnOnce(&mut State)) {
        let mut inner = self.lock();
        change(&mut inner.state);
        let value = serde_json::to_value(&inner.state).unwrap_or(Value::Null);
        let ops = diff(&inner.value, &value);
        if ops.is_empty() {
            return;
        }
        inner.rev += 1;
        inner.value = value;
        match encode(&FromCore::Patch {
            rev: inner.rev,
            ops,
        }) {
            Ok(frame) => {
                let _ = self.patches.send(Arc::new(frame));
            }
            Err(e) => eprintln!("[core] patch {} not sent: {e}", inner.rev),
        }
    }

    fn set_config(&self, patch: &Value) -> Result<(), String> {
        let next = apply_patch(&self.config(), patch)?;
        let text = serde_json::to_string_pretty(&next).map_err(|e| e.to_string())?;
        std::fs::write(&self.config_file, text).map_err(|e| format!("not saved: {e}"))?;
        self.aa_config
            .send_replace(aa_config(&next, self.hevc.load(Ordering::Relaxed)));
        self.wireless.send_if_modified(|plan| {
            let next = wireless::Plan::of(&next);
            (*plan != next) && {
                *plan = next;
                true
            }
        });
        self.update(|s| s.config = next);
        Ok(())
    }
}

fn load_config(file: &Path) -> Config {
    let base = defaults();
    let Ok(text) = std::fs::read_to_string(file) else {
        return base;
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(saved) => apply_patch(&base, &saved).unwrap_or_else(|e| {
            eprintln!("[core] {} ignored: {e}", file.display());
            base
        }),
        Err(e) => {
            eprintln!("[core] {} ignored: {e}", file.display());
            base
        }
    }
}

fn initial(config: Config) -> State {
    let livi = PerScreen {
        main: Front::Livi,
        cluster: Front::Livi,
        aux: Front::Livi,
    };
    State {
        front: livi,
        sessions: Default::default(),
        now_playing: Default::default(),
        telemetry: Default::default(),
        navigation: Default::default(),
        system: Default::default(),
        devices: Default::default(),
        update: Default::default(),
        cluster: Default::default(),
        dab: Default::default(),
        cameras: Vec::new(),
        mfi: Default::default(),
        config,
    }
}

/// What the actions and the session events act on.
struct Ctx {
    hub: Hub,
    aa: AaHandle,
    active: Mutex<Option<SessionId>>,
}

impl Ctx {
    fn active(&self) -> Option<SessionId> {
        *self.active.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_active(&self, id: Option<SessionId>) {
        *self.active.lock().unwrap_or_else(|e| e.into_inner()) = id;
    }

    fn act(&self, action: Action) -> Result<(), String> {
        match action {
            Action::SetConfig { patch } => {
                let before = self.hub.config().appearance_mode.night_mode();
                self.hub.set_config(&patch)?;
                // A phone picks day or night colours from what the car reports.
                let after = self.hub.config().appearance_mode.night_mode();
                if before != after
                    && let Some(night) = after
                {
                    self.aa.send(AaCmd::Sensor(Sensor::NightMode(night)));
                }
                Ok(())
            }
            Action::Show { screen, front } => {
                self.hub.update(|s| *s.front.get_mut(screen) = front);
                Ok(())
            }
            Action::Media { control } => {
                if let Some(id) = self.active() {
                    let command = match control {
                        MediaControl::PlayPause => Command::PlayPause,
                        MediaControl::Play => Command::Play,
                        MediaControl::Pause => Command::Pause,
                        MediaControl::Next => Command::Next,
                        MediaControl::Prev => Command::Prev,
                    };
                    self.aa.session(id, SessionCmd::Command(command));
                }
                Ok(())
            }
            // The app has no way to quit itself yet, the system's back and home do.
            Action::Quit | Action::Restart => Ok(()),
            Action::ForgetDevice { id } => wireless::forget(&id),
            Action::ConnectDevice { id } | Action::SelectDevice { id } => wireless::wake(&id),
            _ => Err("not available on Android yet".into()),
        }
    }

    fn input(&self, input: Input) {
        let Input::Pointer {
            screen: Screen::Main,
            points,
        } = input
        else {
            return;
        };
        if points.is_empty() || self.hub.front() != Front::Projection {
            return;
        }
        let Some(id) = self.active() else { return };
        let touches = points
            .iter()
            .map(|point| TouchItem {
                id: point.id,
                x: point.x.clamp(0.0, 1.0),
                y: point.y.clamp(0.0, 1.0),
                action: match point.phase {
                    Phase::Down => TouchAction::Down,
                    Phase::Move => TouchAction::Move,
                    Phase::Up | Phase::Cancel => TouchAction::Up,
                },
            })
            .collect();
        self.aa.session(id, SessionCmd::MultiTouch(touches));
    }

    fn on_event(&self, event: AaEvent) {
        match event {
            AaEvent::Connected { id } => {
                println!("[core] Android Auto session {id} is up");
                wireless::session_up();
                self.set_active(Some(id));
                self.aa.session(id, SessionCmd::VideoActive(true));
                self.aa.session(id, SessionCmd::Keyframe);
                if let Some(night) = self.hub.config().appearance_mode.night_mode() {
                    self.aa.send(AaCmd::Sensor(Sensor::NightMode(night)));
                }
                self.hub.update(|s| {
                    s.sessions = Sessions {
                        active: Some(Protocol::Androidauto),
                        position: 1,
                        total: 1,
                    };
                    s.front.main = Front::Projection;
                });
            }
            AaEvent::Disconnected { id } => {
                println!("[core] Android Auto session {id} ended");
                wireless::session_down();
                if self.active() != Some(id) {
                    return;
                }
                self.set_active(None);
                self.hub.update(|s| {
                    s.sessions = Sessions::default();
                    s.front.main = Front::Livi;
                    s.now_playing = NowPlaying::default();
                    s.devices = wireless::devices_view();
                });
            }
            AaEvent::Device {
                id, name, model, ..
            } => {
                // A phone on Wi-Fi is one of the known phones, the list shows it from there.
                if wireless::session_model(&model) {
                    return;
                }
                self.hub.update(|s| {
                    s.devices = vec![DeviceView {
                        id: format!("aa-{id}"),
                        name: Some(name).filter(|n| !n.is_empty()),
                        model: Some(model).filter(|m| !m.is_empty()),
                        protocol: Some(Protocol::Androidauto),
                        last_transport: Some("usb".into()),
                        status: DeviceStatus::Active,
                        battery_level: None,
                        battery_charging: None,
                        signal_strength: None,
                        carrier_name: None,
                        session: Some(1),
                    }];
                });
            }
            AaEvent::VideoCodec { id, cluster, .. } if !cluster => {
                if self.active() == Some(id) {
                    self.hub.update(|s| s.front.main = Front::Projection);
                }
            }
            AaEvent::NowPlaying { id, now_playing } if self.active() == Some(id) => {
                self.hub
                    .update(|s| merge_now_playing(&mut s.now_playing, now_playing));
            }
            _ => {}
        }
    }
}

fn merge_now_playing(into: &mut NowPlaying, from: AaNowPlaying) {
    for (field, value) in [
        (&mut into.title, from.title),
        (&mut into.artist, from.artist),
        (&mut into.album, from.album),
        (&mut into.app, from.app),
    ] {
        if value.is_some() {
            *field = value;
        }
    }
    if let Some(ms) = from.duration_ms {
        into.duration_ms = Some(ms as f64);
    }
    if let Some(ms) = from.elapsed_ms {
        into.elapsed_ms = Some(ms as f64);
    }
    if from.playing.is_some() {
        into.playing = from.playing;
    }
    if let Some(art) = from.artwork.filter(|a| !a.is_empty()) {
        into.artwork = Some(STANDARD.encode(art));
    }
}

async fn send(stream: &mut UnixStream, msg: &FromCore) -> io::Result<()> {
    let frame = encode(msg).map_err(io::Error::other)?;
    stream.write_all(&frame).await
}

async fn client(ctx: Arc<Ctx>, mut stream: UnixStream) -> io::Result<()> {
    let mut decoder = Decoder::new();
    let mut buf = vec![0u8; 8192];
    let hello = loop {
        if let Some(msg) = decoder.next_message::<ToCore>() {
            break msg.map_err(io::Error::other)?;
        }
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        decoder.push(&buf[..n]);
    };
    let ToCore::Hello { protocol, client } = hello else {
        let reason = "the first message has to be hello".into();
        return send(&mut stream, &FromCore::Refused { reason }).await;
    };
    if protocol != PROTOCOL {
        let reason = format!("core speaks protocol {PROTOCOL}, not {protocol}");
        return send(&mut stream, &FromCore::Refused { reason }).await;
    }
    println!("[core] {client} connected");
    let (msg, mut patches) = ctx.hub.welcome();
    send(&mut stream, &msg).await?;
    loop {
        tokio::select! {
            read = stream.read(&mut buf) => {
                let n = read?;
                if n == 0 {
                    break;
                }
                decoder.push(&buf[..n]);
                while let Some(msg) = decoder.next_message::<ToCore>() {
                    match msg {
                        Ok(ToCore::Resync) => {
                            let (msg, fresh) = ctx.hub.welcome();
                            patches = fresh;
                            send(&mut stream, &msg).await?;
                        }
                        Ok(ToCore::Action { id, action }) => {
                            let error = ctx.act(action).err();
                            send(&mut stream, &FromCore::Reply { id, error }).await?;
                        }
                        Ok(ToCore::Input { input }) => ctx.input(input),
                        // Nothing shows spectrum or link speed here, and the page needs no one.
                        Ok(_) => {}
                        Err(FrameError::Json(e)) => println!("[core] {client} sent a broken frame: {e}"),
                        Err(e @ FrameError::TooLarge(_)) => return Err(io::Error::other(e)),
                    }
                }
            }
            patch = patches.recv() => match patch {
                Ok(frame) => stream.write_all(&frame).await?,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let (msg, fresh) = ctx.hub.welcome();
                    patches = fresh;
                    send(&mut stream, &msg).await?;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    println!("[core] {client} disconnected");
    Ok(())
}

/// Where the app keeps its files and how to reach the Java side, see `usb::watch`.
pub struct Platform {
    pub dir: PathBuf,
    pub vm: usize,
    pub activity: usize,
}

/// Starts the core on its own thread and returns the stream the UI connects with.
pub fn start(platform: Platform) -> io::Result<StdStream> {
    let Platform { dir, vm, activity } = platform;
    let config_file = dir.join("config.json");
    let hub = Hub::new(initial(load_config(&config_file)), config_file);
    let (ui, core) = StdStream::pair()?;
    core.set_nonblocking(true)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .enable_all()
        .build()?;
    std::thread::Builder::new()
        .name("livi-core".into())
        .spawn(move || {
            runtime.block_on(async move {
                let media = AndroidMedia::new(feed_path_in(&dir));
                let feed = media.clone();
                tokio::spawn(async move {
                    if let Err(e) = feed.serve_feed().await {
                        eprintln!("[media] feed socket: {e}");
                    }
                });
                let helper = Helper::new(dir.join("aa-bt.sock"));
                let line = helper.clone();
                tokio::spawn(async move {
                    if let Err(e) = line.serve().await {
                        eprintln!("[helper] line socket: {e}");
                    }
                });
                let (events, mut session_events) = mpsc::unbounded_channel();
                let media_handle = media.clone();
                let aa = manager::start(
                    AaHelperSock::new(helper.sock_path()),
                    media,
                    hub.aa_config.subscribe(),
                    events,
                );
                media_handle.set_handle(aa.clone());
                let java = Java::new(vm, activity);
                media_handle.set_java(java);
                // Ask once now, so the dialog is not in the way when the assistant is called.
                std::thread::spawn(move || {
                    if !java.has_microphone_permission() {
                        java.ask_for_microphone();
                    }
                });
                let wireless_plan = hub.wireless.subscribe();
                let (device_tx, mut device_rx) = mpsc::unbounded_channel();
                let registry = devices::Registry::load(dir.join("devices.json"), move |views| {
                    let _ = device_tx.send(views);
                });
                let ctx = Arc::new(Ctx {
                    hub,
                    aa,
                    active: Mutex::new(None),
                });
                // Whether H.265 is offered depends on the decoder, and creating one can block on a stuck
                // media service, so it is not done on the way up.
                let probe = ctx.clone();
                std::thread::spawn(move || {
                    if video::hardware_decoder(vm, video::Codec::H265) {
                        probe.hub.offer_hevc();
                    }
                });
                // The known phones go into the state the Devices page reads, now and on every change.
                let for_devices = ctx.clone();
                tokio::spawn(async move {
                    while let Some(views) = device_rx.recv().await {
                        for_devices.hub.update(|s| s.devices = views);
                    }
                });
                registry.publish();
                let for_events = ctx.clone();
                tokio::spawn(async move {
                    while let Some(event) = session_events.recv().await {
                        for_events.on_event(event);
                    }
                });
                wireless::watch(
                    java,
                    dir.clone(),
                    helper.events(),
                    wireless_plan,
                    registry.clone(),
                    &tokio::runtime::Handle::current(),
                );
                crate::session::usb::watch(vm, activity, helper, tokio::runtime::Handle::current());
                match UnixStream::from_std(core) {
                    Ok(stream) => {
                        if let Err(e) = client(ctx, stream).await {
                            println!("[core] client gone: {e}");
                        }
                    }
                    Err(e) => eprintln!("[core] {e}"),
                }
            });
        })?;
    Ok(ui)
}
