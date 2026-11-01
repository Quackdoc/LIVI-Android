// SPDX-FileCopyrightText: 2025-2026 Mitchel Stewart
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! The wireless handshake: Bluetooth to wake a phone, Wi-Fi Direct to give it a link, and the registry of
//! phones this head unit has met.

#[allow(dead_code)]
pub mod bt;
pub mod devices;
pub mod p2p;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use livi_aa::bootstrap::{self, Setup};
use livi_aa::wpp::Identity;
use livi_core_proto::config::{Config, WifiBand};
use livi_hfp::Slc;
use livi_wifi::{Channel, Security};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};

use self::bt::{Bt, Conn, Listener};
use self::devices::Registry;
use self::p2p::{Band, GroupConfig, GroupInfo, P2p};
use crate::android::java::Java;
use crate::session::helper::Broadcaster;

const WAKE_INTERVAL: Duration = Duration::from_secs(15);
const WAKE_LINK_HOLD: Duration = Duration::from_secs(20);
const QUIET_AFTER_PHONE: Duration = Duration::from_secs(120);
const WAKE_MAX_FAILURES: u32 = 3;
const WAKE_BACKOFF: Duration = Duration::from_secs(300);
const PERMISSION_WAIT: Duration = Duration::from_secs(120);

/// Android Auto sessions that are up. While one is, the phone is left alone: a wake-up link would
/// take its single hands-free slot in the middle of a drive.
static LIVE_SESSIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct WirelessState {
    registry: Arc<Registry>,
    wake: mpsc::UnboundedSender<String>,
    bluetooth: std::sync::Mutex<Option<Bt>>,
    last_phone: std::sync::Mutex<Option<(String, String, Instant)>>,
    pending_model: std::sync::Mutex<Option<String>>,
}

static WIRELESS_STATE: std::sync::OnceLock<WirelessState> = std::sync::OnceLock::new();

const SESSION_FOLLOWS_SETUP: Duration = Duration::from_secs(180);

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn session_up() {
    LIVE_SESSIONS.fetch_add(1, Ordering::Relaxed);
    let Some(shared) = WIRELESS_STATE.get() else {
        return;
    };
    let phone = lock(&shared.last_phone).clone();
    if let Some((mac, name, at)) = phone
        && at.elapsed() < SESSION_FOLLOWS_SETUP
    {
        shared.registry.note_connected(&mac, &name);
        shared.registry.set_live(Some(&mac));
        if let Some(model) = lock(&shared.pending_model).take() {
            shared.registry.note_model(&mac, &model);
        }
    }
}

pub fn session_down() {
    let _ = LIVE_SESSIONS.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
    if LIVE_SESSIONS.load(Ordering::Relaxed) == 0
        && let Some(shared) = WIRELESS_STATE.get()
    {
        shared.registry.set_live(None);
    }
}

pub fn session_model(model: &str) -> bool {
    let Some(shared) = WIRELESS_STATE.get() else {
        return false;
    };
    match shared.registry.live() {
        Some(mac) => {
            shared.registry.note_model(&mac, model);
            true
        }
        None => {
            let wireless = lock(&shared.last_phone)
                .as_ref()
                .is_some_and(|(_, _, at)| at.elapsed() < SESSION_FOLLOWS_SETUP);
            if wireless {
                *lock(&shared.pending_model) = Some(model.to_owned());
            }
            wireless
        }
    }
}

pub fn devices_view() -> Vec<livi_core_proto::state::DeviceView> {
    WIRELESS_STATE
        .get()
        .map(|s| s.registry.views())
        .unwrap_or_default()
}

pub fn wake(id: &str) -> Result<(), String> {
    let shared = WIRELESS_STATE.get().ok_or("wireless Android Auto is off")?;
    shared
        .wake
        .send(devices::norm_mac(id))
        .map_err(|_| "the wireless mode is not running".to_owned())
}

pub fn forget(id: &str) -> Result<(), String> {
    let shared = WIRELESS_STATE.get().ok_or("wireless Android Auto is off")?;
    let mac = devices::norm_mac(id);
    if !shared.registry.forget(&mac) {
        return Err("that phone is not in the list".into());
    }
    let bluetooth = lock(&shared.bluetooth).clone();
    if let Some(bluetooth) = bluetooth {
        match bluetooth.remove_bond(&mac) {
            Ok(true) => println!("[wireless] unpaired {mac}"),
            Ok(false) => eprintln!(
                "[wireless] {mac} was not unpaired: it may be unpaired already, or Android would not do it. \
				If it is still listed, remove it in the Bluetooth settings"
            ),
            Err(e) => eprintln!(
                "[wireless] {mac} stays paired ({e}), remove it in the Bluetooth settings"
            ),
        }
    }
    Ok(())
}

type WakeRx = Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<String>>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub enabled: bool,
    pub auto_connect: bool,
    pub ssid: String,
    pub passphrase: String,
    pub band: Band,
    pub channel: u32,
    pub port: u16,
}

impl Plan {
    pub fn of(config: &Config) -> Self {
        // Wi-Fi Direct names are `DIRECT-xy-...` with x and y alphanumeric.
        let name: String = config
            .car_name
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(16)
            .collect();
        let band = match config.wifi_type {
            WifiBand::Ghz24 => Band::Ghz2,
            WifiBand::Ghz5 => Band::Ghz5,
            // Wi-Fi Direct has no 6 GHz group, the system picks.
            WifiBand::Ghz6 => Band::Auto,
        };
        Self {
            enabled: config.wireless_aa_enabled,
            auto_connect: config.auto_conn,
            ssid: format!("DIRECT-LV-{name}"),
            passphrase: config.wifi_password.clone(),
            band,
            channel: config.wifi_channel,
            port: livi_aa::consts::TCP_PORT,
        }
    }
}

pub fn watch(
    java: Java,
    dir: std::path::PathBuf,
    events: Broadcaster,
    mut plan: watch::Receiver<Plan>,
    registry: Arc<Registry>,
    runtime: &tokio::runtime::Handle,
) {
    let (wake_tx, wake_rx) = mpsc::unbounded_channel();
    let _ = WIRELESS_STATE.set(WirelessState {
        registry,
        wake: wake_tx,
        bluetooth: std::sync::Mutex::new(None),
        last_phone: std::sync::Mutex::new(None),
        pending_model: std::sync::Mutex::new(None),
    });
    let wake_rx = Arc::new(tokio::sync::Mutex::new(wake_rx));
    runtime.spawn(async move {
        let mut running: Option<(watch::Sender<bool>, tokio::task::JoinHandle<()>)> = None;
        loop {
            let wanted = plan.borrow_and_update().clone();
            if let Some((cancel, task)) = running.take() {
                let _ = cancel.send(true);
                let _ = task.await;
            }
            if wanted.enabled {
                let (cancel, cancelled) = watch::channel(false);
                let (events, dir, wake_rx) = (events.clone(), dir.clone(), wake_rx.clone());
                let task = tokio::spawn(async move {
                    if let Err(e) = session(java, dir, events, wanted, wake_rx, cancelled).await {
                        eprintln!("[wireless] {e}");
                    }
                });
                running = Some((cancel, task));
            }
            if plan.changed().await.is_err() {
                break;
            }
        }
    });
}

async fn rest(cancel: &mut watch::Receiver<bool>, time: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(time) => *cancel.borrow(),
        _ = cancel.changed() => true,
    }
}

async fn granted(
    cancel: &mut watch::Receiver<bool>,
    what: &str,
    has: impl Fn() -> bool,
    ask: impl Fn(),
) -> Result<(), String> {
    if has() {
        return Ok(());
    }
    eprintln!("[wireless] asking for {what}");
    ask();
    let start = Instant::now();
    while !has() {
        if start.elapsed() > PERMISSION_WAIT {
            return Err(format!("{what} was not granted"));
        }
        if rest(cancel, Duration::from_secs(1)).await {
            return Err("stopped".into());
        }
    }
    Ok(())
}

async fn session(
    java: Java,
    dir: std::path::PathBuf,
    events: Broadcaster,
    plan: Plan,
    wake_rx: WakeRx,
    mut cancel: watch::Receiver<bool>,
) -> Result<(), String> {
    if p2p::wifi_display_cert_mode(java) {
        return Err(
            "Wireless display certification is on in the developer options. It makes \
			Android restart its system process when a Wi-Fi Direct group forms, so wireless \
			Android Auto stays off until it is switched off."
                .into(),
        );
    }
    let mut said_wifi = false;
    while !p2p::wifi_enabled(java) {
        if !said_wifi {
            eprintln!(
                "[wireless] Wi-Fi is switched off; Wi-Fi Direct does not run without it. Waiting for it."
            );
            said_wifi = true;
        }
        if rest(&mut cancel, Duration::from_secs(3)).await {
            return Err("stopped".into());
        }
    }
    // Android needs location on to let an app create a Wi-Fi Direct group, and answers a group
    // request with silence while it is off. Waits for it instead of giving up.
    let mut said = false;
    while !p2p::location_enabled(java) {
        if !said {
            eprintln!(
                "[wireless] location is switched off; Android needs it on for a Wi-Fi Direct \
				group. Waiting for it."
            );
            said = true;
        }
        if rest(&mut cancel, Duration::from_secs(3)).await {
            return Err("stopped".into());
        }
    }
    granted(
        &mut cancel,
        "the Wi-Fi Direct permission",
        || java.has_p2p_permission(),
        || java.ask_for_p2p_permission(),
    )
    .await?;
    granted(
        &mut cancel,
        "the Bluetooth permission",
        || java.has_bt_permission(),
        || java.ask_for_bt_permission(),
    )
    .await?;
    let bluetooth = Bt::new(java)?;
    if !bluetooth.enabled() {
        return Err("Bluetooth is off".into());
    }
    if let Some(shared) = WIRELESS_STATE.get() {
        *lock(&shared.bluetooth) = Some(bluetooth.clone());
    }

    let group_config = GroupConfig {
        ssid: plan.ssid.clone(),
        passphrase: plan.passphrase.clone(),
        band: plan.band,
        frequency: (plan.band != Band::Auto).then(|| Channel::of_number(plan.channel).freq_mhz()),
    };
    let started = {
        let group_config = group_config.clone();
        let state = dir.join("p2p-group");
        tokio::task::spawn_blocking(move || P2p::start(java, &group_config, &state))
            .await
            .map_err(|e| e.to_string())?
    };
    let group = Arc::new(started?);
    let result = serve(
        Parts {
            java,
            events: &events,
            plan: &plan,
            group_config: &group_config,
            group: &group,
            bluetooth: &bluetooth,
            wake_rx,
        },
        &mut cancel,
    )
    .await;
    group.stop();
    result
}

struct Parts<'a> {
    java: Java,
    events: &'a Broadcaster,
    plan: &'a Plan,
    group_config: &'a GroupConfig,
    group: &'a Arc<P2p>,
    bluetooth: &'a Bt,
    wake_rx: WakeRx,
}

async fn serve(parts: Parts<'_>, cancel: &mut watch::Receiver<bool>) -> Result<(), String> {
    let Parts {
        java,
        events,
        plan,
        group_config,
        group,
        bluetooth,
        wake_rx,
    } = parts;
    let info = group
        .ensure(group_config, 3, Duration::from_secs(20))
        .await?;
    println!(
        "[wireless] group {} up on {} ({}), bssid {}",
        plan.ssid,
        info.iface,
        info.ip,
        info.bssid.map_or_else(|| "unknown".into(), p2p::format_mac)
    );

    let announce = events.clone();
    let tcp = tokio::spawn(livi_aa::server::run(plan.port, move |socket, peer| {
        announce.push_json(format!(
			"{{\"event\":\"aa-session\",\"socket\":\"{socket}\",\"peer\":\"{peer}\",\"transport\":\"wifi\"}}"
		));
    }));

    let closing = Arc::new(AtomicBool::new(false));
    let quiet_until = Arc::new(AtomicU64::new(0));
    let (aa_tx, mut aa_rx) = mpsc::unbounded_channel();
    let (hfp_tx, mut hfp_rx) = mpsc::unbounded_channel();
    let mut listeners = Vec::new();
    let mut failures = Vec::new();
    for (name, uuid, tx) in [
        ("AndroidAuto", bt::AA_UUID, aa_tx),
        ("Hands-Free Unit", bt::HFP_HF_UUID, hfp_tx),
    ] {
        match bluetooth.listen(name, uuid, true) {
            Ok(listener) => {
                println!("[wireless] listening on {name} ({uuid})");
                accept_in_thread(listener.clone(), tx, closing.clone(), name);
                listeners.push(listener);
            }
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }
    if listeners.is_empty()
        || failures
            .iter()
            .any(|failure| failure.starts_with("AndroidAuto"))
    {
        closing.store(true, Ordering::Relaxed);
        for listener in &listeners {
            listener.close();
        }
        tcp.abort();
        return Err(format!("no Bluetooth record: {}", failures.join("; ")));
    }
    for failure in &failures {
        eprintln!("[wireless] {failure}");
    }

    let plan_for_phones = plan.clone();
    let info_for_phones = info.clone();
    let events_for_phones = events.clone();
    let quiet = quiet_until.clone();
    let phones = tokio::spawn(async move {
        while let Some(conn) = aa_rx.recv().await {
            if let Some(shared) = WIRELESS_STATE.get() {
                *lock(&shared.last_phone) =
                    Some((conn.mac.clone(), conn.name.clone(), Instant::now()));
            }
            quiet.store(
                now_ms() + QUIET_AFTER_PHONE.as_millis() as u64,
                Ordering::Relaxed,
            );
            let setup = setup_of(&plan_for_phones, &info_for_phones);
            tokio::spawn(bootstrap_phone(conn, setup, events_for_phones.clone()));
        }
    });
    let handsfree = tokio::spawn(async move {
        while let Some(conn) = hfp_rx.recv().await {
            tokio::spawn(hands_free(conn, None));
        }
    });
    let wake_task = tokio::spawn(wake_loop(
        bluetooth.clone(),
        plan.auto_connect,
        wake_rx,
        quiet_until.clone(),
        cancel.clone(),
    ));
    let pairing = tokio::spawn(pairing_loop(bluetooth.clone(), java, cancel.clone()));

    let _ = java;
    while !*cancel.borrow() {
        if cancel.changed().await.is_err() {
            break;
        }
    }
    println!("[wireless] stopping");
    closing.store(true, Ordering::Relaxed);
    for listener in &listeners {
        listener.close();
    }
    wake_task.abort();
    pairing.abort();
    phones.abort();
    handsfree.abort();
    tcp.abort();
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn setup_of(plan: &Plan, info: &GroupInfo) -> Setup {
    Setup {
        ssid: plan.ssid.clone(),
        passphrase: plan.passphrase.clone(),
        channel: Channel::of_number(plan.channel),
        ap_ip: info.ip.to_string(),
        port: plan.port,
        security: Security::Wpa2,
        bssid: info.bssid.map(p2p::format_mac).unwrap_or_default(),
    }
}

/// One thread per record: `accept` blocks, and `close` from another thread is what ends it.
fn accept_in_thread(
    listener: Listener,
    tx: mpsc::UnboundedSender<Conn>,
    closing: Arc<AtomicBool>,
    name: &'static str,
) {
    let spawned = std::thread::Builder::new()
        .name(format!("bt-accept-{name}"))
        .spawn(move || {
            while !closing.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok(conn) => {
                        if tx.send(conn).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        if closing.load(Ordering::Relaxed) {
                            break;
                        }
                        eprintln!("[wireless] {name} accept: {e}");
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            }
        });
    if let Err(e) = spawned {
        eprintln!("[wireless] no accept thread for {name}: {e}");
    }
}

/// Turns a link from `bt` into a tokio stream.
fn async_stream(conn: Conn) -> std::io::Result<(String, UnixStream)> {
    let mac = conn.mac.clone();
    let socket = conn.into_stream()?;
    socket.set_nonblocking(true)?;
    Ok((mac, UnixStream::from_std(socket)?))
}

/// The phone has dialled the Android Auto record: tells it where the network is.
async fn bootstrap_phone(conn: Conn, setup: Setup, events: Broadcaster) {
    let (mac, stream) = match async_stream(conn) {
        Ok(x) => x,
        Err(e) => return eprintln!("[wireless] bridge: {e}"),
    };
    let announce = |mac: &str, id: &Identity| {
        if id.instance_id.is_empty() && id.serial.is_empty() {
            return;
        }
        events.push_json(format!(
			"{{\"event\":\"aa-device\",\"btMac\":\"{}\",\"instanceId\":\"{}\",\"usbSerial\":\"{}\"}}",
			mac.to_uppercase(),
			id.instance_id,
			id.serial
		));
    };
    if let Err(e) = bootstrap::run(stream, &mac, &setup, announce, |_| false).await {
        eprintln!("[wireless] {mac}: setup ended: {e}");
    }
}

async fn hands_free(conn: Conn, hold: Option<Duration>) {
    let (mac, mut stream) = match async_stream(conn) {
        Ok(x) => x,
        Err(e) => return eprintln!("[wireless] hands-free bridge: {e}"),
    };
    let exchange = async {
        let mut slc = Slc::default();
        stream.write_all(Slc::hello().as_bytes()).await?;
        let mut buf = [0u8; 512];
        let mut pending = String::new();
        loop {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                return std::io::Result::Ok(());
            }
            pending.push_str(&String::from_utf8_lossy(&buf[..n]));
            while let Some(end) = pending.find(['\r', '\n']) {
                let line: String = pending.drain(..=end).collect();
                for reply in slc.on_line(&line) {
                    stream.write_all(reply.as_bytes()).await?;
                }
            }
        }
    };
    let ended = match hold {
        Some(hold) => tokio::time::timeout(hold, exchange).await.unwrap_or(Ok(())),
        None => exchange.await,
    };
    match ended {
        Ok(()) => println!("[wireless] hands-free link to {mac} ended"),
        Err(e) => println!("[wireless] hands-free link to {mac}: {e}"),
    }
}

async fn wake_loop(
    bluetooth: Bt,
    auto_connect: bool,
    wake_rx: WakeRx,
    quiet_until: Arc<AtomicU64>,
    mut cancel: watch::Receiver<bool>,
) {
    let Some(shared) = WIRELESS_STATE.get() else {
        return;
    };
    let mut trouble: HashMap<String, (u32, Instant)> = HashMap::new();
    let mut told_empty = false;
    loop {
        let mut wake_rx = wake_rx.lock().await;
        let requested = tokio::select! {
            _ = tokio::time::sleep(WAKE_INTERVAL) => None,
            asked = wake_rx.recv() => match asked {
                Some(mac) => Some(mac),
                None => return,
            },
            _ = cancel.changed() => return,
        };
        drop(wake_rx);
        if *cancel.borrow() {
            return;
        }
        if LIVE_SESSIONS.load(Ordering::Relaxed) > 0 {
            continue;
        }
        if requested.is_none() && (!auto_connect || now_ms() < quiet_until.load(Ordering::Relaxed))
        {
            continue;
        }
        let bonded = {
            let bluetooth = bluetooth.clone();
            tokio::task::spawn_blocking(move || bluetooth.bonded()).await
        };
        let devices = match bonded {
            Ok(Ok(devices)) => devices,
            Ok(Err(e)) => {
                eprintln!("[wireless] paired devices: {e}");
                continue;
            }
            Err(_) => continue,
        };
        if devices.is_empty() && !told_empty {
            eprintln!("[wireless] no phone is paired with this device");
            told_empty = true;
        }
        let names: HashMap<String, String> = devices
            .iter()
            .map(|device| (devices::norm_mac(&device.mac), device.name.clone()))
            .collect();
        let pairs: Vec<(String, Option<i32>)> = devices
            .iter()
            .map(|d| (d.mac.clone(), d.major_class))
            .collect();
        let live: HashSet<String> = shared.registry.live().into_iter().collect();
        let targets = match &requested {
            Some(mac) if names.contains_key(mac) => vec![mac.clone()],
            Some(mac) => {
                eprintln!("[wireless] {mac} is not paired, it cannot be woken");
                continue;
            }
            None => devices::wake_order(&shared.registry.recents(), &pairs, &live),
        };
        for mac in targets {
            let name = names.get(&mac).cloned().unwrap_or_default();
            if requested.is_none()
                && let Some((fails, until)) = trouble.get(&mac)
                && *fails >= WAKE_MAX_FAILURES
                && Instant::now() < *until
            {
                continue;
            }
            let dial = {
                let (bluetooth, mac) = (bluetooth.clone(), mac.clone());
                tokio::task::spawn_blocking(move || {
                    match bluetooth.connect(&mac, bt::HFP_AG_UUID) {
                        Ok(conn) => Ok((conn, true)),
                        Err(hfp) => bluetooth
                            .connect(&mac, bt::HSP_AG_UUID)
                            .map(|conn| (conn, false))
                            .map_err(|hsp| format!("hands-free: {hfp}; headset: {hsp}")),
                    }
                })
                .await
            };
            match dial {
                Ok(Ok((conn, handsfree))) => {
                    trouble.remove(&mac);
                    shared.registry.set_reachable(&mac, true);
                    println!("[wireless] woke {name} ({mac})");
                    if handsfree {
                        hands_free(conn, Some(WAKE_LINK_HOLD)).await;
                    } else {
                        // A headset link carries no AT walk, the connection itself is the wake.
                        tokio::time::sleep(WAKE_LINK_HOLD).await;
                        conn.close();
                    }
                    // One phone answered: it is the one to connect, the others can wait.
                    break;
                }
                failed => {
                    let reason = match failed {
                        Ok(Err(e)) => e,
                        Err(e) => e.to_string(),
                        Ok(Ok(_)) => String::new(),
                    };
                    shared.registry.set_reachable(&mac, false);
                    let seen = trouble.get(&mac).map_or(0, |t| t.0);
                    if seen == 0 {
                        eprintln!("[wireless] cannot reach {name} ({mac}) for a wake-up: {reason}");
                    }
                    let entry = trouble.entry(mac.clone()).or_insert((0, Instant::now()));
                    entry.0 += 1;
                    entry.1 = Instant::now() + WAKE_BACKOFF;
                }
            }
            if *cancel.borrow() {
                return;
            }
        }
    }
}

/// How often the pairing mode looks, how many times it asks one phone to pair again, and how long
/// it leaves the system's "make visible" prompt alone after showing it.
const PAIRING_CHECK_INTERVAL: Duration = Duration::from_secs(20);
const PAIRING_MAX_TRIES: u32 = 3;
const VISIBILITY_PROMPT_COOLDOWN: Duration = Duration::from_secs(600);
const VISIBILITY_SECONDS: u32 = 120;

/// Keeps a phone able to pair. While no phone is paired this makes the device visible, which Android
/// only allows with a prompt the user accepts, and asks the phone that connected last to pair
/// again, which both phones confirm. Nothing happens while a phone is paired or a session runs.
async fn pairing_loop(bluetooth: Bt, java: Java, mut cancel: watch::Receiver<bool>) {
    let Some(shared) = WIRELESS_STATE.get() else {
        return;
    };
    let mut tries: HashMap<String, u32> = HashMap::new();
    let mut visible_at: Option<Instant> = None;
    loop {
        if rest(&mut cancel, PAIRING_CHECK_INTERVAL).await {
            return;
        }
        if LIVE_SESSIONS.load(Ordering::Relaxed) > 0 {
            continue;
        }
        let paired = {
            let bluetooth = bluetooth.clone();
            tokio::task::spawn_blocking(move || bluetooth.bonded()).await
        };
        let Ok(Ok(paired)) = paired else { continue };
        let phone_paired = paired.iter().any(|d| {
            d.major_class == Some(devices::MAJOR_PHONE)
                || shared
                    .registry
                    .recents()
                    .iter()
                    .any(|(m, _)| *m == devices::norm_mac(&d.mac))
        });
        if phone_paired {
            tries.clear();
            continue;
        }
        let macs: Vec<String> = paired.iter().map(|d| d.mac.clone()).collect();
        if let Some(mac) =
            devices::pairing_target(&shared.registry.recents(), &macs, &tries, PAIRING_MAX_TRIES)
        {
            *tries.entry(mac.clone()).or_insert(0) += 1;
            let asked = {
                let (bluetooth, mac) = (bluetooth.clone(), mac.clone());
                tokio::task::spawn_blocking(move || bluetooth.create_bond(&mac)).await
            };
            println!("[wireless] no phone is paired, asking {mac} to pair again: {asked:?}");
        }
        if visible_at.is_none_or(|shown_at| shown_at.elapsed() >= VISIBILITY_PROMPT_COOLDOWN) {
            println!(
                "[wireless] no phone is paired, making this device visible for {VISIBILITY_SECONDS} s"
            );
            java.make_discoverable(VISIBILITY_SECONDS);
            visible_at = Some(Instant::now());
        }
    }
}
