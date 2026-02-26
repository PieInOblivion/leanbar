use rustix::event::{EventfdFlags, PollFd, PollFlags, eventfd, poll};
use rustix::io::{read, write};
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU8};

use wayland_client::Connection;

mod app_state;
mod font_renderer;
mod threads;

use app_state::AppState;

pub static WORKSPACES: [AtomicBool; 10] = [
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
];
pub static ACTIVE_WORKSPACE: AtomicU8 = AtomicU8::new(1);

pub static TIME_HOURS: AtomicU8 = AtomicU8::new(0);
pub static TIME_MINUTES: AtomicU8 = AtomicU8::new(0);
pub static DATE_DAY: AtomicU8 = AtomicU8::new(0);
pub static DATE_MONTH: AtomicU8 = AtomicU8::new(0);
pub static DATE_YEAR: AtomicU8 = AtomicU8::new(0);
pub static BATTERY_PERCENT: AtomicU8 = AtomicU8::new(100);

pub fn ping_main_thread(fd: &OwnedFd) {
    let _ = write(fd, &1u64.to_ne_bytes());
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if font_renderer::maybe_run_builder_mode(&args)? {
        return Ok(());
    }

    println!("Starting leanbar...");

    let font_path = "/usr/share/fonts/TTF/SauceCodeProNerdFont-Regular.ttf";
    let glyph_cache = font_renderer::GlyphCache::load_or_build(font_path, 16.0).ok();
    if glyph_cache.is_none() {
        eprintln!("Failed to load font. Make sure the path is correct.");
    }

    let conn = Connection::connect_to_env()?;
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();

    let display = conn.display();
    let _registry = display.get_registry(&qh, ());

    let mut state = AppState::new(glyph_cache);

    println!("Discovering Wayland globals...");
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
    let mut poll_fds = vec![
        PollFd::new(&wake_fd, PollFlags::IN),
        PollFd::new(&wayland_fd, PollFlags::IN),
    ];
    let mut buf = [0u8; 8];

    loop {
        let _ = conn.flush();

        match poll(&mut poll_fds, None) {
            Ok(_) => {
                if poll_fds[0].revents().contains(PollFlags::IN) {
                    let _ = read(&wake_fd, &mut buf);
                    state.redraw_and_commit();
                }

                if poll_fds[1].revents().contains(PollFlags::IN) {
                    if let Err(e) = conn.prepare_read().unwrap().read() {
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
