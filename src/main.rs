use rustix::event::{EventfdFlags, PollFd, PollFlags, eventfd, poll};
use rustix::io::{read, write};
use std::fs;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use wayland_client::Connection;

mod app_state;
mod error;
mod font;
mod threads;

use app_state::AppState;
use error::LeanbarError;

// Colors are 0xAARRGGBB
pub const COLOR_WS_FOCUSED: u32 = 0xffffffff;
pub const COLOR_WS_OPEN: u32 = 0xffcba6f7;
pub const COLOR_TIME: u32 = 0xffcba6f7;
pub const COLOR_DATE: u32 = 0xff74c7ec;
pub const COLOR_BAT: u32 = 0xffa6e3a1;

pub static WORKSPACES_MASK: AtomicU16 = AtomicU16::new((1 << 10) | 1); // bits 13..10: active_ws, bits 9..0: occupied mask; default ws1 active+occupied
pub static TIME_MASK: AtomicU16 = AtomicU16::new(0); // hours, minutes
pub static DATE_MASK: AtomicU32 = AtomicU32::new(0); // day, month, year
pub static BATTERY_MASK: AtomicU32 = AtomicU32::new(0); // percent, state, estimate_m (0 = No Battery; state: 0: Unknown, 1: Discharging, 2: Charging, 3: Full)

pub fn ping_main_thread(fd: &OwnedFd) {
    let _ = write(fd, &1u64.to_ne_bytes());
}

fn main() -> Result<(), LeanbarError> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--build-font-atlas" {
            font::builder::run_builder_mode(&mut args)?;
            return Ok(());
        }
    }

    println!("Starting leanbar...");

    let font_path = "/usr/share/fonts/noto/NotoSans-Regular.ttf";
    let glyph_cache = font::GlyphCache::load_or_build(font_path, 15.0)?;

    // Check if battery exists on startup
    if fs::metadata("/sys/class/power_supply/BAT0/capacity").is_ok() {
        BATTERY_MASK.store(1, Ordering::Relaxed);
    }

    let conn = Connection::connect_to_env()?;
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();

    let display = conn.display();
    let _registry = display.get_registry(&qh, ());

    let mut state = AppState::new(glyph_cache);

    event_queue.roundtrip(&mut state)?;
    if !state.has_required_globals() {
        eprintln!("Failed to bind essential Wayland globals.");
        return Ok(());
    }

    state.initialize_layer_surface(&qh)?;
    event_queue.roundtrip(&mut state)?;

    let wake_fd = eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?;

    threads::linux_poll::start(wake_fd.try_clone()?);
    threads::hyprland::start(wake_fd.try_clone()?);

    println!("[Main Thread] Entering event loop");

    let backend = conn.backend();
    let wayland_fd = backend.poll_fd();
    let mut poll_fds = [
        PollFd::new(&wake_fd, PollFlags::IN),
        PollFd::new(&wayland_fd, PollFlags::IN),
    ];
    let mut buf = [0u8; 8];

    loop {
        let _ = conn.flush();

        match poll(&mut poll_fds[..], None) {
            Ok(_) => {
                if poll_fds[0].revents().contains(PollFlags::IN) {
                    let _ = read(&wake_fd, &mut buf);
                    state.redraw_and_commit();
                }

                if poll_fds[1].revents().contains(PollFlags::IN) {
                    if let Some(guard) = conn.prepare_read()
                        && let Err(e) = guard.read()
                    {
                        eprintln!("Wayland read error: {}", e);
                    }
                    if let Err(e) = event_queue.dispatch_pending(&mut state) {
                        eprintln!("Wayland dispatch error: {}", e);
                    }
                }
            }
            Err(e) => {
                eprintln!("Poll error: {}", e);
            }
        }
    }
}

pub fn pack_workspaces(active_ws: u8, occupied_mask: u16) -> u16 {
    ((active_ws as u16) << 10) | (occupied_mask & 0x03FF)
}

// (active_ws, occupied_mask)
pub fn unpack_workspaces(mask: u16) -> (u8, u16) {
    (((mask >> 10) & 0x0F) as u8, mask & 0x03FF)
}

pub fn pack_time(hours: u8, minutes: u8) -> u16 {
    ((hours as u16) << 8) | (minutes as u16)
}

// (hours, minutes)
pub fn unpack_time(mask: u16) -> (u8, u8) {
    (((mask >> 8) & 0xFF) as u8, (mask & 0xFF) as u8)
}

pub fn pack_date(day: u8, month: u8, year: u8) -> u32 {
    ((day as u32) << 16) | ((month as u32) << 8) | (year as u32)
}

// (day, month, year)
pub fn unpack_date(mask: u32) -> (u8, u8, u8) {
    (
        ((mask >> 16) & 0xFF) as u8,
        ((mask >> 8) & 0xFF) as u8,
        (mask & 0xFF) as u8,
    )
}

pub fn pack_battery(percent: u8, state: u8, estimate_m: u16) -> u32 {
    ((percent as u32) << 24) | ((state as u32) << 16) | (estimate_m as u32)
}

// (percent, state, estimate_m)
pub fn unpack_battery(mask: u32) -> (u8, u8, u16) {
    (
        ((mask >> 24) & 0xFF) as u8,
        ((mask >> 16) & 0xFF) as u8,
        (mask & 0xFFFF) as u16,
    )
}
