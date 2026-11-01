// SPDX-FileCopyrightText: 2025-2026 Mitchel Stewart
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! What `livi-gst-host` does on the desktop: takes the video and audio an Android Auto session
//! writes to its feed socket and plays it. Video goes through MediaCodec to the UI, audio through
//! AAudio.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use livi_aa_stack::discovery::VideoCodec;
use livi_aa_stack::driver::SessionCmd;
use livi_aa_stack::manager::{AaHandle, SessionId};
use livi_aa_stack::media::{AaMedia, AudioKind, AudioOutput};
use livi_host_proto::feed::{Framer, KIND_AUDIO, KIND_VIDEO, KIND_VIDEO_START, Record};
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;

use crate::android::java::Java;
use crate::stream::audio::AudioOut;
use crate::stream::mic::{self, MicTap};
use crate::stream::video::{Codec, VideoDecoder};

/// The picture size to assume before the phone says which one it sends.
const FALLBACK_TIER: (u32, u32) = (1280, 720);

#[derive(Clone, Copy)]
enum Route {
    Video { session: SessionId, cluster: bool },
    Audio { session: SessionId, stream: u32 },
}

type OutputKey = (AudioKind, u32, u32, String);

#[derive(Default)]
struct Inner {
    next_feed: u32,
    next_stream: u32,
    routes: HashMap<u32, Route>,
    video_ids: HashMap<(SessionId, bool), u32>,
    codecs: HashMap<u32, Codec>,
    decoders: HashMap<u32, VideoDecoder>,
    outputs: Vec<AudioOutput>,
    keys: HashMap<u32, OutputKey>,
    audio: HashMap<u32, Arc<AudioOut>>,
    levels: HashMap<AudioKind, f32>,
    front: Option<SessionId>,
    tier: Option<(u32, u32)>,
    aa: Option<AaHandle>,
    java: Option<Java>,
    mics: HashMap<u32, MicTap>,
    next_mic: u32,
    keyframe_asked: Option<Instant>,
    dropped: u64,
}

/// How often to ask a phone for a new key frame after losing pictures.
const KEYFRAME_EVERY: Duration = Duration::from_secs(1);

pub struct AndroidMedia {
    feed_path: PathBuf,
    inner: Mutex<Inner>,
    opened: broadcast::Sender<AudioOutput>,
}

fn codec_of(kind: u8) -> Option<Codec> {
    match kind {
        0 => Some(Codec::H264),
        1 => Some(Codec::H265),
        _ => None,
    }
}

impl AndroidMedia {
    pub fn new(feed_path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            feed_path,
            inner: Mutex::new(Inner::default()),
            opened: broadcast::channel(16).0,
        })
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_handle(&self, aa: AaHandle) {
        self.inner().aa = Some(aa);
    }

    pub fn set_java(&self, java: Java) {
        self.inner().java = Some(java);
    }

    pub async fn serve_feed(self: Arc<Self>) -> std::io::Result<()> {
        let _ = std::fs::remove_file(&self.feed_path);
        let listener = UnixListener::bind(&self.feed_path)?;
        loop {
            let (stream, _) = listener.accept().await?;
            let media = self.clone();
            tokio::spawn(async move { media.read_feed(stream).await });
        }
    }

    async fn read_feed(self: Arc<Self>, mut stream: UnixStream) {
        let mut framer = Framer::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => framer.push(&buf[..n]),
            }
            while let Some(record) = framer.next_record() {
                self.on_record(record);
            }
        }
    }

    fn on_record(&self, record: Record) {
        let mut inner = self.inner();
        match record.kind {
            KIND_VIDEO_START => {
                if let Some(codec) = record.payload.first().and_then(|byte| codec_of(*byte)) {
                    inner.codecs.insert(record.id, codec);
                }
            }
            KIND_VIDEO => {
                let Some(&Route::Video { session, cluster }) = inner.routes.get(&record.id) else {
                    return;
                };
                // Only the main picture is shown, and only for the session in front.
                if cluster || inner.front != Some(session) {
                    return;
                }
                if !inner.decoders.contains_key(&record.id) {
                    let Some(codec) = inner.codecs.get(&record.id).copied() else {
                        return;
                    };
                    let (width, height) = inner.tier.unwrap_or(FALLBACK_TIER);
                    println!(
                        "[media] {codec:?} decoder for feed {} at {width}x{height}",
                        record.id
                    );
                    let aa = inner.aa.clone();
                    let decoder = VideoDecoder::start(codec, width, height, move || {
                        if let Some(aa) = &aa {
                            aa.session(session, SessionCmd::Keyframe);
                        }
                    });
                    inner.decoders.insert(record.id, decoder);
                }
                let kept = inner
                    .decoders
                    .get(&record.id)
                    .is_some_and(|decoder| decoder.push(record.ts, record.payload));
                if !kept {
                    inner.dropped += 1;
                    if inner.dropped.is_power_of_two() {
                        eprintln!("[media] {} video units lost so far", inner.dropped);
                    }
                    // Every later picture builds on a lost one until the next key frame.
                    let due = inner
                        .keyframe_asked
                        .is_none_or(|t| t.elapsed() >= KEYFRAME_EVERY);
                    if due && let Some(aa) = inner.aa.clone() {
                        inner.keyframe_asked = Some(Instant::now());
                        aa.session(session, SessionCmd::Keyframe);
                    }
                }
            }
            KIND_AUDIO => {
                let Some(Route::Audio { session, stream }) = inner.routes.get(&record.id) else {
                    return;
                };
                if inner.front != Some(*session) {
                    return;
                }
                if let Some(out) = inner.audio.get(stream) {
                    out.push(&record.payload);
                }
            }
            _ => {}
        }
    }

    fn alloc_feed(inner: &mut Inner) -> u32 {
        inner.next_feed += 1;
        inner.next_feed
    }
}

impl AaMedia for AndroidMedia {
    async fn feed_path(&self) -> String {
        self.feed_path.display().to_string()
    }

    fn video_feed(&self, session: SessionId, cluster: bool) -> u32 {
        let mut inner = self.inner();
        if let Some(id) = inner.video_ids.get(&(session, cluster)) {
            return *id;
        }
        let id = Self::alloc_feed(&mut inner);
        inner.routes.insert(id, Route::Video { session, cluster });
        inner.video_ids.insert((session, cluster), id);
        id
    }

    fn prime_video(&self, session: SessionId, cluster: bool, codec: VideoCodec) {
        let mut inner = self.inner();
        let Some(id) = inner.video_ids.get(&(session, cluster)).copied() else {
            return;
        };
        match codec {
            VideoCodec::H264 => inner.codecs.insert(id, Codec::H264),
            VideoCodec::H265 => inner.codecs.insert(id, Codec::H265),
            other => {
                eprintln!("[media] no {} decoder on Android yet", other.name());
                None
            }
        };
    }

    fn video_started(&self, cluster: bool, width: u32, height: u32) {
        if !cluster && width > 0 && height > 0 {
            self.inner().tier = Some((width, height));
        }
    }

    fn audio_feed(&self, session: SessionId, stream: u32) -> u32 {
        let mut inner = self.inner();
        let id = Self::alloc_feed(&mut inner);
        inner.routes.insert(id, Route::Audio { session, stream });
        id
    }

    fn set_active(&self, session: SessionId, active: bool) {
        let mut inner = self.inner();
        if active {
            inner.front = Some(session);
            return;
        }
        if inner.front != Some(session) {
            return;
        }
        inner.front = None;
        // The decoders go with the picture, a later activation starts them over on a key frame.
        inner.decoders.clear();
        drop(inner);
    }

    fn release(&self, session: SessionId) {
        let mut inner = self.inner();
        let gone: Vec<u32> = inner
            .routes
            .iter()
            .filter(|(_, route)| match route {
                Route::Video { session: owner, .. } | Route::Audio { session: owner, .. } => {
                    *owner == session
                }
            })
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            inner.routes.remove(&id);
            inner.codecs.remove(&id);
            inner.decoders.remove(&id);
        }
        inner.video_ids.retain(|(owner, _), _| *owner != session);
        if inner.front == Some(session) {
            inner.front = None;
            drop(inner);
        }
    }

    fn audio_outputs(&self) -> Vec<AudioOutput> {
        self.inner().outputs.clone()
    }

    fn audio_output_opened(&self) -> broadcast::Receiver<AudioOutput> {
        self.opened.subscribe()
    }

    fn prime_audio(&self, kind: AudioKind, sample_rate: u32, channels: u32, tag: &str) {
        if sample_rate == 0 || channels == 0 {
            return;
        }
        let key: OutputKey = (kind, sample_rate, channels, tag.to_owned());
        let mut inner = self.inner();
        if inner.keys.values().any(|k| *k == key) {
            return;
        }
        let out = match AudioOut::open(kind, sample_rate, channels) {
            Ok(out) => Arc::new(out),
            Err(e) => {
                eprintln!("[media] {e}");
                return;
            }
        };
        if let Some(level) = inner.levels.get(&kind) {
            out.set_gain(*level);
        }
        inner.next_stream += 1;
        let stream = inner.next_stream;
        let output = AudioOutput {
            kind,
            stream,
            tag: Some(tag.to_owned()),
        };
        inner.keys.insert(stream, key);
        inner.audio.insert(stream, out);
        inner.outputs.push(output.clone());
        drop(inner);
        let _ = self.opened.send(output);
    }

    fn set_host_volume(&self, kind: AudioKind, level: f64, _ramp_ms: u32) {
        let mut inner = self.inner();
        inner.levels.insert(kind, level as f32);
        for output in inner.outputs.iter().filter(|output| output.kind == kind) {
            if let Some(out) = inner.audio.get(&output.stream) {
                out.set_gain(level as f32);
            }
        }
    }

    fn open_mic_tap(
        &self,
        path: &str,
        sample_rate: u32,
        channels: u32,
        _device: &str,
    ) -> Option<u32> {
        let java = self.inner().java;
        if let Some(java) = java
            && !java.has_microphone_permission()
        {
            eprintln!("[media] the microphone is not allowed yet, asking");
            java.ask_for_microphone();
            return None;
        }
        match mic::open(path, sample_rate, channels) {
            Ok(tap) => {
                let mut inner = self.inner();
                inner.next_mic += 1;
                let id = inner.next_mic;
                inner.mics.insert(id, tap);
                Some(id)
            }
            Err(e) => {
                eprintln!("[media] {e}");
                None
            }
        }
    }

    fn close_mic_tap(&self, id: u32) {
        self.inner().mics.remove(&id);
    }
}

pub fn feed_path_in(dir: &Path) -> PathBuf {
    dir.join("gst.feed")
}
