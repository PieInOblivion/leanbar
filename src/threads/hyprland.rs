use std::env;
use std::io::{BufRead, BufReader};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::thread;

use crate::{ACTIVE_WORKSPACE, WORKSPACES, ping_main_thread};

pub fn start(wake_fd: OwnedFd) {
    let _ = thread::Builder::new()
        .stack_size(128 * 1024)
        .spawn(move || {
            println!("[Hyprland Thread] Started");

        // 1. Initialize current workspaces using `hyprctl`
        init_workspaces();
        ping_main_thread(&wake_fd);

        // 2. Connect to the event socket
        let his =
            env::var("HYPRLAND_INSTANCE_SIGNATURE").expect("HYPRLAND_INSTANCE_SIGNATURE not set.");
        let runtime_dir = env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR not set.");
        let socket_path = format!("{}/hypr/{}/.socket2.sock", runtime_dir, his);

        loop {
            match UnixStream::connect(&socket_path) {
                Ok(stream) => {
                    println!("[Hyprland Thread] Connected to IPC socket.");
                    let reader = BufReader::new(stream);

                    for line in reader.lines() {
                        match line {
                            Ok(event) => handle_event(&event, &wake_fd),
                            Err(e) => {
                                eprintln!("[Hyprland Thread] Socket read error: {}", e);
                                break; // Break and reconnect
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!(
                        "[Hyprland Thread] Failed to connect to IPC socket: {}. Retrying in 2s...",
                        e
                    );
                    thread::sleep(std::time::Duration::from_secs(2));
                }
            }
        }
    });
}

fn init_workspaces() {
    // hyprctl activeworkspace
    if let Ok(output) = Command::new("hyprctl").arg("activeworkspace").output() {
        let out_str = String::from_utf8_lossy(&output.stdout);
        // Look for "workspace ID " then parse the next token
        if let Some(ws_idx) = out_str.find("workspace ID ") {
            let remainder = &out_str[ws_idx + 13..];
            let ws_str = remainder.split_whitespace().next().unwrap_or("");
            if let Ok(ws) = ws_str.parse::<u8>() {
                ACTIVE_WORKSPACE.store(ws, Ordering::Release);
                if ws > 0 && ws <= 10 {
                    WORKSPACES[(ws - 1) as usize].store(true, Ordering::Release);
                }
            }
        }
    }

    // hyprctl workspaces
    if let Ok(output) = Command::new("hyprctl").arg("workspaces").output() {
        let out_str = String::from_utf8_lossy(&output.stdout);
        for line in out_str.lines() {
            if let Some(remainder) = line.strip_prefix("workspace ID ") {
                let ws_str = remainder.split_whitespace().next().unwrap_or("");
                if let Ok(ws) = ws_str.parse::<u8>()
                    && ws > 0
                    && ws <= 10
                {
                    WORKSPACES[(ws - 1) as usize].store(true, Ordering::Release);
                }
            }
        }
    }
}

fn handle_event(event: &str, wake_fd: &OwnedFd) {
    // Some Hyprland events have trailing newlines or whitespace depending on the reader
    let event = event.trim();

    if let Some(ws_str) = event.strip_prefix("workspace>>") {
        if let Ok(ws) = ws_str.parse::<u8>() {
            ACTIVE_WORKSPACE.store(ws, Ordering::Release);
            if ws > 0 && ws <= 10 {
                WORKSPACES[(ws - 1) as usize].store(true, Ordering::Release);
            }
            ping_main_thread(wake_fd);
        }
    } else if let Some(ws_str) = event.strip_prefix("createworkspace>>") {
        if let Ok(ws) = ws_str.parse::<u8>()
            && ws > 0
            && ws <= 10
        {
            WORKSPACES[(ws - 1) as usize].store(true, Ordering::Release);
            ping_main_thread(wake_fd);
        }
    } else if let Some(ws_str) = event.strip_prefix("destroyworkspace>>")
        && let Ok(ws) = ws_str.parse::<u8>()
        && ws > 0
        && ws <= 10
    {
        WORKSPACES[(ws - 1) as usize].store(false, Ordering::Release);
        ping_main_thread(wake_fd);
    }
}
