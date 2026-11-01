// SPDX-FileCopyrightText: 2025-2026 Mitchel Stewart
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use std::collections::HashMap;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jni::Env;
use jni::errors::Result as JResult;
use jni::objects::{JObject, JObjectArray, JString, JValue};
use livi_aa::usb::{is_accessory_id, is_phone_candidate};
use tokio::runtime::Handle;
use tokio::sync::Notify;

use crate::android::env::{EnvExt, Ref, vm_of};
use crate::session::helper::Helper;

const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const PERMISSION_ASK_INTERVAL: Duration = Duration::from_secs(8);
const RETRY_AFTER_SESSION_END: Duration = Duration::from_secs(3);
const FLAG_IMMUTABLE: i32 = 0x0400_0000;
const PERMISSION_ACTION: &str = "dev.f_io.livi.USB_PERMISSION";

struct Found {
    name: String,
    vendor: u16,
    product: u16,
    class: u8,
    interfaces: Vec<u8>,
}

enum State {
    Switched,
    Serving,
    Ended(Instant),
    Failed,
}

type Shared = Arc<Mutex<HashMap<String, State>>>;

struct Usb<'a, 'l> {
    env: &'a mut Env<'l>,
    activity: &'a JObject<'l>,
    manager: JObject<'l>,
}

fn object<'l>(
    env: &mut Env<'l>,
    obj: &JObject,
    name: &str,
    sig: &str,
    args: &[JValue],
) -> JResult<JObject<'l>> {
    env.call(obj, name, sig, args)?.l()
}

fn text(env: &mut Env, obj: JObject) -> JResult<String> {
    let string = env.cast_local::<JString>(obj)?;
    string.try_to_string(env)
}

impl<'a, 'l> Usb<'a, 'l> {
    fn new(env: &'a mut Env<'l>, activity: &'a JObject<'l>) -> JResult<Self> {
        let service = env.new_string("usb")?;
        let manager = object(
            env,
            activity,
            "getSystemService",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[JValue::Object(&service)],
        )?;
        Ok(Self {
            env,
            activity,
            manager,
        })
    }

    fn devices(&mut self) -> JResult<JObject<'l>> {
        object(
            self.env,
            &self.manager,
            "getDeviceList",
            "()Ljava/util/HashMap;",
            &[],
        )
    }

    fn device(&mut self, name: &str) -> JResult<Option<JObject<'l>>> {
        let map = self.devices()?;
        let key = self.env.new_string(name)?;
        let dev = object(
            self.env,
            &map,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[JValue::Object(&key)],
        )?;
        Ok((!dev.is_null()).then_some(dev))
    }

    fn list(&mut self) -> JResult<Vec<Found>> {
        let map = self.devices()?;
        let values = object(self.env, &map, "values", "()Ljava/util/Collection;", &[])?;
        let array = object(self.env, &values, "toArray", "()[Ljava/lang/Object;", &[])?;
        let array = self.env.cast_local::<JObjectArray>(array)?;
        let count = array.len(self.env)?;
        let mut found = Vec::new();
        for i in 0..count {
            let dev = array.get_element(self.env, i)?;
            let name = object(self.env, &dev, "getDeviceName", "()Ljava/lang/String;", &[])?;
            let name = text(self.env, name)?;
            let vendor = self.env.call(&dev, "getVendorId", "()I", &[])?.i()?;
            let product = self.env.call(&dev, "getProductId", "()I", &[])?.i()?;
            let class = self.env.call(&dev, "getDeviceClass", "()I", &[])?.i()?;
            let interface_count = self.env.call(&dev, "getInterfaceCount", "()I", &[])?.i()?;
            let mut interfaces = Vec::new();
            for index in 0..interface_count {
                let iface = object(
                    self.env,
                    &dev,
                    "getInterface",
                    "(I)Landroid/hardware/usb/UsbInterface;",
                    &[JValue::Int(index)],
                )?;
                let class = self
                    .env
                    .call(&iface, "getInterfaceClass", "()I", &[])?
                    .i()?;
                interfaces.push(class as u8);
            }
            found.push(Found {
                name,
                vendor: vendor as u16,
                product: product as u16,
                class: class as u8,
                interfaces,
            });
        }
        Ok(found)
    }

    fn has_permission(&mut self, dev: &JObject) -> JResult<bool> {
        self.env
            .call(
                &self.manager,
                "hasPermission",
                "(Landroid/hardware/usb/UsbDevice;)Z",
                &[JValue::Object(dev)],
            )?
            .z()
    }

    /// Shows the system's dialog. Nothing reads its answer, the next poll sees the permission.
    fn request_permission(&mut self, dev: &JObject) -> JResult<()> {
        let action = self.env.new_string(PERMISSION_ACTION)?;
        let intent = self.env.construct(
            "android/content/Intent",
            "(Ljava/lang/String;)V",
            &[JValue::Object(&action)],
        )?;
        let package = object(
            self.env,
            self.activity,
            "getPackageName",
            "()Ljava/lang/String;",
            &[],
        )?;
        self.env.call(
            &intent,
            "setPackage",
            "(Ljava/lang/String;)Landroid/content/Intent;",
            &[JValue::Object(&package)],
        )?;
        let pending = self
            .env
            .call_static(
                "android/app/PendingIntent",
                "getBroadcast",
                "(Landroid/content/Context;ILandroid/content/Intent;I)Landroid/app/PendingIntent;",
                &[
                    JValue::Object(self.activity),
                    JValue::Int(0),
                    JValue::Object(&intent),
                    JValue::Int(FLAG_IMMUTABLE),
                ],
            )?
            .l()?;
        self.env
            .call(
                &self.manager,
                "requestPermission",
                "(Landroid/hardware/usb/UsbDevice;Landroid/app/PendingIntent;)V",
                &[JValue::Object(dev), JValue::Object(&pending)],
            )
            .map(|_| ())
    }

    fn open(&mut self, dev: &JObject) -> JResult<Option<(OwnedFd, String, Ref)>> {
        let conn = object(
            self.env,
            &self.manager,
            "openDevice",
            "(Landroid/hardware/usb/UsbDevice;)Landroid/hardware/usb/UsbDeviceConnection;",
            &[JValue::Object(dev)],
        )?;
        if conn.is_null() {
            return Ok(None);
        }
        let fd = self.env.call(&conn, "getFileDescriptor", "()I", &[])?.i()?;
        // SAFETY: dup of a descriptor the connection holds open.
        let own = unsafe { libc::dup(fd) };
        if own < 0 {
            return Ok(None);
        }
        // SAFETY: `own` is a fresh descriptor nobody else owns.
        let own = unsafe { OwnedFd::from_raw_fd(own) };
        let serial = match object(
            self.env,
            dev,
            "getSerialNumber",
            "()Ljava/lang/String;",
            &[],
        ) {
            Ok(s) if !s.is_null() => text(self.env, s).unwrap_or_default(),
            _ => {
                self.env.exception_clear();
                String::new()
            }
        };
        let keep = self.env.global(&conn)?;
        Ok(Some((own, serial, keep)))
    }
}

struct Connections(HashMap<String, Ref>);

fn clear_on_error<T>(env: &mut Env, what: &str, result: JResult<T>) -> Option<T> {
    match result {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("[usb] {what}: {e}");
            env.exception_clear();
            None
        }
    }
}

pub fn watch(vm: usize, activity: usize, helper: Helper, runtime: Handle) {
    let spawned = std::thread::Builder::new()
        .name("livi-usb".into())
        .spawn(move || {
            // The thread stays attached for as long as it runs, which is as long as the process.
            let attached = vm_of(vm).attach_current_thread(|env| -> JResult<()> {
                // SAFETY: the activity is a global reference of the framework, valid for the process.
                let activity = unsafe { JObject::from_raw(env, activity as jni::sys::jobject) };
                let activity = env.global(&activity)?;
                println!("[usb] watching for phones");
                run(env, &activity, &helper, &runtime);
                Ok(())
            });
            if let Err(e) = attached {
                eprintln!("[usb] cannot attach: {e}");
            }
        });
    if let Err(e) = spawned {
        eprintln!("[usb] no watch thread: {e}");
    }
}

fn run(env: &mut Env, activity: &Ref, helper: &Helper, runtime: &Handle) {
    let states: Shared = Arc::default();
    let mut connections = Connections(HashMap::new());
    let mut cancels: HashMap<String, Arc<Notify>> = HashMap::new();
    let mut asked: HashMap<String, Instant> = HashMap::new();
    loop {
        std::thread::sleep(DEVICE_POLL_INTERVAL);
        let step = env.with_local_frame(128, |env| -> JResult<()> {
            let activity = env.new_local_ref(&**activity)?;
            let mut usb = Usb::new(env, &activity)?;
            let found = usb.list()?;
            let names: Vec<&str> = found.iter().map(|listed| listed.name.as_str()).collect();

            let gone: Vec<String> = states
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .keys()
                .filter(|n| !names.contains(&n.as_str()))
                .cloned()
                .collect();
            for name in gone {
                states
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&name);
                asked.remove(&name);
                if let Some(cancel) = cancels.remove(&name) {
                    cancel.notify_one();
                }
                if let Some(conn) = connections.0.remove(&name) {
                    let _ = usb.env.call(&conn, "close", "()V", &[]);
                }
            }

            for listed in found {
                let accessory = is_accessory_id(listed.vendor, listed.product);
                if !accessory
                    && !is_phone_candidate(listed.vendor, listed.class, &listed.interfaces)
                {
                    continue;
                }
                {
                    let mut tracked = states.lock().unwrap_or_else(|e| e.into_inner());
                    match tracked.get(&listed.name) {
                        Some(State::Ended(at)) if at.elapsed() > RETRY_AFTER_SESSION_END => {
                            tracked.remove(&listed.name);
                            if let Some(conn) = connections.0.remove(&listed.name) {
                                let _ = usb.env.call(&conn, "close", "()V", &[]);
                            }
                        }
                        Some(_) => continue,
                        None => {}
                    }
                }
                let Some(dev) = usb.device(&listed.name)? else {
                    continue;
                };
                if !usb.has_permission(&dev)? {
                    let due = asked
                        .get(&listed.name)
                        .is_none_or(|asked_at| asked_at.elapsed() > PERMISSION_ASK_INTERVAL);
                    if due {
                        println!(
                            "[usb] asking for permission to use {:04x}:{:04x}",
                            listed.vendor, listed.product
                        );
                        usb.request_permission(&dev)?;
                        asked.insert(listed.name.clone(), Instant::now());
                    }
                    continue;
                }
                let Some((fd, serial, conn)) = usb.open(&dev)? else {
                    eprintln!("[usb] {} would not open", listed.name);
                    states
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(listed.name.clone(), State::Failed);
                    continue;
                };
                connections.0.insert(listed.name.clone(), conn);
                let (helper, states_task, name) =
                    (helper.clone(), states.clone(), listed.name.clone());
                if accessory {
                    println!("[usb] {name}: accessory, starting a session");
                    let cancel = Arc::new(Notify::new());
                    cancels.insert(listed.name.clone(), cancel.clone());
                    states
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(listed.name.clone(), State::Serving);
                    runtime.spawn(async move {
                        if let Err(e) = helper.serve_phone(fd, serial, cancel).await {
                            eprintln!("[usb] {name}: {e}");
                        }
                        states_task
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(name, State::Ended(Instant::now()));
                    });
                } else {
                    println!(
                        "[usb] {name}: phone {:04x}:{:04x}, switching to accessory mode",
                        listed.vendor, listed.product
                    );
                    states
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(listed.name.clone(), State::Switched);
                    runtime.spawn(async move {
                        match helper.switch(fd).await {
                            Ok(()) => println!("[usb] {name}: accessory mode requested"),
                            Err(e) => {
                                eprintln!("[usb] {name}: {e}");
                                states_task
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .insert(name, State::Failed);
                            }
                        }
                    });
                }
            }
            Ok(())
        });
        clear_on_error(env, "poll", step);
    }
}
