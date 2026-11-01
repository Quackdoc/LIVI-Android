// SPDX-FileCopyrightText: 2025-2026 Mitchel Stewart
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use livi_host_proto::feed::{KIND_MIC, encode};
use ndk::audio::{
    AudioCallbackResult, AudioDirection, AudioFormat, AudioStream, AudioStreamBuilder,
};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

// SAFETY: the stream is created and started on one thread and afterwards only dropped, which can
// happen on another. The callback AAudio runs captures nothing but the channel sender.
struct Stream {
    _open: AudioStream,
}
unsafe impl Send for Stream {}

pub struct MicTap {
    // In a mutex only so `MicTap` is `Sync` without a second unsafe impl.
    _stream: Mutex<Stream>,
    writer: JoinHandle<()>,
}

impl Drop for MicTap {
    fn drop(&mut self) {
        self.writer.abort();
    }
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

// 16bit
pub fn open(path: &str, sample_rate: u32, channels: u32) -> Result<MicTap, String> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let width = channels as usize;
    let stream = AudioStreamBuilder::new()
        .map_err(|e| format!("mic builder: {e}"))?
        .direction(AudioDirection::Input)
        .format(AudioFormat::PCM_I16)
        .sample_rate(sample_rate as i32)
        .channel_count(channels as i32)
        .data_callback(Box::new(move |_, data, frames| {
            let bytes = frames.max(0) as usize * width * 2;
            // SAFETY: AAudio hands over `frames` frames of the 16-bit format asked for.
            let pcm = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), bytes) };
            let _ = tx.send(pcm.to_vec());
            AudioCallbackResult::Continue
        }))
        .open_stream()
        .map_err(|e| format!("mic stream: {e}"))?;
    stream
        .request_start()
        .map_err(|e| format!("mic start: {e}"))?;
    let path = path.to_owned();
    let writer = tokio::spawn(async move {
        let mut sock = match UnixStream::connect(&path).await {
            Ok(s) => s,
            Err(e) => return eprintln!("[mic] {path}: {e}"),
        };
        while let Some(pcm) = rx.recv().await {
            if sock
                .write_all(&encode(KIND_MIC, 0, now_ns(), &pcm))
                .await
                .is_err()
            {
                return;
            }
        }
    });
    Ok(MicTap {
        _stream: Mutex::new(Stream { _open: stream }),
        writer,
    })
}
