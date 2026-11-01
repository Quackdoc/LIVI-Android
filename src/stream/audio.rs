// SPDX-FileCopyrightText: 2026 Mitchel Stewart
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use livi_aa_stack::media::AudioKind;
use ndk::audio::{
    AudioCallbackResult, AudioDirection, AudioFormat, AudioStream, AudioStreamBuilder,
};
use ndk::audio::{AudioContentType, AudioUsage};

/// What waits in the queue at most, a longer backlog would delay the sound behind the picture.
const MAX_QUEUED_MS: usize = 250;

struct Shared {
    queue: Mutex<VecDeque<i16>>,
    gain: AtomicU32,
    limit: usize,
}

// SAFETY: the stream is created and started on one thread and afterwards only dropped, which can
// happen on another. The callback AAudio runs captures nothing but `Arc<Shared>`.
struct Stream {
    _open: AudioStream,
}
unsafe impl Send for Stream {}

pub struct AudioOut {
    shared: Arc<Shared>,
    _stream: Mutex<Stream>,
}

fn scale(sample: i16, gain: f32) -> i16 {
    if gain >= 0.999 {
        return sample;
    }
    (f32::from(sample) * gain) as i16
}

// What the stream is for, so Android can route it and duck other sound.
fn usage_of(kind: AudioKind) -> (AudioUsage, AudioContentType) {
    match kind {
        AudioKind::Media => (AudioUsage::Media, AudioContentType::Music),
        AudioKind::Speech => (AudioUsage::Assistant, AudioContentType::Speech),
        AudioKind::Call => (AudioUsage::VoiceCommunication, AudioContentType::Speech),
        AudioKind::Alert => (
            AudioUsage::AssistanceNavigationGuidance,
            AudioContentType::Speech,
        ),
    }
}

impl AudioOut {
    pub fn open(kind: AudioKind, sample_rate: u32, channels: u32) -> Result<Self, String> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::new()),
            gain: AtomicU32::new(1.0f32.to_bits()),
            limit: sample_rate as usize * channels as usize * MAX_QUEUED_MS / 1000,
        });
        let reader = shared.clone();
        let width = channels as usize;
        let builder = AudioStreamBuilder::new()
            .map_err(|e| format!("audio builder: {e}"))?
            .direction(AudioDirection::Output)
            .format(AudioFormat::PCM_I16)
            .sample_rate(sample_rate as i32)
            .channel_count(channels as i32);
        let (usage, content) = usage_of(kind);
        let builder = builder.usage(usage).content_type(content);
        let stream = builder
            .data_callback(Box::new(move |_, data, frames| {
                let wanted = frames.max(0) as usize * width;
                // SAFETY: AAudio hands over `frames` frames of the 16-bit format asked for.
                let out = unsafe { std::slice::from_raw_parts_mut(data.cast::<i16>(), wanted) };
                let gain = f32::from_bits(reader.gain.load(Ordering::Relaxed));
                let got = match reader.queue.try_lock() {
                    Ok(mut queue) => {
                        let take = wanted.min(queue.len());
                        for (slot, sample) in out.iter_mut().zip(queue.drain(..take)) {
                            *slot = scale(sample, gain);
                        }
                        take
                    }
                    Err(_) => 0,
                };
                out[got..].fill(0);
                AudioCallbackResult::Continue
            }))
            .open_stream()
            .map_err(|e| format!("audio stream: {e}"))?;
        stream
            .request_start()
            .map_err(|e| format!("audio start: {e}"))?;
        Ok(Self {
            shared,
            _stream: Mutex::new(Stream { _open: stream }),
        })
    }

    // 16bit audio
    pub fn push(&self, pcm: &[u8]) {
        let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.extend(
            pcm.chunks_exact(2)
                .map(|pair| i16::from_le_bytes([pair[0], pair[1]])),
        );
        if queue.len() > self.shared.limit {
            let extra = queue.len() - self.shared.limit;
            queue.drain(..extra);
        }
    }

    pub fn set_gain(&self, level: f32) {
        self.shared
            .gain
            .store(level.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
}
