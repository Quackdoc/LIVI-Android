// SPDX-FileCopyrightText: 2025-2026 Mitchel Stewart
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

pub mod layer;
pub mod surface_layer;

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use std::collections::VecDeque;

use ndk::hardware_buffer::HardwareBufferUsage;
use ndk::media::image_reader::{AcquireResult, Image, ImageFormat, ImageReader};
use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
    OutputBuffer,
};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;

use jni::objects::{JObjectArray, JString, JValue};

use self::layer::ARect;
use crate::android::env::{EnvExt, with_env};

const INPUT_QUEUE_CAPACITY: usize = 64;
const INPUT_IDLE_WAIT: Duration = Duration::from_millis(4);
const INPUT_BUFFER_WAIT: Duration = Duration::from_millis(20);
const RETRY: Duration = Duration::from_secs(2);
const STALL: Duration = Duration::from_secs(2);
const STALL_UNITS: u32 = 20;

static LAYER_UNUSABLE: AtomicBool = AtomicBool::new(false);

fn note_failure(sink: &Sink) {
    if matches!(sink, Sink::Layer(_)) {
        LAYER_UNUSABLE.store(true, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

impl Codec {
    fn mime(self) -> &'static str {
        match self {
            Self::H264 => "video/avc",
            Self::H265 => "video/hevc",
        }
    }
}

struct Unit {
    timestamp: u64,
    data: Vec<u8>,
}

pub struct VideoDecoder {
    tx: SyncSender<Unit>,
}

impl VideoDecoder {
    pub fn start(
        codec: Codec,
        width: u32,
        height: u32,
        on_open: impl Fn() + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::sync_channel(INPUT_QUEUE_CAPACITY);
        let spawned = thread::Builder::new()
            .name("livi-video".into())
            .spawn(move || run(codec, width as i32, height as i32, rx, on_open));
        if let Err(e) = spawned {
            eprintln!("[video] no decoder thread: {e}");
        }
        Self { tx }
    }

    pub fn push(&self, timestamp: u64, data: Vec<u8>) -> bool {
        self.tx.try_send(Unit { timestamp, data }).is_ok()
    }
}

// Where the video decodes to, either a layer, or direct to surface, android 13 is buggy.
enum Sink {
    Layer(ImageReader),
    Surface,
}

struct Open {
    held: std::cell::RefCell<VecDeque<Image>>,
    codec: MediaCodec,
    sink: Sink,
    failures: Cell<u32>,
    rendered: Cell<u32>,
}

const MAX_FAILURES: u32 = 8;

fn configured(
    kind: Codec,
    width: i32,
    height: i32,
    window: &NativeWindow,
    hints: bool,
) -> Result<MediaCodec, String> {
    let codec = MediaCodec::from_decoder_type(kind.mime())
        .ok_or_else(|| format!("no {} decoder", kind.mime()))?;
    let mut format = MediaFormat::new();
    format.set_str("mime", kind.mime());
    format.set_i32("width", width);
    format.set_i32("height", height);
    // Some decoders refuse a configuration with these, they only trim the delay.
    if hints {
        format.set_i32("low-latency", 1);
        format.set_i32("priority", 0);
    }
    codec
        .configure(&format, Some(window), MediaCodecDirection::Decoder)
        .map_err(|e| format!("configure: {e}"))?;
    codec.start().map_err(|e| format!("start: {e}"))?;
    println!(
        "[video] {} decoded by {}",
        kind.mime(),
        codec.name().unwrap_or_default()
    );
    Ok(codec)
}

/// `MediaCodecList.ALL_CODECS`.
const ALL_CODECS: i32 = 1;

/// Whether the platform's codec list says the decoder `name` is hardware: accelerated and not
/// software only. `None` when the list has no such decoder.
fn accelerated(vm: usize, name: &str) -> Result<Option<bool>, String> {
    with_env(vm, |env| {
        let list = env.construct(
            "android/media/MediaCodecList",
            "(I)V",
            &[JValue::Int(ALL_CODECS)],
        )?;
        let infos = env
            .call(
                &list,
                "getCodecInfos",
                "()[Landroid/media/MediaCodecInfo;",
                &[],
            )?
            .l()?;
        let infos = env.cast_local::<JObjectArray>(infos)?;
        for i in 0..infos.len(env)? {
            // The list has a hundred entries or more, each one gets its own frame of references.
            let found =
                env.with_local_frame(8, |env| -> Result<Option<bool>, jni::errors::Error> {
                    let info = infos.get_element(env, i)?;
                    let listed = env
                        .call(&info, "getName", "()Ljava/lang/String;", &[])?
                        .l()?;
                    let listed = env.cast_local::<JString>(listed)?.try_to_string(env)?;
                    if listed != name {
                        return Ok(None);
                    }
                    let accelerated = env.call(&info, "isHardwareAccelerated", "()Z", &[])?.z()?;
                    let software = env.call(&info, "isSoftwareOnly", "()Z", &[])?.z()?;
                    Ok(Some(accelerated && !software))
                })?;
            if found.is_some() {
                return Ok(found);
            }
        }
        Ok(None)
    })
}

/// we want tomake sure we only get avc when only software decoding is availible
pub fn hardware_decoder(vm: usize, kind: Codec) -> bool {
    let Some(name) = MediaCodec::from_decoder_type(kind.mime()).and_then(|c| c.name().ok()) else {
        println!("[video] no {} decoder", kind.mime());
        return false;
    };
    let hardware = match accelerated(vm, &name) {
        Ok(Some(hardware)) => hardware,
        Ok(None) => {
            println!("[video] {name} is not in the codec list");
            false
        }
        Err(e) => {
            println!("[video] the codec list: {e}");
            false
        }
    };
    println!(
        "[video] {} is decoded by {name}, {}",
        kind.mime(),
        if hardware { "hardware" } else { "software" }
    );
    hardware
}

const LAYER_IMAGES: i32 = 6;
const LAYER_HELD: usize = 2;

fn open_layer(kind: Codec, width: i32, height: i32) -> Result<Open, String> {
    let reader = ImageReader::new_with_usage(
        width,
        height,
        ImageFormat::PRIVATE,
        HardwareBufferUsage::GPU_SAMPLED_IMAGE | HardwareBufferUsage::COMPOSER_OVERLAY,
        LAYER_IMAGES,
    )
    .map_err(|e| format!("layer reader: {e}"))?;
    let window = reader.window().map_err(|e| format!("layer window: {e}"))?;
    let mut last = String::new();
    for hints in [true, false] {
        match configured(kind, width, height, &window, hints) {
            Ok(codec) => {
                return Ok(Open {
                    held: Default::default(),
                    codec,
                    sink: Sink::Layer(reader),
                    failures: Cell::new(0),
                    rendered: Cell::new(0),
                });
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn open_surface(kind: Codec, width: i32, height: i32) -> Result<Open, String> {
    let window = surface_layer::ensure((width, height)).ok_or("no surface layer")?;
    let mut last = String::new();
    for hints in [true, false] {
        match configured(kind, width, height, &window, hints) {
            Ok(codec) => {
                return Ok(Open {
                    held: Default::default(),
                    codec,
                    sink: Sink::Surface,
                    failures: Cell::new(0),
                    rendered: Cell::new(0),
                });
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn open(kind: Codec, width: i32, height: i32) -> Result<Open, String> {
    if !LAYER_UNUSABLE.load(Ordering::Relaxed) && layer::ensure() {
        match open_layer(kind, width, height) {
            Ok(open) => return Ok(open),
            Err(e) => {
                println!("[video] {e}, trying the layer made through Java");
                LAYER_UNUSABLE.store(true, Ordering::Relaxed);
            }
        }
    }
    open_surface(kind, width, height)
}

impl Open {
    fn feed(&self, unit: &Unit) -> Result<bool, String> {
        let DequeuedInputBufferResult::Buffer(mut input) = self
            .codec
            .dequeue_input_buffer(INPUT_BUFFER_WAIT)
            .map_err(|e| format!("input: {e}"))?
        else {
            return Ok(false);
        };
        let room = input.buffer_mut();
        if unit.data.len() > room.len() {
            return Err(format!(
                "a {} byte unit does not fit the {} byte input",
                unit.data.len(),
                room.len()
            ));
        }
        // SAFETY: the unit fits the input buffer and the two cannot overlap.
        unsafe {
            std::ptr::copy_nonoverlapping(
                unit.data.as_ptr(),
                room.as_mut_ptr().cast::<u8>(),
                unit.data.len(),
            );
        }
        self.codec
            .queue_input_buffer(input, 0, unit.data.len(), unit.timestamp, 0)
            .map_err(|e| format!("queue: {e}"))?;
        Ok(true)
    }

    fn drain(&self) -> Result<(), String> {
        loop {
            match self
                .codec
                .dequeue_output_buffer(Duration::ZERO)
                .map_err(|e| format!("output: {e}"))?
            {
                DequeuedOutputBufferInfoResult::Buffer(out) => {
                    self.give_back(out, true)?;
                    self.rendered.set(self.rendered.get() + 1);
                }
                DequeuedOutputBufferInfoResult::OutputFormatChanged => {
                    let f = self.codec.output_format();
                    println!(
                        "[video] output {}x{}",
                        f.i32("width").unwrap_or(0),
                        f.i32("height").unwrap_or(0)
                    );
                }
                DequeuedOutputBufferInfoResult::OutputBuffersChanged => {}
                DequeuedOutputBufferInfoResult::TryAgainLater => return Ok(()),
            }
        }
    }

    fn give_back(&self, out: OutputBuffer<'_>, render: bool) -> Result<(), String> {
        self.codec
            .release_output_buffer(out, render)
            .map_err(|e| format!("release: {e}"))
    }

    fn presented(&self) -> Result<bool, String> {
        let Sink::Layer(reader) = &self.sink else {
            return Ok(self.rendered.take() > 0);
        };
        self.rendered.set(0);
        match reader.acquire_latest_image() {
            Ok(AcquireResult::Image(image)) => {
                self.failures.set(0);
                Ok(self.show(image))
            }
            Ok(_) => Ok(false),
            Err(e) => {
                self.failures.set(self.failures.get() + 1);
                if self.failures.get() >= MAX_FAILURES {
                    return Err(format!("image reader: {e}"));
                }
                Ok(false)
            }
        }
    }
}

impl Open {
    fn show(&self, image: Image) -> bool {
        let Ok(buffer) = image.hardware_buffer() else {
            return false;
        };
        let desc = buffer.describe();
        let crop = match image.crop_rect() {
            Ok(r) if r.right > r.left && r.bottom > r.top => ARect {
                left: r.left,
                top: r.top,
                right: r.right,
                bottom: r.bottom,
            },
            _ => ARect {
                left: 0,
                top: 0,
                right: desc.width as i32,
                bottom: desc.height as i32,
            },
        };
        layer::present(buffer.as_ptr().cast(), crop);
        let mut held = self.held.borrow_mut();
        held.push_back(image);
        while held.len() > LAYER_HELD {
            held.pop_front();
        }
        true
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        match self.sink {
            Sink::Layer(_) => layer::clear(),
            Sink::Surface => surface_layer::clear(),
        }
    }
}

/// The ndk wrapper panics on a decoder that has failed, and a thread that panics stops the picture
/// for good. A failed decoder is started over like any other error.
fn guarded<T>(work: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
        .unwrap_or_else(|_| Err("the decoder failed".to_owned()))
}

/// Drops the decoder, which panics in the wrapper when the codec will not be deleted.
fn close(decoder: &mut Option<Open>) {
    let old = decoder.take();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(old)));
}

fn run(kind: Codec, width: i32, height: i32, units: mpsc::Receiver<Unit>, on_open: impl Fn()) {
    let mut decoder: Option<Open> = None;
    let mut need_key = true;
    let mut showed = false;
    let mut lost = 0u32;
    let mut shown = 0u64;
    let mut fed = 0u32;
    let mut last_picture = Instant::now();
    let mut retry_at = Instant::now();
    let mut stats = Stats::new();
    loop {
        match units.recv_timeout(INPUT_IDLE_WAIT) {
            Ok(unit) => {
                stats.units += 1;
                if decoder.is_none() {
                    if Instant::now() < retry_at {
                        continue;
                    }
                    match open(kind, width, height) {
                        Ok(d) => {
                            decoder = Some(d);
                            fed = 0;
                            need_key = true;
                            showed = false;
                            last_picture = Instant::now();
                            on_open();
                        }
                        Err(e) => {
                            eprintln!("[video] {e}");
                            retry_at = Instant::now() + RETRY;
                            continue;
                        }
                    }
                }
                let Some(d) = &decoder else { continue };
                if need_key {
                    if !is_key_frame(kind, &unit.data) {
                        continue;
                    }
                    need_key = false;
                }
                // Units offered since the last picture count, a stuck decoder accepts none. A phone
                // whose screen does not change sends nothing, and that is no stall.
                fed = fed.saturating_add(1);
                match guarded(|| d.feed(&unit)) {
                    Ok(true) => {}
                    Ok(false) => {
                        lost += 1;
                        stats.lost += 1;
                        if lost.is_power_of_two() {
                            eprintln!("[video] the decoder is behind, {lost} units lost");
                        }
                    }
                    Err(e) => {
                        eprintln!("[video] {e}, starting over");
                        close(&mut decoder);
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let Some(d) = &decoder else { continue };
        if fed >= STALL_UNITS && last_picture.elapsed() > STALL {
            eprintln!("[video] no picture from {fed} units, starting over");
            if !showed {
                note_failure(&d.sink);
            }
            close(&mut decoder);
            continue;
        }
        let began = Instant::now();
        let drained = guarded(|| d.drain().and_then(|()| d.presented()));
        match drained {
            Ok(true) => {
                last_picture = Instant::now();
                fed = 0;
                showed = true;
                stats.pictures += 1;
                stats.busy += began.elapsed();
                shown += 1;
                if shown == 1 || shown.is_multiple_of(300) {
                    println!("[video] picture {shown} in the layer");
                }
            }
            Ok(false) => {}
            Err(e) => {
                eprintln!("[video] {e}, starting over");
                note_failure(&d.sink);
                close(&mut decoder);
            }
        }
        stats.report();
    }
}

fn is_key_frame(kind: Codec, data: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let header = data[i + 3];
            let key = match kind {
                Codec::H264 => matches!(header & 0x1f, 5 | 7),
                Codec::H265 => matches!((header >> 1) & 0x3f, 16..=21 | 32),
            };
            if key {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

struct Stats {
    since: Instant,
    units: u32,
    pictures: u32,
    lost: u32,
    busy: Duration,
}

impl Stats {
    const REPORT_INTERVAL: Duration = Duration::from_secs(5);

    fn new() -> Self {
        Self {
            since: Instant::now(),
            units: 0,
            pictures: 0,
            lost: 0,
            busy: Duration::ZERO,
        }
    }

    fn report(&mut self) {
        if self.since.elapsed() < Self::REPORT_INTERVAL {
            return;
        }
        if self.units > 0 || self.pictures > 0 {
            let each = self.busy.as_secs_f64() * 1000.0 / f64::from(self.pictures.max(1));
            println!(
                "[video] {} units in, {} pictures out, {} lost, {each:.1} ms a picture",
                self.units, self.pictures, self.lost
            );
        }
        *self = Self::new();
    }
}
