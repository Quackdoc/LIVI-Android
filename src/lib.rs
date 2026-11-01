// SPDX-FileCopyrightText: 2025-2026 Mitchel Stewart
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

mod android;
mod core;
mod session;
mod stream;
mod wireless;

use slint::android::android_activity::WindowManagerFlags;

#[unsafe(no_mangle)]
fn android_main(app: slint::android::AndroidApp) {
    // A head unit's screen stays on and has no status bar over the picture.
    app.set_window_flags(
        WindowManagerFlags::KEEP_SCREEN_ON | WindowManagerFlags::FULLSCREEN,
        WindowManagerFlags::empty(),
    );
    android::java::Java::new(app.vm_as_ptr() as usize, app.activity_as_ptr() as usize)
        .hide_system_bars();
    let dir = app.internal_data_path().expect("no internal data path");
    // The session sockets live in the app's own directory, Android has no /tmp.
    let tmp = dir.join("tmp");
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        eprintln!("[livi] no {}: {e}", tmp.display());
    }
    // Sockets of an earlier run are still there, binding over one fails.
    if let Ok(entries) = std::fs::read_dir(&tmp) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with("aa-") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    // SAFETY: nothing else runs yet.
    unsafe { std::env::set_var("TMPDIR", &tmp) };
    let platform = core::Platform {
        dir: tmp.clone(),
        vm: app.vm_as_ptr() as usize,
        activity: app.activity_as_ptr() as usize,
    };
    let stream = match core::start(platform) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[livi] the core did not start: {e}");
            return;
        }
    };
    stream::video::layer::install(app.clone());
    slint::android::init(app).expect("cannot start the Android backend");
    if let Err(e) = livi_ui::run(move || Ok(stream)) {
        eprintln!("[livi] {e}");
    }
}
