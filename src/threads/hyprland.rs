use std::io::{BufRead, BufReader};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::time::Duration;
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

        loop {
            match UnixStream::connect(&socket_path) {
                Ok(stream) => {
                    println!("[Hyprland Thread] Connected to IPC socket.");
                    let mut reader = BufReader::new(stream);
                    let mut line = String::with_capacity(128);

                    while reader.read_line(&mut line).unwrap_or(0) > 0 {
                        handle_event(&line, &wake_fd);
                        line.clear();
                    }
                    println!("[Hyprland Thread] Connection closed.");
                }
                Err(e) => {
                    eprintln!(
                        "[Hyprland Thread] Failed to connect to IPC socket: {}. Retrying in 2s...",
                        e
                    );
                    thread::sleep(Duration::from_secs(2));
                }
            }
        }
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

fn handle_event(event: &str, wake_fd: &OwnedFd) {
    if let Some((name, data)) = event.trim().split_once(">>")
        && let Ok(ws) = data.parse::<u8>()
    {
        match name {
            "workspace" => set_active_workspace(ws),
            "createworkspace" => set_workspace_occupied(ws),
            "destroyworkspace" => set_workspace_empty(ws),
            _ => return,
        };
        ping_main_thread(wake_fd);
    }
}
