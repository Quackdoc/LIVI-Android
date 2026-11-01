// SPDX-FileCopyrightText: 2025-2026 Mitchel Stewart
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

pub mod pure {
    pub fn valid_mac(mac: &str) -> bool {
        let bytes = mac.as_bytes();
        bytes.len() == 17
            && bytes.iter().enumerate().all(|(i, b)| match i % 3 {
                2 => *b == b':',
                _ => b.is_ascii_hexdigit(),
            })
    }

    pub fn valid_uuid(uuid: &str) -> bool {
        let bytes = uuid.as_bytes();
        bytes.len() == 36
            && bytes.iter().enumerate().all(|(i, b)| match i {
                8 | 13 | 18 | 23 => *b == b'-',
                _ => b.is_ascii_hexdigit(),
            })
    }
}

pub use pure::*;

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use jni::Env;
use jni::errors::Error as JError;
use jni::objects::{JObject, JObjectArray, JString, JValue};

use crate::android::env::{EnvExt, Ref, describe, vm_of, with_env};
use crate::android::java::Java;

/// Service UUID of wireless Android Auto, the record a phone looks for.
pub const AA_UUID: &str = "4de17a00-52cb-11e6-bdf4-0800200c9a66";
/// Hands-Free, our stand-in record: what the phone offers under the Audio Gateway role.
pub const HFP_HF_UUID: &str = "0000111e-0000-1000-8000-00805f9b34fb";
/// The phone's Audio Gateway, for an RFCOMM link that does not need Bluetooth Auto.
pub const HFP_AG_UUID: &str = "0000111f-0000-1000-8000-00805f9b34fb";
/// The phone's Headset Audio Gateway.
pub const HSP_AG_UUID: &str = "00001112-0000-1000-8000-00805f9b34fb";

const DEVICE_TYPE: &str = "Landroid/bluetooth/BluetoothDevice;";
const SOCKET_TYPE: &str = "Landroid/bluetooth/BluetoothSocket;";
const SERVER_SOCKET_TYPE: &str = "Landroid/bluetooth/BluetoothServerSocket;";
const BUILD_VERSION_CLASS: &str = "android/os/Build$VERSION";
/// Android 12 API val, where `BLUETOOTH_CONNECT` replaced the install-time Bluetooth permissions.
const ANDROID_12: i32 = 31;
const BLUETOOTH_CONNECT: &str = "android.permission.BLUETOOTH_CONNECT";
const PERMISSION_GRANTED: i32 = 0;
const BLUETOOTH_REQUEST_CODE: i32 = 3;
/// One chunk of the bridge: the Java byte array is allocated once per thread with this size.
const BRIDGE_CHUNK_BYTES: usize = 4096;

/// A bonded device. `name` is empty for a device that never reported one.
#[derive(Clone, Debug)]
pub struct Device {
    pub mac: String,
    pub name: String,
    pub major_class: Option<i32>,
}

#[derive(Clone)]
pub struct Bt {
    vm: usize,
    adapter: Ref,
}

fn device_info(env: &mut Env, obj: &JObject) -> Result<Device, JError> {
    let mac = env
        .call(obj, "getAddress", "()Ljava/lang/String;", &[])?
        .l()?;
    let mac = text(env, mac)?;
    let name = env.call(obj, "getName", "()Ljava/lang/String;", &[])?.l()?;
    let name = text(env, name)?;
    // A device that never reported a class has none, which is not an error.
    let class = env
        .call(
            obj,
            "getBluetoothClass",
            "()Landroid/bluetooth/BluetoothClass;",
            &[],
        )?
        .l()?;
    let major_class = if class.is_null() {
        None
    } else {
        Some(env.call(&class, "getMajorDeviceClass", "()I", &[])?.i()?)
    };
    Ok(Device {
        mac,
        name,
        major_class,
    })
}

fn text(env: &mut Env, obj: JObject) -> Result<String, JError> {
    if obj.is_null() {
        return Ok(String::new());
    }
    let string = env.cast_local::<JString>(obj)?;
    string.try_to_string(env)
}

fn uuid_of<'l>(env: &mut Env<'l>, uuid: &str) -> Result<JObject<'l>, JError> {
    let string = env.new_string(uuid)?;
    env.call_static(
        "java/util/UUID",
        "fromString",
        "(Ljava/lang/String;)Ljava/util/UUID;",
        &[JValue::Object(&string)],
    )?
    .l()
}

impl Bt {
    /// The framework's adapter. A device without Bluetooth gives `no Bluetooth`.
    pub fn new(java: Java) -> Result<Bt, String> {
        let (vm, activity) = java.raw();
        let adapter = with_env(vm, |env| {
            // SAFETY: the activity is a global reference of the framework, valid for the process.
            let activity = unsafe { JObject::from_raw(env, activity as jni::sys::jobject) };
            let service = env.new_string("bluetooth")?;
            let manager = env
                .call(
                    &activity,
                    "getSystemService",
                    "(Ljava/lang/String;)Ljava/lang/Object;",
                    &[JValue::Object(&service)],
                )?
                .l()?;
            let mut adapter = JObject::null();
            if !manager.is_null() {
                adapter = env
                    .call(
                        &manager,
                        "getAdapter",
                        "()Landroid/bluetooth/BluetoothAdapter;",
                        &[],
                    )?
                    .l()?;
            }
            if adapter.is_null() {
                return Ok(None);
            }
            Ok(Some(env.global(&adapter)?))
        })?;
        let adapter = adapter.ok_or("no Bluetooth")?;
        Ok(Bt { vm, adapter })
    }

    /// Whether the user has Bluetooth switched on.
    pub fn enabled(&self) -> bool {
        with_env(self.vm, |env| {
            env.call(&self.adapter, "isEnabled", "()Z", &[])?.z()
        })
        .unwrap_or(false)
    }

    /// The devices that are paired with this head unit.
    /// The bond state of a device: 10 none, 11 bonding, 12 bonded.
    pub fn bond_state(&self, mac: &str) -> Result<i32, String> {
        if !valid_mac(mac) {
            return Err(format!("not a Bluetooth address: {mac}"));
        }
        with_env(self.vm, |env| {
            let device = self.remote(env, mac)?;
            env.call(&device, "getBondState", "()I", &[])?.i()
        })
    }

    /// Starts pairing with a device that is in range.
    /// Both phones ask their user to confirm. Returns whether pairing was started.
    pub fn create_bond(&self, mac: &str) -> Result<bool, String> {
        if !valid_mac(mac) {
            return Err(format!("not a Bluetooth address: {mac}"));
        }
        with_env(self.vm, |env| {
            let device = self.remote(env, mac)?;
            env.call(&device, "createBond", "()Z", &[])?.z()
        })
    }

    /// `removeBond` is not public API, so try to release.
    /// blocks the method answers with an error,
    /// and the user has to forget the device in the system settings.
    pub fn remove_bond(&self, mac: &str) -> Result<bool, String> {
        if !valid_mac(mac) {
            return Err(format!("not a Bluetooth address: {mac}"));
        }
        with_env(self.vm, |env| {
            let device = self.remote(env, mac)?;
            env.call(&device, "removeBond", "()Z", &[])?.z()
        })
    }

    fn remote<'l>(&self, env: &mut Env<'l>, mac: &str) -> Result<JObject<'l>, JError> {
        let address = env.new_string(mac)?;
        env.call(
            &self.adapter,
            "getRemoteDevice",
            format!("(Ljava/lang/String;){DEVICE_TYPE}"),
            &[JValue::Object(&address)],
        )?
        .l()
    }

    pub fn bonded(&self) -> Result<Vec<Device>, String> {
        with_env(self.vm, |env| {
            let set = env
                .call(&self.adapter, "getBondedDevices", "()Ljava/util/Set;", &[])?
                .l()?;
            let mut out = Vec::new();
            if set.is_null() {
                return Ok(out);
            }
            let iter = env
                .call(&set, "iterator", "()Ljava/util/Iterator;", &[])?
                .l()?;
            while env.call(&iter, "hasNext", "()Z", &[])?.z()? {
                let next = env.call(&iter, "next", "()Ljava/lang/Object;", &[])?.l()?;
                if next.is_null() {
                    continue;
                }
                out.push(device_info(env, &next)?);
            }
            Ok(out)
        })
    }

    pub fn listen(&self, name: &str, uuid: &str, secure: bool) -> Result<Listener, String> {
        if !valid_uuid(uuid) {
            return Err(format!("invalid UUID: {uuid}"));
        }
        let server = with_env(self.vm, |env| {
            let record = env.new_string(name)?;
            let id = uuid_of(env, uuid)?;
            let method = if secure {
                "listenUsingRfcommWithServiceRecord"
            } else {
                "listenUsingInsecureRfcommWithServiceRecord"
            };
            let server = env
                .call(
                    &self.adapter,
                    method,
                    format!("(Ljava/lang/String;Ljava/util/UUID;){SERVER_SOCKET_TYPE}"),
                    &[JValue::Object(&record), JValue::Object(&id)],
                )?
                .l()?;
            if server.is_null() {
                return Ok(None);
            }
            Ok(Some(env.global(&server)?))
        })?;
        let server = server.ok_or_else(|| format!("no server socket for {uuid}"))?;
        eprintln!(
            "[bt] listening as {name:?} on {uuid} ({})",
            if secure { "secure" } else { "insecure" }
        );
        Ok(Listener {
            inner: Arc::new(Server {
                vm: self.vm,
                server,
            }),
        })
    }

    pub fn connect(&self, mac: &str, uuid: &str) -> Result<Conn, String> {
        if !valid_mac(mac) {
            return Err(format!("invalid MAC address: {mac}"));
        }
        if !valid_uuid(uuid) {
            return Err(format!("invalid UUID: {uuid}"));
        }
        // Opening the socket is separate from connecting it, so a failed connect can still close it
        // instead of leaving it to the garbage collector.
        let (socket, remote) = with_env(self.vm, |env| {
            let id = uuid_of(env, uuid)?;
            let address = env.new_string(mac)?;
            let device = env
                .call(
                    &self.adapter,
                    "getRemoteDevice",
                    format!("(Ljava/lang/String;){DEVICE_TYPE}"),
                    &[JValue::Object(&address)],
                )?
                .l()?;
            let socket = env
                .call(
                    &device,
                    "createRfcommSocketToServiceRecord",
                    format!("(Ljava/util/UUID;){SOCKET_TYPE}"),
                    &[JValue::Object(&id)],
                )?
                .l()?;
            if socket.is_null() {
                return Ok(None);
            }
            let remote = device_info(env, &device)?;
            Ok(Some((env.global(&socket)?, remote)))
        })?
        .ok_or_else(|| format!("no RFCOMM socket for {mac} {uuid}"))?;
        let joined = with_env(self.vm, |env| {
            env.call(&socket, "connect", "()V", &[]).map(|_| ())
        });
        if let Err(e) = joined {
            eprintln!("[bt] connect to {mac} failed: {e}");
            let _ = with_env(self.vm, |env| {
                env.call(&socket, "close", "()V", &[]).map(|_| ())
            });
            return Err(e);
        }
        eprintln!("[bt] connected to {mac} ({})", remote.name);
        Ok(Conn {
            id: NEXT_LINK_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            mac: remote.mac,
            name: remote.name,
            vm: self.vm,
            socket,
        })
    }
}

struct Server {
    vm: usize,
    server: Ref,
}

#[derive(Clone)]
pub struct Listener {
    inner: Arc<Server>,
}

impl Listener {
    /// Blocks until a phone connects. The error of an accept that was closed from another thread is
    /// a `java.io.IOException`, not something to act on.
    pub fn accept(&self) -> Result<Conn, String> {
        let (socket, remote) = with_env(self.inner.vm, |env| {
            let socket = env
                .call(
                    &self.inner.server,
                    "accept",
                    format!("(){SOCKET_TYPE}"),
                    &[],
                )?
                .l()?;
            if socket.is_null() {
                return Ok(None);
            }
            let device = env
                .call(&socket, "getRemoteDevice", format!("(){DEVICE_TYPE}"), &[])?
                .l()?;
            let remote = device_info(env, &device)?;
            Ok(Some((env.global(&socket)?, remote)))
        })?
        .ok_or("the server socket returned no socket")?;
        eprintln!("[bt] accepted {} ({})", remote.mac, remote.name);
        Ok(Conn {
            id: NEXT_LINK_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            mac: remote.mac,
            name: remote.name,
            vm: self.inner.vm,
            socket,
        })
    }

    pub fn close(&self) {
        let out = with_env(self.inner.vm, |env| {
            env.call(&self.inner.server, "close", "()V", &[])
                .map(|_| ())
        });
        if let Err(e) = out {
            eprintln!("[bt] server close: {e}");
        }
    }
}

static NEXT_LINK_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

pub struct Conn {
    id: u32,
    pub mac: String,
    pub name: String,
    vm: usize,
    socket: Ref,
}

impl Conn {
    /// Bridges the socket to an ordinary unix stream pair and returns
    pub fn into_stream(self) -> std::io::Result<UnixStream> {
        let (ours, bridge) = UnixStream::pair()?;
        let back = bridge.try_clone()?;
        let vm = self.vm;
        let socket = self.socket.clone();
        let id = self.id;
        eprintln!("[bt] #{id} bridging the link to {}", self.mac);
        // Whichever direction ends first closes the socket and shuts the pair down; that ends the
        // other thread, whether it is blocked in Java or in the pair.
        let inbound_socket = socket.clone();
        std::thread::Builder::new()
            .name("bt-in".into())
            .spawn(move || {
                if let Err(e) = inbound(vm, &inbound_socket, &bridge) {
                    eprintln!("[bt] #{id} inbound: {e}");
                }
                stop(vm, &inbound_socket, &bridge);
            })?;
        let outbound_socket = socket.clone();
        std::thread::Builder::new()
            .name("bt-out".into())
            .spawn(move || {
                if let Err(e) = outbound(vm, &outbound_socket, &back) {
                    eprintln!("[bt] #{id} outbound: {e}");
                }
                stop(vm, &outbound_socket, &back);
            })?;
        Ok(ours)
    }

    /// Closes the link. A thread blocked on the socket throws, which ends the bridge on both sides.
    pub fn close(&self) {
        let out = with_env(self.vm, |env| {
            env.call(&self.socket, "close", "()V", &[]).map(|_| ())
        });
        if let Err(e) = out {
            eprintln!("[bt] socket close: {e}");
        }
    }
}

// A listener is accepted on one thread and closed on another, a connection is handed to the
// bridge threads: the references have to allow that.
const _: fn() = || {
    fn send_sync<T: Send + Sync>() {}
    fn send<T: Send>() {}
    fn clone<T: Clone>() {}
    send_sync::<Listener>();
    clone::<Listener>();
    send::<Conn>();
    clone::<Bt>();
};

/// Ends both halves of a bridge: `close` makes the other side's blocked Java call throw, `shutdown`
/// wakes the pair. Errors are the normal case here, a link that is already gone is what is wanted.
fn stop(vm: usize, socket: &Ref, bridge: &UnixStream) {
    let _ = with_env(vm, |env| env.call(socket, "close", "()V", &[]).map(|_| ()));
    let _ = bridge.shutdown(Shutdown::Both);
}

/// `jbyte` is `i8` and the pair carries bytes: the two views are of the same memory.
fn bytes_of(chunk: &[i8]) -> &[u8] {
    // SAFETY: `i8` and `u8` have the same size and alignment, and the length is kept.
    unsafe { std::slice::from_raw_parts(chunk.as_ptr() as *const u8, chunk.len()) }
}

fn jbytes_of(chunk: &mut [u8]) -> &mut [i8] {
    // SAFETY: as above, nothing writes through the other view while this one is alive.
    unsafe { std::slice::from_raw_parts_mut(chunk.as_mut_ptr() as *mut i8, chunk.len()) }
}

/// Copies what the phone sends into the pair. Ends on EOF, on an error, or when the socket is closed
/// by the other direction.
fn inbound(vm: usize, socket: &Ref, bridge: &UnixStream) -> Result<(), String> {
    // Attached for the life of this thread: the loop runs per chunk, an attach would not.
    let attached = vm_of(vm).attach_current_thread(|env| -> Result<Result<(), String>, JError> {
        Ok(inbound_on(env, socket, bridge))
    });
    attached.map_err(|e| e.to_string())?
}

fn inbound_on(env: &mut Env, socket: &Ref, bridge: &UnixStream) -> Result<(), String> {
    let array = env
        .new_byte_array(BRIDGE_CHUNK_BYTES)
        .map_err(|e| describe(env, e))?;
    let mut chunk = [0i8; BRIDGE_CHUNK_BYTES];
    // `BluetoothSocket.read` is package-private, the public way in is the stream.
    let input = env
        .call(socket, "getInputStream", "()Ljava/io/InputStream;", &[])
        .and_then(|v| v.l())
        .map_err(|e| describe(env, e))?;
    loop {
        let read = env.call(&input, "read", "([B)I", &[JValue::Object(&array)]);
        let n = match read {
            Ok(value) => value.i().map_err(|e| describe(env, e))?,
            Err(e) => return Err(describe(env, e)),
        };
        if n < 0 {
            return Ok(());
        }
        let n = n as usize;
        array
            .get_region(env, 0, &mut chunk[..n])
            .map_err(|e| describe(env, e))?;
        if (&*bridge).write_all(bytes_of(&chunk[..n])).is_err() {
            // The pair is gone, nobody is left to read what the phone sends.
            return Ok(());
        }
    }
}

/// Copies what the pair holds into the socket, one write and flush per chunk.
fn outbound(vm: usize, socket: &Ref, bridge: &UnixStream) -> Result<(), String> {
    // Attached for the life of this thread: the loop runs per chunk, an attach would not.
    let attached = vm_of(vm).attach_current_thread(|env| -> Result<Result<(), String>, JError> {
        Ok(outbound_on(env, socket, bridge))
    });
    attached.map_err(|e| e.to_string())?
}

fn outbound_on(env: &mut Env, socket: &Ref, bridge: &UnixStream) -> Result<(), String> {
    let array = env
        .new_byte_array(BRIDGE_CHUNK_BYTES)
        .map_err(|e| describe(env, e))?;
    let mut chunk = [0u8; BRIDGE_CHUNK_BYTES];
    let output = env
        .call(socket, "getOutputStream", "()Ljava/io/OutputStream;", &[])
        .and_then(|v| v.l())
        .map_err(|e| describe(env, e))?;
    loop {
        let n = match (&*bridge).read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) => return Err(e.to_string()),
        };
        array
            .set_region(env, 0, jbytes_of(&mut chunk[..n]))
            .map_err(|e| describe(env, e))?;
        let args = [
            JValue::Object(&array),
            JValue::Int(0),
            JValue::Int(n as i32),
        ];
        let written = env.call(&output, "write", "([BII)V", &args);
        if let Err(e) = written {
            return Err(describe(env, e));
        }
        let flushed = env.call(&output, "flush", "()V", &[]);
        if let Err(e) = flushed {
            return Err(describe(env, e));
        }
    }
}

impl Java {
    /// What a classic Bluetooth link needs on this release: from Android 12 on the runtime
    /// `BLUETOOTH_CONNECT`, before that the install-time `BLUETOOTH` permissions.
    pub fn has_bt_permission(self) -> bool {
        self.call(|env, activity| {
            let sdk = env.static_field(BUILD_VERSION_CLASS, "SDK_INT", "I")?.i()?;
            if sdk < ANDROID_12 {
                return Ok(true);
            }
            let name = env.new_string(BLUETOOTH_CONNECT)?;
            env.call(
                activity,
                "checkSelfPermission",
                "(Ljava/lang/String;)I",
                &[JValue::Object(&name)],
            )?
            .i()
            .map(|state| state == PERMISSION_GRANTED)
        }) == Some(true)
    }

    /// Shows the system's dialog. The answer is not read, the next check finds the permission.
    pub fn ask_for_bt_permission(self) {
        let _ = self.call(|env, activity| {
            let sdk = env.static_field(BUILD_VERSION_CLASS, "SDK_INT", "I")?.i()?;
            if sdk < ANDROID_12 {
                return Ok(());
            }
            let string = env.class("java/lang/String")?;
            let name = env.new_string(BLUETOOTH_CONNECT)?;
            let list: JObjectArray = env.new_object_array(1, string, &name)?;
            env.call(
                activity,
                "requestPermissions",
                "([Ljava/lang/String;I)V",
                &[JValue::Object(&list), JValue::Int(BLUETOOTH_REQUEST_CODE)],
            )
            .map(|_| ())
        });
    }

    /// Asks Android for a discoverable window of `seconds`. It shows an activity, whose answer is
    /// not read; a head unit that is found by a phone needs it.
    pub fn make_discoverable(self, seconds: u32) {
        let _ = self.call(|env, activity| {
            let action = env.new_string("android.bluetooth.adapter.action.REQUEST_DISCOVERABLE")?;
            let intent = env.construct(
                "android/content/Intent",
                "(Ljava/lang/String;)V",
                &[JValue::Object(&action)],
            )?;
            let extra = env.new_string("android.bluetooth.adapter.extra.DISCOVERABLE_DURATION")?;
            env.call(
                &intent,
                "putExtra",
                "(Ljava/lang/String;I)Landroid/content/Intent;",
                &[JValue::Object(&extra), JValue::Int(seconds as i32)],
            )?;
            env.call(
                activity,
                "startActivity",
                "(Landroid/content/Intent;)V",
                &[JValue::Object(&intent)],
            )
            .map(|_| ())
        });
    }
}
