# LIVI for Android

Based on LIVI by Lasse Heitgres (https://github.com/f-io/LIVI)

An Android port of LIVI's head unit side: an Android device shows and plays
what a phone projects over USB with Android Auto. 
It is a Rust and Slint app, there is no Kotlin or Java in it.

The port reuses the desktop code. The UI is `livi-ui`
the protocol is `livi-aa` and `livi-aa-stack`, and the settings are `livi-core-proto`. 

This is early work. Android Auto is the only mode currently. 
Carplay, general media, GPS and Hardware keys are not yet implemented.

## AI Use disclosure

AI was used for a large amount of porting livi's code specifics to android. 
Each line of code has been read by humans however.

I am not familiar with a lot of JNI stuff so I am relying heavily on AI for it.

## Requirements

Linux build host. Everything below can live outside your home directory, nothing needs root except creating the
directories.

Android NDK: r27c: C compiler for `ring` and Skia
Slint's Android backend compiles Java against API 34
Android build tools: 34.0.0
JDK: 11 or newer
`cargo-apk`: 0.10: `cargo install cargo-apk --locked`

## Build Setup
```sh
rustup target add aarch64-linux-android x86_64-linux-android

export ANDROID_HOME=/opt/android-sdk
export ANDROID_NDK_ROOT=/opt/android-ndk/android-ndk-r27c
export ANDROID_NDK_HOME=$ANDROID_NDK_ROOT
```

`sdkmanager "platforms;android-34" "platforms;android-33" "build-tools;34.0.0"`

### Signing key

The APK is signed with `debug.keystore`, which is git-ignored. Create it once:

```sh
cd android
keytool -genkey -v -keystore debug.keystore -storepass android -alias androiddebugkey -keypass android \
  -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=Android Debug,O=Android,C=US"
```

This is a debug key, fine for sideloading. A release that is distributed needs a key of its own, set under
`[package.metadata.android.signing.release]` in `Cargo.toml`.

## Build

`android/` is its own Cargo workspace, the desktop build in `native/` does not know about it.

```sh
cd android
cargo apk build --release --target aarch64-linux-android   # a phone or tablet
cargo apk build --release --target x86_64-linux-android    # an emulator, Waydroid
```

The APK is `android/target/release/apk/livi-android.apk`. Both targets write the same file name, so copy it away after
each build. A build without `--target` makes one APK with both libraries, `build_targets` in `Cargo.toml` lists them.

Release builds matter. A debug build runs very slowly and is prone to lockups.

```sh
adb install -r target/release/apk/livi-android.apk
```

## Using it

Start the app on the Android device and allow the microphone when asked, 
the voice assistant needs it. A phone can connect over a cable or wirelessly.

### With a cable

1. Connect the phone with a cable. The Android device has to be the USB host. 
2. The system asks for permission to use the phone. 
    After LIVI switches it into accessory mode the phone appears again 
    as a new device and the system asks a second time. Tick "use by default" to make both stick.
3. The phone starts Android Auto and the picture appears on the projection page.

### Wireless

1. Turn on wireless Android Auto in the app settings. 
2. The app asks for the various permissions that must be granted and enabled.
   - Bluetooth ("nearby devices"), 
   - nearby Wi-Fi devices
   - location permissions
2. Pair the phone with the Android device in the system's Bluetooth settings, once. 
    With no phone paired, the app asks the phone that connected last to pair again
    and shows the system's "make visible" prompt, which has to be accepted.
    Pairing must be confirmed on both devices.
3. With Auto Connect on, the app wakes the paired phones on its own, 
    the one that connected most recently first, and the phone starts Android Auto over Wi-Fi Direct.
    A session comes up in about 20 seconds, and a dropped link is picked up again in about 10.
    With Auto Connect off nothing is woken, the Devices page in the settings can still wake a phone.
4. The Devices page lists the phones that have connected, with their name and model.
    Delete device forgets a phone and tries to unpair it. 
    Android may refuse, then remove it in the Bluetooth settings.

A phone that hosts a Wi-Fi Direct group of its own at the same time can break the connection. Switching the phone's Wi-Fi off and on clears it. This includes hotspot and direct sharing

## Layout

```
src/lib.rs                      entry point, `android_main`
src/core.rs                     the core the UI talks to: state, settings, actions, the Android Auto stack
src/android/                    framework calls over JNI: `env.rs` plumbing, `java.rs` activity calls
src/stream/                     what a session plays: `media.rs` feed routing, `audio.rs`, `mic.rs`
src/stream/video/               the MediaCodec decoder (`mod.rs`) and the layer behind the window
                                (`layer.rs`, `surface_layer.rs`)
src/session/                    how a phone reaches the stack: `helper.rs` (in-process helper), `usb.rs` (wired)
src/wireless/                   the wireless handshake (`mod.rs`), `bt.rs`, `p2p.rs` (Wi-Fi Direct), `devices.rs`
```

## Status

- Wired Android auto
- Wireless Android auto

Not there:

- CarPlay, the instrument cluster
- GPS and the other car sensors, only night mode is sent
- hardware keys and knobs, only touch
- VP9 and AV1
- more than one phone at a time
- the panel size is not detected, set it under Settings → Video
- no launch when a phone is plugged in. `cargo-apk` cannot declare the USB intent filter that needs

## License

GPL-3.0-or-later, like LIVI. See [LICENSE](LICENSE) and [NOTICE](NOTICE), which carries LIVI's additional terms
(section 7(b)): the attribution at the top of this file has to stay. Third-party components are in
[CREDITS.md](CREDITS.md).
