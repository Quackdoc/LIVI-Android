// SPDX-FileCopyrightText: 2025-2026 Mitchel Stewart
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! The phones this head unit has connected to. The settings' Devices page lists them, and the
//! wireless mode wakes them, most recently connected first, when Auto Connect is on. It is what
//! `livi-core`'s device registry is on Linux, with the little the Android port needs: a name and an
//! address per phone, kept in a file.

pub mod pure {
    use std::collections::{HashMap, HashSet};

    /// `BluetoothClass.Device.Major.PHONE`.
    pub const MAJOR_PHONE: i32 = 0x0200;

    /// Upper-case, colon separated: how an address is kept and compared.
    pub fn norm_mac(mac: &str) -> String {
        mac.trim().to_uppercase().replace('-', ":")
    }

    /// The phones to wake, in order. Only a paired device can be connected to, so only those are
    /// considered. A phone that connected before comes first, the latest first, then any other
    /// paired device that is a phone. A device with no class is left alone: a headset or a car kit
    /// would only be disturbed by it. `live` are the ones that have a session, which need nothing.
    pub fn wake_order(
        known: &[(String, f64)],
        bonded: &[(String, Option<i32>)],
        live: &HashSet<String>,
    ) -> Vec<String> {
        let paired: HashMap<String, Option<i32>> =
            bonded.iter().map(|(m, c)| (norm_mac(m), *c)).collect();
        let mut recent: Vec<&(String, f64)> = known.iter().collect();
        recent.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let mut out: Vec<String> = Vec::new();
        for (mac, _) in recent {
            let mac = norm_mac(mac);
            if paired.contains_key(&mac) && !live.contains(&mac) && !out.contains(&mac) {
                out.push(mac);
            }
        }
        for (mac, class) in bonded {
            let mac = norm_mac(mac);
            if *class == Some(MAJOR_PHONE) && !live.contains(&mac) && !out.contains(&mac) {
                out.push(mac);
            }
        }
        out
    }

    pub fn pairing_target(
        known: &[(String, f64)],
        bonded: &[String],
        tries: &HashMap<String, u32>,
        max_tries: u32,
    ) -> Option<String> {
        let paired: HashSet<String> = bonded.iter().map(|m| norm_mac(m)).collect();
        let mut recent: Vec<&(String, f64)> = known.iter().collect();
        recent.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        recent
            .into_iter()
            .map(|(m, _)| norm_mac(m))
            .find(|m| !paired.contains(m) && tries.get(m).copied().unwrap_or(0) < max_tries)
    }
}

pub use pure::*;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use livi_core_proto::state::{DeviceStatus, DeviceView, Protocol};
use serde_json::{Value, json};

/// A phone this head unit has connected to.
#[derive(Clone, Debug, PartialEq)]
struct Entry {
    mac: String,
    name: String,
    model: Option<String>,
    last_seen: f64,
}

pub struct Registry {
    path: PathBuf,
    entries: Mutex<Vec<Entry>>,
    live: Mutex<Option<String>>,
    reachable: Mutex<HashSet<String>>,
    changed: Box<dyn Fn(Vec<DeviceView>) + Send + Sync>,
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Registry {
    pub fn load(
        path: PathBuf,
        changed: impl Fn(Vec<DeviceView>) + Send + Sync + 'static,
    ) -> Arc<Self> {
        let entries = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|v| {
                Some(Entry {
                    mac: norm_mac(v["btMac"].as_str()?),
                    name: v["name"].as_str().unwrap_or_default().to_owned(),
                    model: v["model"].as_str().map(str::to_owned),
                    last_seen: v["lastSeen"].as_f64().unwrap_or(0.0),
                })
            })
            .collect();
        Arc::new(Self {
            path,
            entries: Mutex::new(entries),
            live: Mutex::new(None),
            reachable: Mutex::new(HashSet::new()),
            changed: Box::new(changed),
        })
    }

    fn save(&self, entries: &[Entry]) {
        let list: Vec<Value> = entries
			.iter()
			.map(|e| {
				json!({ "btMac": e.mac, "name": e.name, "model": e.model, "lastSeen": e.last_seen })
			})
			.collect();
        let text = serde_json::to_string_pretty(&list).unwrap_or_default();
        if let Err(e) = std::fs::write(&self.path, text) {
            eprintln!("[devices] {} not saved: {e}", self.path.display());
        }
    }

    pub fn publish(&self) {
        (self.changed)(self.views());
    }

    pub fn views(&self) -> Vec<DeviceView> {
        let live = lock(&self.live).clone();
        let reachable = lock(&self.reachable).clone();
        let mut entries = lock(&self.entries).clone();
        entries.sort_by(|a, b| {
            b.last_seen
                .partial_cmp(&a.last_seen)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        entries
            .into_iter()
            .map(|e| {
                let status = if live.as_deref() == Some(e.mac.as_str()) {
                    DeviceStatus::Active
                } else if reachable.contains(&e.mac) {
                    DeviceStatus::Available
                } else {
                    DeviceStatus::Offline
                };
                DeviceView {
                    id: e.mac.clone(),
                    name: Some(e.name).filter(|n| !n.is_empty()),
                    model: e.model,
                    protocol: Some(Protocol::Androidauto),
                    last_transport: Some("wifi".into()),
                    status,
                    // The Devices page only lets a phone with a running session be picked.
                    session: (status == DeviceStatus::Active).then_some(1),
                    ..DeviceView::default()
                }
            })
            .collect()
    }

    pub fn recents(&self) -> Vec<(String, f64)> {
        lock(&self.entries)
            .iter()
            .map(|e| (e.mac.clone(), e.last_seen))
            .collect()
    }

    /// A phone completed the Android Auto setup. It is remembered, or its time moves up.
    pub fn note_connected(&self, mac: &str, name: &str) {
        let mac = norm_mac(mac);
        {
            let mut entries = lock(&self.entries);
            match entries.iter_mut().find(|e| e.mac == mac) {
                Some(e) => {
                    e.last_seen = now();
                    if !name.is_empty() {
                        e.name = name.to_owned();
                    }
                }
                None => entries.push(Entry {
                    mac,
                    name: name.to_owned(),
                    model: None,
                    last_seen: now(),
                }),
            }
            self.save(&entries);
        }
        self.publish();
    }

    /// The phone model, which a session learns after the setup.
    pub fn note_model(&self, mac: &str, model: &str) {
        let mac = norm_mac(mac);
        let changed = {
            let mut entries = lock(&self.entries);
            let Some(e) = entries.iter_mut().find(|e| e.mac == mac) else {
                return;
            };
            if model.is_empty() || e.model.as_deref() == Some(model) {
                false
            } else {
                e.model = Some(model.to_owned());
                self.save(&entries);
                true
            }
        };
        if changed {
            self.publish();
        }
    }

    /// Which phone has the running session, if any.
    pub fn set_live(&self, mac: Option<&str>) {
        let mac = mac.map(norm_mac);
        let changed = {
            let mut live = lock(&self.live);
            let changed = *live != mac;
            *live = mac;
            changed
        };
        if changed {
            self.publish();
        }
    }

    pub fn live(&self) -> Option<String> {
        lock(&self.live).clone()
    }

    /// Whether a wake-up reached the phone, which is what "available" means here.
    pub fn set_reachable(&self, mac: &str, yes: bool) {
        let mac = norm_mac(mac);
        let changed = {
            let mut set = lock(&self.reachable);
            if yes {
                set.insert(mac)
            } else {
                set.remove(&mac)
            }
        };
        if changed {
            self.publish();
        }
    }

    pub fn forget(&self, id: &str) -> bool {
        let mac = norm_mac(id);
        let removed = {
            let mut entries = lock(&self.entries);
            let before = entries.len();
            entries.retain(|e| e.mac != mac);
            let removed = entries.len() != before;
            if removed {
                self.save(&entries);
            }
            removed
        };
        lock(&self.reachable).remove(&mac);
        if removed {
            self.publish();
        }
        removed
    }
}
