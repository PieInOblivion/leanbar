use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use std::os::fd::AsFd;
use std::ptr;
use std::sync::atomic::Ordering;

use wayland_client::{
    Connection, Dispatch, QueueHandle,
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_registry::{self, WlRegistry},
        wl_shm::{self, WlShm},
        wl_surface::WlSurface,
    },
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};

use crate::{
    ACTIVE_WORKSPACE, BATTERY_ESTIMATE_M, BATTERY_PERCENT, BATTERY_STATE, DATE_DAY, DATE_MONTH,
    DATE_YEAR, TIME_HOURS, TIME_MINUTES, WORKSPACES, font_renderer,
};

pub struct AppState {
    pub compositor: Option<WlCompositor>,
    pub shm: Option<WlShm>,
    pub layer_shell: Option<ZwlrLayerShellV1>,

    pub layer_surface: Option<ZwlrLayerSurfaceV1>,
    pub wl_surface: Option<WlSurface>,
    pub buffer: Option<WlBuffer>,
    pub pixels: *mut u8,
    pub pixels_len: usize,
    pub width: u32,
    pub height: u32,
    pub configured: bool,

    pub force_full_redraw: bool,
    pub last_active_ws: u8,
    pub last_workspaces: [bool; 10],
    pub last_h: u8,
    pub last_m: u8,
    pub last_day: u8,
    pub last_month: u8,
    pub last_year: u8,
    pub last_bat_percent: u8,
    pub last_bat_state: u8,
    pub last_bat_est_m: u16,

    pub glyphs: Option<font_renderer::GlyphCache>,
}

impl AppState {
    pub fn new(glyphs: Option<font_renderer::GlyphCache>) -> Self {
        Self {
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
            last_active_ws: 255,
            last_workspaces: [false; 10],
            last_h: 255,
            last_m: 255,
            last_day: 255,
            last_month: 255,
            last_year: 255,
            last_bat_percent: 255,
            last_bat_state: 255,
            last_bat_est_m: 65535,
            glyphs,
        }
    }

    pub fn has_required_globals(&self) -> bool {
        self.compositor.is_some() && self.shm.is_some() && self.layer_shell.is_some()
    }

    pub fn initialize_layer_surface(
        &mut self,
        qh: &QueueHandle<Self>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let compositor = self.compositor.as_ref().ok_or("missing wl_compositor")?;
        let layer_shell = self
            .layer_shell
            .as_ref()
            .ok_or("missing zwlr_layer_shell_v1")?;

        let wl_surface = compositor.create_surface(qh, ());
        let layer_surface = layer_shell.get_layer_surface(
            &wl_surface,
            None,
            zwlr_layer_shell_v1::Layer::Top,
            "leanbar".to_string(),
            qh,
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

        self.wl_surface = Some(wl_surface);
        self.layer_surface = Some(layer_surface);

        Ok(())
    }

    pub fn redraw_and_commit(&mut self) {
        if !self.configured {
            return;
        }

        let damages = self.draw_bar();
        if damages.is_empty() {
            return;
        }

        if let Some(surface) = &self.wl_surface
            && let Some(buffer) = &self.buffer
        {
            surface.attach(Some(buffer), 0, 0);
            for (x, y, w, h) in damages {
                surface.damage_buffer(x, y, w, h);
            }
            surface.commit();
        }
    }

    fn date_content_width(
        glyphs: &font_renderer::GlyphCache,
        day: u8,
        month: u8,
        year: u8,
    ) -> usize {
        glyphs.numbers[(day / 10) as usize].width
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

        glyphs.numbers[(display_h / 10) as usize].width
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

    fn battery_content_width(
        glyphs: &font_renderer::GlyphCache,
        percent: u8,
        state: u8,
        est_m_total: u16,
    ) -> usize {
        if state == 3 {
            return glyphs.full.width;
        }

        let mut w = 0;
        if percent == 100 {
            w += glyphs.numbers[1].width
                + 1
                + glyphs.numbers[0].width
                + 1
                + glyphs.numbers[0].width
                + 1;
        } else if percent >= 10 {
            w += glyphs.numbers[(percent / 10) as usize].width
                + 1
                + glyphs.numbers[(percent % 10) as usize].width
                + 1;
        } else {
            w += glyphs.numbers[percent as usize].width + 1;
        }

        w += glyphs.percent.width;

        w += glyphs.space.width * 2 + 2; // "  "
        w += if state == 2 {
            glyphs.plus.width
        } else {
            glyphs.minus.width
        };
        w += glyphs.space.width * 2 + 2; // "  "

        let est_h = (est_m_total / 60) as u8;
        let est_m = (est_m_total % 60) as u8;

        w += glyphs.numbers[(est_h / 10) as usize].width
            + 1
            + glyphs.numbers[(est_h % 10) as usize].width
            + 1;
        w += glyphs.colon.width + 1;
        w += glyphs.numbers[(est_m / 10) as usize].width
            + 1
            + glyphs.numbers[(est_m % 10) as usize].width;
        w
    }

    fn draw_bar(&mut self) -> Vec<(i32, i32, i32, i32)> {
        let mut damage = Vec::with_capacity(4);

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

        let bat_percent = BATTERY_PERCENT.load(Ordering::Acquire);
        let bat_state = BATTERY_STATE.load(Ordering::Acquire);
        let bat_est_m_total = BATTERY_ESTIMATE_M.load(Ordering::Acquire);

        let bat_changed = self.force_full_redraw
            || bat_percent != self.last_bat_percent
            || bat_state != self.last_bat_state
            || bat_est_m_total != self.last_bat_est_m;

        if !ws_changed && !clock_changed && !date_changed && !bat_changed {
            return damage;
        }

        if let Some(glyphs) = &self.glyphs {
            let color_time = [0xf7, 0xa6, 0xcb, 0xff];
            let color_ws_focused = [0xff, 0xff, 0xff, 0xff];
            let color_ws_other = [0xf7, 0xa6, 0xcb, 0xff];
            let color_date = [0xec, 0xc7, 0x74, 0xff];
            let color_bat = [0xa1, 0xe3, 0xa6, 0xff];

            let max_digit_width = glyphs.numbers.iter().map(|g| g.width).max().unwrap_or(0);
            let max_ampm_width = glyphs.am.width.max(glyphs.pm.width);

            // Re-calculated widths without icons
            let date_slot_width = (max_digit_width * 6) + (glyphs.slash.width * 2) + 7;
            let time_slot_width = (max_digit_width * 4)
                + glyphs.colon.width
                + glyphs.space.width
                + max_ampm_width
                + 5;

            let center_gap = 24usize;
            let screen_center = (self.width as usize) / 2;
            let date_slot_x = screen_center
                .saturating_sub(center_gap / 2)
                .saturating_sub(date_slot_width);
            let time_slot_x = screen_center + (center_gap / 2);

            let bat_max_width = 160;
            let bat_slot_x = (self.width as usize).saturating_sub(bat_max_width);

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

            if bat_changed && bat_slot_x < self.width as usize && bat_state != 255 {
                for y in 0..self.height as usize {
                    let start = y * stride + bat_slot_x * 4;
                    let end = start + bat_max_width * 4;
                    if end <= len {
                        slice[start..end].fill(0);
                    }
                }

                let bat_content_width =
                    Self::battery_content_width(glyphs, bat_percent, bat_state, bat_est_m_total);
                // 10px right margin
                let mut current_x = (self.width as usize).saturating_sub(10 + bat_content_width);

                let mut draw_char =
                    |g: &font_renderer::RasterizedGlyph, color: [u8; 4], extra_margin: usize| {
                        let y = (28usize.saturating_sub(g.height)) / 2;
                        Self::draw_glyph(slice, stride, current_x, y, color, g);
                        current_x += g.width + extra_margin;
                    };

                if bat_state == 3 {
                    draw_char(&glyphs.full, color_bat, 0);
                } else {
                    if bat_percent == 100 {
                        draw_char(&glyphs.numbers[1], color_bat, 1);
                        draw_char(&glyphs.numbers[0], color_bat, 1);
                        draw_char(&glyphs.numbers[0], color_bat, 1);
                    } else if bat_percent >= 10 {
                        draw_char(&glyphs.numbers[(bat_percent / 10) as usize], color_bat, 1);
                        draw_char(&glyphs.numbers[(bat_percent % 10) as usize], color_bat, 1);
                    } else {
                        draw_char(&glyphs.numbers[bat_percent as usize], color_bat, 1);
                    }

                    draw_char(&glyphs.percent, color_bat, 0);

                    draw_char(&glyphs.space, color_bat, 1);
                    draw_char(&glyphs.space, color_bat, 1);

                    if bat_state == 2 {
                        draw_char(&glyphs.plus, color_bat, 0);
                    } else {
                        draw_char(&glyphs.minus, color_bat, 0);
                    }

                    draw_char(&glyphs.space, color_bat, 1);
                    draw_char(&glyphs.space, color_bat, 1);

                    let est_h = (bat_est_m_total / 60) as u8;
                    let est_m = (bat_est_m_total % 60) as u8;

                    draw_char(&glyphs.numbers[(est_h / 10) as usize], color_bat, 1);
                    draw_char(&glyphs.numbers[(est_h % 10) as usize], color_bat, 1);
                    draw_char(&glyphs.colon, color_bat, 1);
                    draw_char(&glyphs.numbers[(est_m / 10) as usize], color_bat, 1);
                    draw_char(&glyphs.numbers[(est_m % 10) as usize], color_bat, 0);
                }

                let dmg_w = bat_max_width.min((self.width as usize) - bat_slot_x);
                if dmg_w > 0 {
                    damage.push((bat_slot_x as i32, 0, dmg_w as i32, self.height as i32));
                }

                self.last_bat_percent = bat_percent;
                self.last_bat_state = bat_state;
                self.last_bat_est_m = bat_est_m_total;
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

impl Drop for AppState {
    fn drop(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }

        if !self.pixels.is_null() && self.pixels_len > 0 {
            let _ = unsafe { munmap(self.pixels.cast(), self.pixels_len) };
            self.pixels = ptr::null_mut();
            self.pixels_len = 0;
        }
    }
}

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

                let pool = state
                    .shm
                    .as_ref()
                    .expect("wl_shm must exist after globals discovery")
                    .create_pool(memfd.as_fd(), size as i32, qhandle, ());
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
            state.redraw_and_commit();
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
