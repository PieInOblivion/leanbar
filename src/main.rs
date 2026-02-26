use rustix::event::{EventfdFlags, PollFd, PollFlags, eventfd, poll};
use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use rustix::io::{read, write};
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use std::os::fd::{AsFd, OwnedFd};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use wayland_client::{
    Connection, Dispatch, QueueHandle,
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_output::WlOutput,
        wl_registry::{self, WlRegistry},
        wl_shm::{self, WlShm},
        wl_surface::WlSurface,
    },
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};

mod font_renderer;
mod threads;

// --- Global Lock-Free State ---

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

// --- Wayland State ---
struct AppState {
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,

    // Render state
    layer_surface: Option<ZwlrLayerSurfaceV1>,
    wl_surface: Option<WlSurface>,
    buffer: Option<WlBuffer>,
    pixels: *mut u8,
    pixels_len: usize,
    width: u32,
    height: u32,
    configured: bool,

    // Damage tracking
    force_full_redraw: bool,
    last_active_ws: u8,
    last_workspaces: [bool; 10],
    last_h: u8,
    last_m: u8,
    last_day: u8,
    last_month: u8,
    last_year: u8,

    // Cached Font Glyphs
    glyphs: Option<font_renderer::GlyphCache>,
}

impl AppState {
    fn date_content_width(
        glyphs: &font_renderer::GlyphCache,
        day: u8,
        month: u8,
        year: u8,
    ) -> usize {
        glyphs.calendar_icon.width
            + 6
            + glyphs.numbers[(day / 10) as usize].width
            + 1
            + glyphs.numbers[(day % 10) as usize].width
            + 1
            + glyphs.slash.width
            + 1
            + glyphs.numbers[(month / 10) as usize].width
            + 1
            + glyphs.numbers[(month % 10) as usize].width
            + 1
            + glyphs.slash.width
            + 1
            + glyphs.numbers[(year / 10) as usize].width
            + 1
            + glyphs.numbers[(year % 10) as usize].width
    }

    fn time_content_width(glyphs: &font_renderer::GlyphCache, h: u8, m: u8) -> usize {
        let display_h = if h == 0 {
            12
        } else if h > 12 {
            h - 12
        } else {
            h
        };
        let am_pm = if h >= 12 { &glyphs.pm } else { &glyphs.am };

        glyphs.time_icon.width
            + 6
            + glyphs.numbers[(display_h / 10) as usize].width
            + 1
            + glyphs.numbers[(display_h % 10) as usize].width
            + 1
            + glyphs.colon.width
            + 1
            + glyphs.numbers[(m / 10) as usize].width
            + 1
            + glyphs.numbers[(m % 10) as usize].width
            + 1
            + glyphs.space.width
            + 1
            + am_pm.width
    }

    fn draw_bar(&mut self) -> Vec<(i32, i32, i32, i32)> {
        let mut damage = Vec::with_capacity(3);

        if self.pixels.is_null() || self.width == 0 || self.height == 0 {
            return damage;
        }

        let len = (self.width * self.height * 4) as usize;
        let slice = unsafe { std::slice::from_raw_parts_mut(self.pixels, len) };
        let stride = (self.width * 4) as usize;

        let active_ws = ACTIVE_WORKSPACE.load(Ordering::Acquire);
        let mut current_ws = [false; 10];
        let mut ws_changed = self.force_full_redraw || active_ws != self.last_active_ws;
        for (i, ws) in WORKSPACES.iter().enumerate() {
            current_ws[i] = ws.load(Ordering::Acquire);
            if current_ws[i] != self.last_workspaces[i] {
                ws_changed = true;
            }
        }

        let h = TIME_HOURS.load(Ordering::Acquire);
        let m = TIME_MINUTES.load(Ordering::Acquire);
        let day = DATE_DAY.load(Ordering::Acquire);
        let month = DATE_MONTH.load(Ordering::Acquire);
        let year = DATE_YEAR.load(Ordering::Acquire);

        let clock_changed = self.force_full_redraw || h != self.last_h || m != self.last_m;
        let date_changed = self.force_full_redraw
            || day != self.last_day
            || month != self.last_month
            || year != self.last_year;

        if !ws_changed && !clock_changed && !date_changed {
            return damage;
        }

        if let Some(glyphs) = &self.glyphs {
            let color_time = [0xf7, 0xa6, 0xcb, 0xff];
            let color_ws_focused = [0xff, 0xff, 0xff, 0xff];
            let color_ws_other = [0xf7, 0xa6, 0xcb, 0xff];
            let color_date = [0xec, 0xc7, 0x74, 0xff];

            let max_digit_width = glyphs.numbers.iter().map(|g| g.width).max().unwrap_or(0);
            let max_ampm_width = glyphs.am.width.max(glyphs.pm.width);
            let date_slot_width =
                glyphs.calendar_icon.width + (max_digit_width * 6) + (glyphs.slash.width * 2) + 13;
            let time_slot_width = glyphs.time_icon.width
                + (max_digit_width * 4)
                + glyphs.colon.width
                + glyphs.space.width
                + max_ampm_width
                + 12;
            let center_gap = 24usize;
            let center_total_width = date_slot_width + center_gap + time_slot_width;
            let center_start = (self.width as usize).saturating_sub(center_total_width) / 2;
            let date_slot_x = center_start;
            let time_slot_x = center_start + date_slot_width + center_gap;

            if ws_changed {
                let ws_area_width = 600.min(self.width as usize);
                for y in 0..self.height as usize {
                    let start = y * stride;
                    let end = start + ws_area_width * 4;
                    slice[start..end].fill(0);
                }

                let mut current_x = 10;
                for (i, ws) in current_ws.iter().enumerate() {
                    let ws_num = i + 1;
                    if *ws || active_ws == ws_num as u8 {
                        let color = if active_ws == ws_num as u8 {
                            color_ws_focused
                        } else {
                            color_ws_other
                        };

                        if ws_num == 10 {
                            let y_offset1 = (28usize.saturating_sub(glyphs.numbers[1].height)) / 2;
                            Self::draw_glyph(
                                slice,
                                stride,
                                current_x,
                                y_offset1,
                                color,
                                &glyphs.numbers[1],
                            );
                            current_x += glyphs.numbers[1].width + 1;

                            let y_offset0 = (28usize.saturating_sub(glyphs.numbers[0].height)) / 2;
                            Self::draw_glyph(
                                slice,
                                stride,
                                current_x,
                                y_offset0,
                                color,
                                &glyphs.numbers[0],
                            );
                            current_x += glyphs.numbers[0].width + 10;
                        } else {
                            let y_offset =
                                (28usize.saturating_sub(glyphs.numbers[ws_num].height)) / 2;
                            Self::draw_glyph(
                                slice,
                                stride,
                                current_x,
                                y_offset,
                                color,
                                &glyphs.numbers[ws_num],
                            );
                            current_x += glyphs.numbers[ws_num].width + 10;
                        }
                    }
                }

                damage.push((0, 0, ws_area_width as i32, self.height as i32));
                self.last_active_ws = active_ws;
                self.last_workspaces = current_ws;
            }

            if date_changed && date_slot_x < self.width as usize {
                for y in 0..self.height as usize {
                    let start = y * stride + date_slot_x * 4;
                    let end = start + date_slot_width * 4;
                    if end <= len {
                        slice[start..end].fill(0);
                    }
                }

                let date_content_width = Self::date_content_width(glyphs, day, month, year);
                let mut current_x =
                    date_slot_x + date_slot_width.saturating_sub(date_content_width) / 2;
                let mut draw_char =
                    |g: &font_renderer::RasterizedGlyph, color: [u8; 4], extra_margin: usize| {
                        let y = (28usize.saturating_sub(g.height)) / 2;
                        Self::draw_glyph(slice, stride, current_x, y, color, g);
                        current_x += g.width + extra_margin;
                    };

                draw_char(&glyphs.calendar_icon, color_date, 6);
                draw_char(&glyphs.numbers[(day / 10) as usize], color_date, 1);
                draw_char(&glyphs.numbers[(day % 10) as usize], color_date, 1);
                draw_char(&glyphs.slash, color_date, 1);
                draw_char(&glyphs.numbers[(month / 10) as usize], color_date, 1);
                draw_char(&glyphs.numbers[(month % 10) as usize], color_date, 1);
                draw_char(&glyphs.slash, color_date, 1);
                draw_char(&glyphs.numbers[(year / 10) as usize], color_date, 1);
                draw_char(&glyphs.numbers[(year % 10) as usize], color_date, 0);

                let dmg_w = date_slot_width.min((self.width as usize) - date_slot_x);
                if dmg_w > 0 {
                    damage.push((date_slot_x as i32, 0, dmg_w as i32, self.height as i32));
                }

                self.last_day = day;
                self.last_month = month;
                self.last_year = year;
            }

            if clock_changed && time_slot_x < self.width as usize {
                for y in 0..self.height as usize {
                    let start = y * stride + time_slot_x * 4;
                    let end = start + time_slot_width * 4;
                    if end <= len {
                        slice[start..end].fill(0);
                    }
                }

                let time_content_width = Self::time_content_width(glyphs, h, m);
                let mut current_x =
                    time_slot_x + time_slot_width.saturating_sub(time_content_width) / 2;
                let mut draw_char =
                    |g: &font_renderer::RasterizedGlyph, color: [u8; 4], extra_margin: usize| {
                        let y = (28usize.saturating_sub(g.height)) / 2;
                        Self::draw_glyph(slice, stride, current_x, y, color, g);
                        current_x += g.width + extra_margin;
                    };

                let display_h = if h == 0 {
                    12
                } else if h > 12 {
                    h - 12
                } else {
                    h
                };
                let am_pm = if h >= 12 { &glyphs.pm } else { &glyphs.am };

                draw_char(&glyphs.time_icon, color_time, 6);
                draw_char(&glyphs.numbers[(display_h / 10) as usize], color_time, 1);
                draw_char(&glyphs.numbers[(display_h % 10) as usize], color_time, 1);
                draw_char(&glyphs.colon, color_time, 1);
                draw_char(&glyphs.numbers[(m / 10) as usize], color_time, 1);
                draw_char(&glyphs.numbers[(m % 10) as usize], color_time, 1);
                draw_char(&glyphs.space, color_time, 1);
                draw_char(am_pm, color_time, 0);

                let dmg_w = time_slot_width.min((self.width as usize) - time_slot_x);
                if dmg_w > 0 {
                    damage.push((time_slot_x as i32, 0, dmg_w as i32, self.height as i32));
                }

                self.last_h = h;
                self.last_m = m;
            }

            self.force_full_redraw = false;
        }

        damage
    }

    fn draw_glyph(
        pixels: &mut [u8],
        stride: usize,
        start_x: usize,
        start_y: usize,
        color: [u8; 4],
        glyph: &font_renderer::RasterizedGlyph,
    ) {
        if glyph.coverage.is_empty() {
            return;
        }

        for gy in 0..glyph.height {
            let py = start_y + gy;
            if py >= 28 {
                continue;
            }

            for gx in 0..glyph.width {
                let px = start_x + gx;
                if px >= (stride / 4) {
                    continue;
                }

                let alpha = glyph.coverage[gy * glyph.width + gx] as u32;
                if alpha == 0 {
                    continue;
                }

                let dst_idx = py * stride + px * 4;
                let a = (color[3] as u32 * alpha) / 255;
                let b = (color[0] as u32 * a) / 255;
                let g = (color[1] as u32 * a) / 255;
                let r = (color[2] as u32 * a) / 255;

                pixels[dst_idx] = b as u8;
                pixels[dst_idx + 1] = g as u8;
                pixels[dst_idx + 2] = r as u8;
                pixels[dst_idx + 3] = a as u8;
            }
        }
    }
}

// ... (Wayland Dispatch boilerplate remains the same)
impl Dispatch<WlRegistry, ()> for AppState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, 4, qhandle, ()));
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind(name, 1, qhandle, ()));
                }
                "zwlr_layer_shell_v1" => {
                    state.layer_shell = Some(registry.bind(name, 4, qhandle, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for AppState {
    fn event(
        state: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: <ZwlrLayerSurfaceV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            layer_surface.ack_configure(serial);

            let w = if width == 0 { 1920 } else { width };
            let h = if height == 0 { 28 } else { height };

            if state.width != w || state.height != h {
                if let Some(old_buffer) = state.buffer.take() {
                    old_buffer.destroy();
                }

                if !state.pixels.is_null() && state.pixels_len > 0 {
                    let _ = unsafe { munmap(state.pixels.cast(), state.pixels_len) };
                    state.pixels = ptr::null_mut();
                    state.pixels_len = 0;
                }

                state.width = w;
                state.height = h;

                let stride = w * 4;
                let size = stride * h;

                let memfd = memfd_create("leanbar-shm", MemfdFlags::CLOEXEC).unwrap();
                ftruncate(&memfd, size as u64).unwrap();

                let ptr = unsafe {
                    mmap(
                        ptr::null_mut(),
                        size as usize,
                        ProtFlags::READ | ProtFlags::WRITE,
                        MapFlags::SHARED,
                        &memfd,
                        0,
                    )
                    .unwrap()
                };

                state.pixels = ptr.cast();
                state.pixels_len = size as usize;

                let pool = state.shm.as_ref().unwrap().create_pool(
                    memfd.as_fd(),
                    size as i32,
                    qhandle,
                    (),
                );
                let buffer = pool.create_buffer(
                    0,
                    w as i32,
                    h as i32,
                    stride as i32,
                    wl_shm::Format::Argb8888,
                    qhandle,
                    (),
                );
                state.buffer = Some(buffer);
            }

            state.configured = true;
            state.force_full_redraw = true;

            let damages = state.draw_bar();

            if let Some(surface) = &state.wl_surface
                && let Some(buf) = &state.buffer
            {
                surface.attach(Some(buf), 0, 0);
                for (x, y, w, h) in damages {
                    surface.damage_buffer(x, y, w, h);
                }
                surface.commit();
            }
        }
    }
}

impl Dispatch<WlCompositor, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &WlCompositor,
        _: <WlCompositor as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<WlShm, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &WlShm,
        _: <WlShm as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ZwlrLayerShellV1, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &ZwlrLayerShellV1,
        _: <ZwlrLayerShellV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<WlOutput, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &WlOutput,
        _: <WlOutput as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<WlSurface, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &WlSurface,
        _: <WlSurface as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<WlBuffer, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &WlBuffer,
        _: <WlBuffer as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<wayland_client::protocol::wl_shm_pool::WlShmPool, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_shm_pool::WlShmPool,
        _: <wayland_client::protocol::wl_shm_pool::WlShmPool as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
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

    let mut state = AppState {
        compositor: None,
        shm: None,
        layer_shell: None,
        layer_surface: None,
        wl_surface: None,
        buffer: None,
        pixels: ptr::null_mut(),
        pixels_len: 0,
        width: 0,
        height: 0,
        configured: false,
        force_full_redraw: true,
        last_active_ws: 255, // Impossible default to force initial draw
        last_workspaces: [false; 10],
        last_h: 255,
        last_m: 255,
        last_day: 255,
        last_month: 255,
        last_year: 255,
        glyphs: glyph_cache,
    };

    println!("Discovering Wayland globals...");
    event_queue.roundtrip(&mut state)?;

    if state.compositor.is_none() || state.shm.is_none() || state.layer_shell.is_none() {
        eprintln!("Failed to bind essential Wayland globals.");
        return Ok(());
    }

    let compositor = state.compositor.as_ref().unwrap();
    let layer_shell = state.layer_shell.as_ref().unwrap();

    let wl_surface = compositor.create_surface(&qh, ());
    let layer_surface = layer_shell.get_layer_surface(
        &wl_surface,
        None,
        zwlr_layer_shell_v1::Layer::Top,
        "leanbar".to_string(),
        &qh,
        (),
    );

    layer_surface.set_size(0, 28);
    layer_surface.set_anchor(
        zwlr_layer_surface_v1::Anchor::Bottom
            | zwlr_layer_surface_v1::Anchor::Left
            | zwlr_layer_surface_v1::Anchor::Right,
    );
    layer_surface.set_exclusive_zone(28);

    wl_surface.commit();

    state.wl_surface = Some(wl_surface);
    state.layer_surface = Some(layer_surface);

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

                    if state.configured {
                        let damages = state.draw_bar();
                        if !damages.is_empty()
                            && let Some(surface) = &state.wl_surface
                            && let Some(buffer) = &state.buffer
                        {
                            surface.attach(Some(buffer), 0, 0);
                            for (x, y, w, h) in damages {
                                surface.damage_buffer(x, y, w, h);
                            }
                            surface.commit();
                        }
                    }
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
