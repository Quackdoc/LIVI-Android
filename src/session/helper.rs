// SPDX-FileCopyrightText: 2025-2026 Mitchel Stewart
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! The part of `livi-helperd` that Android Auto needs, in the app's own process. The stack asks
//! over a line socket and learns of new sessions from the events it subscribes to.

use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use livi_aa::usb::Control;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, mpsc};

/// Hands one line of JSON to everybody subscribed.
#[derive(Clone, Default)]
pub struct Broadcaster {
    subs: Arc<Mutex<Vec<mpsc::UnboundedSender<String>>>>,
    joined: Arc<Notify>,
}

impl Broadcaster {
    pub fn push_json(&self, line: String) {
        self.subs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|tx| tx.send(line.clone()).is_ok());
    }

    fn subscribe(&self) -> mpsc::UnboundedReceiver<String> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subs.lock().unwrap_or_else(|e| e.into_inner()).push(tx);
        self.joined.notify_waiters();
        rx
    }

    /// Waits until somebody listens, a session announced earlier would be lost.
    pub async fn subscribed(&self) {
        loop {
            let joined = self.joined.notified();
            if !self
                .subs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
            {
                return;
            }
            joined.await;
        }
    }
}

#[derive(Clone)]
pub struct Helper {
    events: Broadcaster,
    control: Control,
    sock: PathBuf,
}

async fn handle(mut stream: UnixStream, helper: Helper) -> io::Result<()> {
    let mut line = String::new();
    BufReader::new(&mut stream).read_line(&mut line).await?;
    let line = line.trim();
    let (verb, arg) = line.split_once(' ').unwrap_or((line, ""));
    if verb == "subscribe" {
        let mut rx = helper.events.subscribe();
        let mut probe = [0u8; 64];
        loop {
            tokio::select! {
                event = rx.recv() => {
                    let Some(event) = event else { return Ok(()) };
                    stream.write_all(event.as_bytes()).await?;
                    stream.write_all(b"\n").await?;
                }
                read = tokio::io::AsyncReadExt::read(&mut stream, &mut probe) => {
                    if read? == 0 {
                        return Ok(());
                    }
                }
            }
        }
    }
    let answer = match verb {
        // Nothing about a phone's Bluetooth link or the call audio exists here yet.
        "wired-phones" | "sco-sink" => "{\"ok\":true}".to_owned(),
        "restart-usb" => {
            let n = helper.control.restart(arg.trim());
            format!("{{\"ok\":true,\"count\":{n}}}")
        }
        other => format!(
            "{{\"ok\":false,\"error\":\"unknown command: {}\"}}",
            other.replace(['"', '\n', '\r'], " ")
        ),
    };
    stream.write_all(answer.as_bytes()).await?;
    stream.write_all(b"\n").await
}

impl Helper {
    pub fn new(sock: PathBuf) -> Self {
        Self {
            events: Broadcaster::default(),
            control: Control::default(),
            sock,
        }
    }

    pub fn events(&self) -> Broadcaster {
        self.events.clone()
    }

    pub fn sock_path(&self) -> &Path {
        &self.sock
    }

    pub async fn serve(self) -> io::Result<()> {
        let _ = std::fs::remove_file(&self.sock);
        let listener = UnixListener::bind(&self.sock)?;
        std::fs::set_permissions(&self.sock, std::fs::Permissions::from_mode(0o600))?;
        loop {
            let (stream, _) = listener.accept().await?;
            let helper = self.clone();
            tokio::spawn(async move {
                if let Err(e) = handle(stream, helper).await {
                    eprintln!("[helper] connection: {e}");
                }
            });
        }
    }

    /// Asks a phone that is not yet in accessory mode to switch, it comes back as another device.
    pub async fn switch(&self, fd: OwnedFd) -> Result<(), String> {
        livi_aa::usb::switch_fd(fd).await
    }

    /// Runs a session with a phone in accessory mode until it ends or `cancel` fires.
    pub async fn serve_phone(
        &self,
        fd: OwnedFd,
        serial: String,
        cancel: Arc<Notify>,
    ) -> Result<(), String> {
        self.events.subscribed().await;
        let events = self.events.clone();
        livi_aa::usb::serve_fd(
			&self.control,
			fd,
			serial,
			move |socket, peer, serial| {
				events.push_json(format!(
					"{{\"event\":\"aa-session\",\"socket\":\"{socket}\",\"peer\":\"{peer}\",\"transport\":\"usb\",\"serial\":\"{serial}\"}}"
				));
			},
			cancel,
		)
		.await
    }
}
