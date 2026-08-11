use std::io::{BufRead, BufReader};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::{env, thread};

use crate::{WORKSPACES_MASK, pack_workspaces, ping_main_thread, unpack_workspaces};

pub fn start(wake_fd: OwnedFd) {
    let _ = thread::Builder::new().spawn(move || {
        println!("[Hyprland Thread] Started");

        let runtime_dir = env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR not set.");
        let his =
            env::var("HYPRLAND_INSTANCE_SIGNATURE").expect("HYPRLAND_INSTANCE_SIGNATURE not set.");

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

        let mut reader = BufReader::with_capacity(384, stream);
        let mut buf = Vec::with_capacity(128);

        while let Ok(n) = reader.read_until(b'\n', &mut buf)
            && n != 0
        {
            handle_event(&buf, &wake_fd);
            buf.clear();
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

fn handle_event(event: &[u8], wake_fd: &OwnedFd) {
    if let Some(ws) = event.strip_prefix(b"workspace>>").and_then(parse_ws) {
        set_active_workspace(ws);
    } else if let Some(ws) = event.strip_prefix(b"createworkspace>>").and_then(parse_ws) {
        set_workspace_occupied(ws);
    } else if let Some(ws) = event.strip_prefix(b"destroyworkspace>>").and_then(parse_ws) {
        set_workspace_empty(ws);
    } else {
        return;
    }
    ping_main_thread(wake_fd);
}

fn parse_ws(data: &[u8]) -> Option<u8> {
    match data {
        [b @ b'1'..=b'9', b'\n'] => Some(b - b'0'),
        [b'1', b'0', b'\n'] => Some(10),
        _ => None,
    }
}
