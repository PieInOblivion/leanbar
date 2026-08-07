use std::io::{BufRead, BufReader};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::{env, thread};

use crate::{WORKSPACES_MASK, pack_workspaces, ping_main_thread, unpack_workspaces};

pub fn start(wake_fd: OwnedFd) {
    let _ = thread::Builder::new().spawn(move || {
        println!("[Hyprland Thread] Started");

        // 1. Initialize current workspaces using `hyprctl`
        init_workspaces();
        ping_main_thread(&wake_fd);

        // 2. Connect to the event socket
        let his =
            env::var("HYPRLAND_INSTANCE_SIGNATURE").expect("HYPRLAND_INSTANCE_SIGNATURE not set.");
        let runtime_dir = env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR not set.");
        let socket_path = PathBuf::from(runtime_dir)
            .join("hypr")
            .join(his)
            .join(".socket2.sock");

        let stream = match UnixStream::connect(&socket_path) {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("[Hyprland Thread] Failed to connect to IPC socket: {}", e);
                return;
            }
        };

        println!("[Hyprland Thread] Connected to IPC socket.");

        // if the OS splits a line across two reads, we'll drop the first chunk
        // and miss that event. unlikely in practice since our events are ~40 bytes
        // and the buffer is 512, but not impossible. fix would be a small stack
        // accumulator to hold partial lines between fills.
        let mut reader = BufReader::with_capacity(512, stream);

        while let Ok(buf) = reader.fill_buf() {
            if buf.is_empty() {
                break;
            }
            if let Some(i) = buf.iter().position(|&b| b == b'\n') {
                handle_event(&buf[..i], &wake_fd);
                reader.consume(i + 1);
            } else {
                let len = buf.len();
                reader.consume(len);
            }
        }

        println!("[Hyprland Thread] Connection closed.");
    });
}

fn set_active_workspace(ws: u8) {
    if (1..=10).contains(&ws) {
        let (_, occupied) = unpack_workspaces(WORKSPACES_MASK.load(Ordering::Relaxed));
        let new_occupied = occupied | (1 << (ws - 1));
        WORKSPACES_MASK.store(pack_workspaces(ws, new_occupied), Ordering::Relaxed);
    }
}

fn set_workspace_occupied(ws: u8) {
    if (1..=10).contains(&ws) {
        let (active, occupied) = unpack_workspaces(WORKSPACES_MASK.load(Ordering::Relaxed));
        let new_occupied = occupied | (1 << (ws - 1));
        WORKSPACES_MASK.store(pack_workspaces(active, new_occupied), Ordering::Relaxed);
    }
}

fn set_workspace_empty(ws: u8) {
    if (1..=10).contains(&ws) {
        let (active, occupied) = unpack_workspaces(WORKSPACES_MASK.load(Ordering::Relaxed));
        let new_occupied = occupied & !(1 << (ws - 1));
        WORKSPACES_MASK.store(pack_workspaces(active, new_occupied), Ordering::Relaxed);
    }
}

fn init_workspaces() {
    // hyprctl activeworkspace
    if let Ok(output) = Command::new("hyprctl").arg("activeworkspace").output() {
        let out_str = String::from_utf8_lossy(&output.stdout);
        if let Some((_, remainder)) = out_str.split_once("workspace ID ") {
            let ws_str = remainder.split_whitespace().next().unwrap_or("");
            if let Ok(ws) = ws_str.parse::<u8>() {
                set_active_workspace(ws);
            }
        }
    }

    // hyprctl workspaces
    if let Ok(output) = Command::new("hyprctl").arg("workspaces").output() {
        let out_str = String::from_utf8_lossy(&output.stdout);
        for line in out_str.lines() {
            if let Some(remainder) = line.strip_prefix("workspace ID ") {
                let ws_str = remainder.split_whitespace().next().unwrap_or("");
                if let Ok(ws) = ws_str.parse::<u8>() {
                    set_workspace_occupied(ws);
                }
            }
        }
    }
}

fn handle_event(event: &[u8], wake_fd: &OwnedFd) {
    if let Some(pos) = event.windows(2).position(|w| w == b">>") {
        let (name, data) = (&event[..pos], &event[pos + 2..]);
        if let Some(ws) = parse_ws(data) {
            match name {
                b"workspace" => set_active_workspace(ws),
                b"createworkspace" => set_workspace_occupied(ws),
                b"destroyworkspace" => set_workspace_empty(ws),
                _ => return,
            };
            ping_main_thread(wake_fd);
        }
    }
}

fn parse_ws(data: &[u8]) -> Option<u8> {
    match data {
        [b @ b'1'..=b'9'] => Some(b - b'0'),
        [b'1', b'0'] => Some(10),
        _ => None,
    }
}
