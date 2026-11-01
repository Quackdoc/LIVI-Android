// SPDX-FileCopyrightText: 2025-2026 Mitchel Stewart
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! Wi-Fi Direct P2P module.

pub mod pure {
    use std::net::Ipv6Addr;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Band {
        Auto,
        Ghz2,
        Ghz5,
    }

    impl Band {
        pub fn to_int(self) -> i32 {
            match self {
                Band::Auto => 0,
                Band::Ghz2 => 1,
                Band::Ghz5 => 2,
            }
        }
    }

    #[derive(Clone)]
    pub struct GroupConfig {
        pub ssid: String,
        pub passphrase: String,
        pub band: Band,
        pub frequency: Option<u32>,
    }

    impl GroupConfig {
        /// What a group was asked to be, to tell later whether a group that is still there is the
        /// same one. The name, key and where it sits, nothing else.
        pub fn fingerprint(&self) -> String {
            format!(
                "{}\n{}\n{:?}\n{:?}",
                self.ssid, self.passphrase, self.band, self.frequency
            )
        }

        pub fn validate(&self) -> Result<(), String> {
            let named = self.ssid.strip_prefix("DIRECT-").is_some_and(|rest| {
                rest.len() >= 2 && rest.bytes().take(2).all(|b| b.is_ascii_alphanumeric())
            });
            if !named || self.ssid.len() > 32 {
                return Err(
                    "Invalid SSID: must be DIRECT-xy (x, y alphanumeric) and <= 32 bytes"
                        .to_string(),
                );
            }
            if self.passphrase.len() < 8 || self.passphrase.len() > 63 {
                return Err("Invalid passphrase: 8..=63 characters".to_string());
            }
            if self.frequency.is_some_and(|f| !(2412..=7125).contains(&f)) {
                return Err("Invalid frequency: not a Wi-Fi channel".to_string());
            }
            Ok(())
        }
    }

    #[derive(Clone, Debug)]
    pub struct GroupInfo {
        pub iface: String,
        pub ip: std::net::Ipv4Addr,
        pub bssid: Option<[u8; 6]>,
    }

    pub fn eui64_to_mac(addr: Ipv6Addr) -> Option<[u8; 6]> {
        let octets = addr.octets();
        // Link-local prefix fe80::/64
        if octets[0] == 0xfe && (octets[1] & 0xc0) == 0x80 {
            // Interface ID bytes 3 and 4 are 0xff, 0xfe (octets 11 and 12)
            if octets[11] == 0xff && octets[12] == 0xfe {
                let mut mac = [0u8; 6];
                mac[0] = octets[8] ^ 0x02;
                mac[1] = octets[9];
                mac[2] = octets[10];
                mac[3] = octets[13];
                mac[4] = octets[14];
                mac[5] = octets[15];
                return Some(mac);
            }
        }
        None
    }

    pub fn format_mac(mac: [u8; 6]) -> String {
        format!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
        )
    }
}

pub use pure::*;

use std::time::Duration;

use jni::Env;
use jni::errors::Error as JError;
use jni::objects::{JObject, JValue};

use crate::android::env::{EnvExt, Ref, with_env};
use crate::android::java::Java;

const WIFI_DIRECT_REQUEST_CODE: i32 = 2;
const NEARBY_WIFI_DEVICES: &str = "android.permission.NEARBY_WIFI_DEVICES";
const ACCESS_FINE_LOCATION: &str = "android.permission.ACCESS_FINE_LOCATION";
const PERMISSION_GRANTED: i32 = 0;
/// Android 13, the first release with `NEARBY_WIFI_DEVICES`.
const ANDROID_13: i32 = 33;

const CHANNEL_TYPE: &str = "Landroid/net/wifi/p2p/WifiP2pManager$Channel;";
const ACTION_LISTENER_TYPE: &str = "Landroid/net/wifi/p2p/WifiP2pManager$ActionListener;";
const CONFIG_TYPE: &str = "Landroid/net/wifi/p2p/WifiP2pConfig;";
const BUILDER_TYPE: &str = "Landroid/net/wifi/p2p/WifiP2pConfig$Builder;";

pub struct P2p {
    vm: usize,
    manager: Ref,
    channel: Ref,
    state: std::path::PathBuf,
}

fn create_group(
    env: &mut Env,
    manager: &JObject,
    channel: &JObject,
    cfg: &GroupConfig,
) -> Result<(), JError> {
    let builder = env.construct("android/net/wifi/p2p/WifiP2pConfig$Builder", "()V", &[])?;
    let ssid = env.new_string(&cfg.ssid)?;
    let passphrase = env.new_string(&cfg.passphrase)?;
    let string_in = format!("(Ljava/lang/String;){BUILDER_TYPE}");
    env.call(
        &builder,
        "setNetworkName",
        &string_in,
        &[JValue::Object(&ssid)],
    )?;
    env.call(
        &builder,
        "setPassphrase",
        &string_in,
        &[JValue::Object(&passphrase)],
    )?;
    match cfg.frequency {
        Some(mhz) => env.call(
            &builder,
            "setGroupOperatingFrequency",
            format!("(I){BUILDER_TYPE}"),
            &[JValue::Int(mhz as i32)],
        )?,
        None => env.call(
            &builder,
            "setGroupOperatingBand",
            format!("(I){BUILDER_TYPE}"),
            &[JValue::Int(cfg.band.to_int())],
        )?,
    };
    env.call(
        &builder,
        "enablePersistentMode",
        format!("(Z){BUILDER_TYPE}"),
        &[JValue::Bool(false)],
    )?;
    let config = env
        .call(&builder, "build", format!("(){CONFIG_TYPE}"), &[])?
        .l()?;
    // No listener: the framework allows null and the outcome is read from the interface instead.
    env.call(
        manager,
        "createGroup",
        format!("({CHANNEL_TYPE}{CONFIG_TYPE}{ACTION_LISTENER_TYPE})V"),
        &[
            JValue::Object(channel),
            JValue::Object(&config),
            JValue::Object(&JObject::null()),
        ],
    )?;
    Ok(())
}

impl P2p {
    pub fn start(java: Java, cfg: &GroupConfig, state: &std::path::Path) -> Result<Self, String> {
        cfg.validate()?;
        let (vm, activity) = java.raw();
        let made = with_env(vm, |env| {
            // SAFETY: the activity is a global reference of the framework, valid for the process.
            let activity = unsafe { JObject::from_raw(env, activity as jni::sys::jobject) };
            let service = env.new_string("wifip2p")?;
            let manager = env
                .call(
                    &activity,
                    "getSystemService",
                    "(Ljava/lang/String;)Ljava/lang/Object;",
                    &[JValue::Object(&service)],
                )?
                .l()?;
            if manager.is_null() {
                return Ok(None);
            }
            let looper = env
                .call_static(
                    "android/os/Looper",
                    "getMainLooper",
                    "()Landroid/os/Looper;",
                    &[],
                )?
                .l()?;
            let channel = env
				.call(
					&manager,
					"initialize",
					format!(
						"(Landroid/content/Context;Landroid/os/Looper;Landroid/net/wifi/p2p/WifiP2pManager$ChannelListener;){CHANNEL_TYPE}"
					),
					&[
						JValue::Object(&activity),
						JValue::Object(&looper),
						JValue::Object(&JObject::null()),
					],
				)?
				.l()?;
            if channel.is_null() {
                return Ok(None);
            }
            Ok(Some((env.global(&manager)?, env.global(&channel)?)))
        })?;
        let (manager, channel) =
            made.ok_or("this device has no Wi-Fi Direct, or the service refused the channel")?;
        let p2p = Self {
            vm,
            manager,
            channel,
            state: state.to_path_buf(),
        };
        // A group outlives the app that made it: the framework keeps it when the app is killed. One
        // that this app made with these very settings is used as it is. Rebuilding it would drop the
        // phone, which stays associated with the old one, reuses that association and then loses it
        // in the middle of the setup, and its recovery from that takes long and often fails.
        if find_group().is_some() {
            if p2p.remembered().as_deref() == Some(cfg.fingerprint().as_str()) {
                eprintln!("[p2p] reusing the group from the last run");
                return Ok(p2p);
            }
            // Not ours, or made with other settings: it would keep its old name and key.
            eprintln!("[p2p] removing a group left over from an earlier run");
            p2p.remove_group();
            for _ in 0..25 {
                if find_group().is_none() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        p2p.ask_for(cfg)?;
        Ok(p2p)
    }

    fn remembered(&self) -> Option<String> {
        std::fs::read_to_string(&self.state).ok()
    }

    /// Asks for the group
    fn ask_for(&self, cfg: &GroupConfig) -> Result<(), String> {
        if let Err(e) = std::fs::write(&self.state, cfg.fingerprint()) {
            eprintln!("[p2p] cannot remember the group: {e}");
        }
        with_env(self.vm, |env| {
            create_group(env, &self.manager, &self.channel, cfg)
        })
    }

    fn remove_group(&self) {
        let out = with_env(self.vm, |env| {
            env.call(
                &self.manager,
                "removeGroup",
                format!("({CHANNEL_TYPE}{ACTION_LISTENER_TYPE})V"),
                &[
                    JValue::Object(&self.channel),
                    JValue::Object(&JObject::null()),
                ],
            )
            .map(|_| ())
        });
        if let Err(e) = out {
            eprintln!("[p2p] removeGroup: {e}");
        }
    }

    pub fn stop(&self) {
        self.remove_group();
        let _ = std::fs::remove_file(&self.state);
    }

    pub fn restart(&self, cfg: &GroupConfig) -> Result<(), String> {
        cfg.validate()?;
        self.remove_group();
        std::thread::sleep(Duration::from_secs(1));
        self.ask_for(cfg)
    }

    pub async fn ensure(
        &self,
        cfg: &GroupConfig,
        attempts: u32,
        per_try: Duration,
    ) -> Result<GroupInfo, String> {
        let mut last = String::from("no attempt was made");
        let mut asked = cfg.clone();
        for attempt in 0..attempts {
            match wait_ready(per_try).await {
                Ok(info) => return Ok(info),
                Err(e) => last = e,
            }
            if attempt + 1 < attempts {
                if asked.frequency.is_some() {
                    eprintln!(
                        "[p2p] no group on {:?} MHz after {per_try:?}, trying the band",
                        asked.frequency
                    );
                    asked.frequency = None;
                } else if asked.band != Band::Auto {
                    eprintln!(
                        "[p2p] no {:?} group after {per_try:?}, leaving the band to the system",
                        asked.band
                    );
                    asked.band = Band::Auto;
                } else {
                    eprintln!("[p2p] no group after {per_try:?}, asking again");
                }
                self.restart(&asked)?;
            }
        }
        Err(last)
    }
}

pub fn find_group() -> Option<GroupInfo> {
    use libc::{AF_INET, AF_INET6, freeifaddrs, getifaddrs, ifaddrs, sockaddr_in, sockaddr_in6};
    use std::net::{Ipv4Addr, Ipv6Addr};

    let mut list: *mut ifaddrs = std::ptr::null_mut();
    // SAFETY: `list` is a valid out pointer, freed below on every path that set it.
    if unsafe { getifaddrs(&mut list) } != 0 {
        return None;
    }
    let mut v4: Vec<(String, Ipv4Addr)> = Vec::new();
    let mut v6: Vec<(String, Ipv6Addr)> = Vec::new();
    let mut cursor = list;
    while !cursor.is_null() {
        // SAFETY: the list is well-formed until `freeifaddrs`.
        let entry = unsafe { &*cursor };
        cursor = entry.ifa_next;
        // SAFETY: `ifa_name` is a NUL-terminated string owned by the list.
        let name = unsafe { std::ffi::CStr::from_ptr(entry.ifa_name) }.to_string_lossy();
        let ours = name.starts_with("p2p-") || name == "p2p0";
        // The flags are not checked: without the netlink link query, which apps may not make,
        // bionic reports meaningless flags (0x2 for every interface, no IFF_UP).
        if !ours || entry.ifa_addr.is_null() {
            continue;
        }
        // SAFETY: `ifa_addr` is non-null and its family says which sockaddr it is.
        match i32::from(unsafe { (*entry.ifa_addr).sa_family }) {
            AF_INET => {
                // SAFETY: family AF_INET is a `sockaddr_in`.
                let address = unsafe { &*(entry.ifa_addr as *const sockaddr_in) };
                v4.push((
                    name.to_string(),
                    Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr)),
                ));
            }
            AF_INET6 => {
                // SAFETY: family AF_INET6 is a `sockaddr_in6`.
                let sin6 = unsafe { &*(entry.ifa_addr as *const sockaddr_in6) };
                let ip = Ipv6Addr::from(sin6.sin6_addr.s6_addr);
                if ip.octets()[0] == 0xfe && ip.octets()[1] & 0xc0 == 0x80 {
                    v6.push((name.to_string(), ip));
                }
            }
            _ => {}
        }
    }
    let found = v4.first().map(|(iface, ip)| {
        let link_local = v6.iter().find(|(name, _)| name == iface).map(|(_, ip)| *ip);
        (iface.clone(), *ip, link_local)
    });
    // SAFETY: `list` came from `getifaddrs` and nothing points into it any more.
    unsafe { freeifaddrs(list) };
    found.map(|(iface, ip, link_local)| GroupInfo {
        iface,
        ip,
        bssid: link_local.and_then(eui64_to_mac),
    })
}

pub fn wifi_enabled(java: Java) -> bool {
    java.call(|env, activity| {
        let service = env.new_string("wifi")?;
        let manager = env
            .call(
                activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[JValue::Object(&service)],
            )?
            .l()?;
        if manager.is_null() {
            return Ok(true);
        }
        env.call(&manager, "isWifiEnabled", "()Z", &[])?.z()
    })
    .unwrap_or(true)
}

pub fn location_enabled(java: Java) -> bool {
    java.call(|env, activity| {
        let service = env.new_string("location")?;
        let manager = env
            .call(
                activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[JValue::Object(&service)],
            )?
            .l()?;
        if manager.is_null() {
            return Ok(true);
        }
        env.call(&manager, "isLocationEnabled", "()Z", &[])?.z()
    })
    // A check that cannot be made is not a reason to refuse.
    .unwrap_or(true)
}

/// Whether the developer option "Wireless display certification" is on. With it on, the system's
/// Miracast code treats any Wi-Fi Direct group as an incoming display and, when it cannot listen
/// for one, takes the whole system process down: the screen restarts. Seen on a Pixel 10 Pro XL.
pub fn wifi_display_cert_mode(java: Java) -> bool {
    java.call(|env, activity| {
        let resolver = env
            .call(
                activity,
                "getContentResolver",
                "()Landroid/content/ContentResolver;",
                &[],
            )?
            .l()?;
        let key = env.new_string("wifi_display_certification_on")?;
        env.call_static(
            "android/provider/Settings$Global",
            "getInt",
            "(Landroid/content/ContentResolver;Ljava/lang/String;I)I",
            &[
                JValue::Object(&resolver),
                JValue::Object(&key),
                JValue::Int(0),
            ],
        )?
        .i()
    })
    .is_some_and(|value| value != 0)
}

pub async fn wait_ready(timeout: Duration) -> Result<GroupInfo, String> {
    let start = std::time::Instant::now();
    loop {
        if let Some(info) = find_group() {
            return Ok(info);
        }
        if start.elapsed() >= timeout {
            return Err(format!("no Wi-Fi Direct group after {timeout:?}"));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

impl Java {
    /// What Wi-Fi Direct needs on this release. From Android 13 on `NEARBY_WIFI_DEVICES`, and as
    /// `cargo-apk` cannot declare `neverForLocation` for it, the location permission as well.
    fn p2p_permissions(env: &mut Env) -> Result<Vec<&'static str>, JError> {
        let sdk = env
            .static_field("android/os/Build$VERSION", "SDK_INT", "I")?
            .i()?;
        Ok(if sdk >= ANDROID_13 {
            vec![NEARBY_WIFI_DEVICES, ACCESS_FINE_LOCATION]
        } else {
            vec![ACCESS_FINE_LOCATION]
        })
    }

    pub fn has_p2p_permission(self) -> bool {
        self.call(|env, activity| {
            for permission in Self::p2p_permissions(env)? {
                let name = env.new_string(permission)?;
                let state = env
                    .call(
                        activity,
                        "checkSelfPermission",
                        "(Ljava/lang/String;)I",
                        &[JValue::Object(&name)],
                    )?
                    .i()?;
                if state != PERMISSION_GRANTED {
                    return Ok(false);
                }
            }
            Ok(true)
        }) == Some(true)
    }

    pub fn ask_for_p2p_permission(self) {
        self.call(|env, activity| {
            let permissions = Self::p2p_permissions(env)?;
            let string = env.class("java/lang/String")?;
            let first = env.new_string(permissions[0])?;
            let list = env.new_object_array(permissions.len() as i32, string, &first)?;
            for (i, permission) in permissions.iter().enumerate() {
                let name = env.new_string(permission)?;
                list.set_element(env, i, &name)?;
            }
            env.call(
                activity,
                "requestPermissions",
                "([Ljava/lang/String;I)V",
                &[JValue::Object(&list), JValue::Int(WIFI_DIRECT_REQUEST_CODE)],
            )
            .map(|_| ())
        });
    }
}
