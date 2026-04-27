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
    ACTIVE_WORKSPACE, BATTERY_ESTIMATE_M, BATTERY_PERCENT, BATTERY_STATE, COLOR_BAT, COLOR_DATE,
    COLOR_TIME, COLOR_WS_FOCUSED, COLOR_WS_OPEN, DATE_DAY, DATE_MONTH, DATE_YEAR, TIME_HOURS,
    TIME_MINUTES, WORKSPACES, error::LeanbarError, font_renderer,
};

const BAR_HEIGHT: usize = 28;
const MARGIN_LEFT: usize = 10;
const MARGIN_RIGHT: usize = 10;
const MARGIN_GAP: usize = 24;

const BATTERY_SLOT_MAX_WIDTH: usize = 180;

struct PixelBuffer<'a> {
    pixels: &'a mut [u32],
    width: usize,
    height: usize,
}

struct DrawCache {
    active_ws: u8,
    workspaces: [bool; 10],
    ws_render_width: usize,
    minute: u8,
    hour: u8,
    day: u8,
    month: u8,
    year: u8,
    bat_percent: u8,
    bat_state: u8,
    bat_est_min: u16,
}

impl Default for DrawCache {
    fn default() -> Self {
        Self {
            active_ws: 255,
            workspaces: [false; 10],
            ws_render_width: 0,
            minute: 255,
            hour: 255,
            day: 255,
            month: 255,
            year: 255,
            bat_percent: 255,
            bat_state: 255,
            bat_est_min: 65535,
        }
    }
}

impl<'a> PixelBuffer<'a> {
    fn new(pixels: &'a mut [u32], width: usize, height: usize) -> Self {
        Self {
            pixels,
            width,
            height,
        }
    }

    fn clear_rect(&mut self, x: usize, width: usize) {
        if x >= self.width || width == 0 {
            return;
        }
        let actual_w = width.min(self.width - x);
        for y in 0..self.height {
            let start = y * self.width + x;
            self.pixels[start..start + actual_w].fill(0);
        }
    }

    fn draw_glyph(
        &mut self,
        x: usize,
        y: usize,
        glyph: &font_renderer::RasterizedGlyph,
        color: u32,
    ) {
        if glyph.coverage.is_empty() {
            return;
        }
        let ca = (color >> 24) & 0xFF;
        let cr = (color >> 16) & 0xFF;
        let cg = (color >> 8) & 0xFF;
        let cb = color & 0xFF;

        for gy in 0..glyph.height {
            let py = y + gy;
            if py >= self.height {
                continue;
            }
            let row_idx = py * self.width;
            for gx in 0..glyph.width {
                let px = x + gx;
                if px >= self.width {
                    continue;
                }

                let alpha = glyph.coverage[gy * glyph.width + gx] as u32;
                if alpha == 0 {
                    continue;
                }

                let a = (ca * alpha) / 255;
                let r = (cr * a) / 255;
                let g = (cg * a) / 255;
                let b = (cb * a) / 255;

                self.pixels[row_idx + px] = (a << 24) | (r << 16) | (g << 8) | b;
            }
        }
    }

    fn measure_num(
        glyphs: &font_renderer::GlyphCache,
        num: u32,
        pad: usize,
        spacing: usize,
    ) -> usize {
        let mut w = 0;
        let mut temp = num;
        let mut len = 0;
        if temp == 0 {
            len = 1;
            w += glyphs.numbers[0].width + spacing;
        } else {
            while temp > 0 {
                w += glyphs.numbers[(temp % 10) as usize].width + spacing;
                temp /= 10;
                len += 1;
            }
        }
        while len < pad {
            w += glyphs.numbers[0].width + spacing;
            len += 1;
        }
        w.saturating_sub(spacing)
    }

    fn draw_num(
        &mut self,
        x: &mut usize,
        glyphs: &font_renderer::GlyphCache,
        num: u32,
        color: u32,
        pad: usize,
        spacing: usize,
    ) {
        let mut s = [0u8; 11];
        let mut len = 0;
        let mut temp = num;
        if temp == 0 {
            s[0] = 0;
            len = 1;
        } else {
            while temp > 0 {
                s[len] = (temp % 10) as u8;
                temp /= 10;
                len += 1;
            }
        }
        while len < pad {
            s[len] = 0;
            len += 1;
        }
        for i in (0..len).rev() {
            let g = &glyphs.numbers[s[i] as usize];
            self.draw_glyph(*x, (BAR_HEIGHT.saturating_sub(g.height)) / 2, g, color);
            *x += g.width;
            if i > 0 {
                *x += spacing;
            }
        }
    }
}

pub struct AppState {
    pub compositor: Option<WlCompositor>,
    pub shm: Option<WlShm>,
    pub layer_shell: Option<ZwlrLayerShellV1>,

    pub layer_surface: Option<ZwlrLayerSurfaceV1>,
    pub wl_surface: Option<WlSurface>,
    pub buffer: Option<WlBuffer>,
    pub pixels: *mut u32,
    pub pixels_len: usize,
    pub width: u32,
    pub height: u32,
    pub configured: bool,

    pub force_full_redraw: bool,
    cache: DrawCache,

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
            cache: DrawCache::default(),
            glyphs,
        }
    }

    pub fn has_required_globals(&self) -> bool {
        self.compositor.is_some() && self.shm.is_some() && self.layer_shell.is_some()
    }

    pub fn initialize_layer_surface(&mut self, qh: &QueueHandle<Self>) -> Result<(), LeanbarError> {
        let compositor = self
            .compositor
            .as_ref()
            .ok_or_else(|| LeanbarError::Wayland("missing wl_compositor".into()))?;
        let layer_shell = self
            .layer_shell
            .as_ref()
            .ok_or_else(|| LeanbarError::Wayland("missing zwlr_layer_shell_v1".into()))?;

        let wl_surface = compositor.create_surface(qh, ());
        let layer_surface = layer_shell.get_layer_surface(
            &wl_surface,
            None,
            zwlr_layer_shell_v1::Layer::Top,
            "leanbar".to_string(),
            qh,
            (),
        );

        layer_surface.set_anchor(
            zwlr_layer_surface_v1::Anchor::Bottom
                | zwlr_layer_surface_v1::Anchor::Left
                | zwlr_layer_surface_v1::Anchor::Right,
        );
        layer_surface.set_size(0, BAR_HEIGHT as u32);
        layer_surface.set_exclusive_zone(BAR_HEIGHT as i32);

        wl_surface.commit();

        self.wl_surface = Some(wl_surface);
        self.layer_surface = Some(layer_surface);

        Ok(())
    }

    pub fn redraw_and_commit(&mut self) {
        if self.configured
            && self.draw_and_damage()
            && let (Some(surface), Some(buffer)) = (&self.wl_surface, &self.buffer)
        {
            surface.attach(Some(buffer), 0, 0);
            surface.commit();
        }
    }

    fn draw_and_damage(&mut self) -> bool {
        if self.pixels.is_null() || self.width == 0 || self.glyphs.is_none() {
            return false;
        }

        let active_ws = ACTIVE_WORKSPACE.load(Ordering::Acquire);
        let mut current_ws = [false; 10];
        let mut ws_changed = self.force_full_redraw || active_ws != self.cache.active_ws;
        for (i, ws) in WORKSPACES.iter().enumerate() {
            current_ws[i] = ws.load(Ordering::Acquire);
            ws_changed |= current_ws[i] != self.cache.workspaces[i];
        }

        let h = TIME_HOURS.load(Ordering::Acquire);
        let m = TIME_MINUTES.load(Ordering::Acquire);
        let day = DATE_DAY.load(Ordering::Acquire);
        let month = DATE_MONTH.load(Ordering::Acquire);
        let year = DATE_YEAR.load(Ordering::Acquire);
        let bat_p = BATTERY_PERCENT.load(Ordering::Acquire);
        let bat_s = BATTERY_STATE.load(Ordering::Acquire);
        let bat_e = BATTERY_ESTIMATE_M.load(Ordering::Acquire);

        let clock_changed =
            self.force_full_redraw || h != self.cache.hour || m != self.cache.minute;
        let date_changed = self.force_full_redraw
            || day != self.cache.day
            || month != self.cache.month
            || year != self.cache.year;
        let bat_changed = self.force_full_redraw
            || bat_p != self.cache.bat_percent
            || bat_s != self.cache.bat_state
            || bat_e != self.cache.bat_est_min;

        if !ws_changed && !clock_changed && !date_changed && !bat_changed {
            return false;
        }

        let glyphs = self.glyphs.as_ref().unwrap();
        let slice = unsafe {
            std::slice::from_raw_parts_mut(self.pixels, (self.width * self.height) as usize)
        };
        let mut pb = PixelBuffer::new(slice, self.width as usize, self.height as usize);

        if ws_changed {
            let mut w = 0;
            for (i, ws) in current_ws.iter().enumerate() {
                if *ws || active_ws == (i + 1) as u8 {
                    w += PixelBuffer::measure_num(glyphs, (i + 1) as u32, 1, 1) + 9;
                }
            }
            let clear_w = (MARGIN_LEFT + w.max(self.cache.ws_render_width)).min(pb.width);
            pb.clear_rect(0, clear_w);
            let mut cur_x = MARGIN_LEFT;
            for (i, ws) in current_ws.iter().enumerate() {
                let num = (i + 1) as u8;
                if *ws || active_ws == num {
                    let color = if active_ws == num {
                        COLOR_WS_FOCUSED
                    } else {
                        COLOR_WS_OPEN
                    };
                    pb.draw_num(&mut cur_x, glyphs, num as u32, color, 1, 1);
                    cur_x += 10;
                }
            }
            self.cache.active_ws = active_ws;
            self.cache.workspaces = current_ws;
            self.cache.ws_render_width = w;
            self.damage(0, clear_w);
        }

        let center = pb.width / 2;
        if date_changed {
            let dw_max = (glyphs.max_digit_width * 6) + (glyphs.slash.width * 2) + 10;
            let dx_stable = center.saturating_sub(MARGIN_GAP / 2).saturating_sub(dw_max);
            pb.clear_rect(dx_stable, dw_max);

            let dw = PixelBuffer::measure_num(glyphs, day as u32, 2, 1)
                + 1
                + glyphs.slash.width
                + 1
                + PixelBuffer::measure_num(glyphs, month as u32, 2, 1)
                + 1
                + glyphs.slash.width
                + 1
                + PixelBuffer::measure_num(glyphs, year as u32, 2, 0);
            let mut cur_x = center.saturating_sub(MARGIN_GAP / 2).saturating_sub(dw);

            let col = COLOR_DATE;
            pb.draw_num(&mut cur_x, glyphs, day as u32, col, 2, 1);
            cur_x += 1;
            pb.draw_glyph(
                cur_x,
                (BAR_HEIGHT - glyphs.slash.height) / 2,
                &glyphs.slash,
                col,
            );
            cur_x += glyphs.slash.width + 1;
            pb.draw_num(&mut cur_x, glyphs, month as u32, col, 2, 1);
            cur_x += 1;
            pb.draw_glyph(
                cur_x,
                (BAR_HEIGHT - glyphs.slash.height) / 2,
                &glyphs.slash,
                col,
            );
            cur_x += glyphs.slash.width + 1;
            pb.draw_num(&mut cur_x, glyphs, year as u32, col, 2, 0);
            self.cache.day = day;
            self.cache.month = month;
            self.cache.year = year;
            self.damage(dx_stable, dw_max);
        }

        if clock_changed {
            let tw_max = (glyphs.max_digit_width * 4)
                + glyphs.colon.width
                + glyphs.space.width
                + glyphs.max_ampm_width
                + 10;
            let tx = center + (MARGIN_GAP / 2);
            pb.clear_rect(tx, tw_max);
            let mut cur_x = tx;
            let col = COLOR_TIME;
            let dh = if h == 0 {
                12
            } else if h > 12 {
                h - 12
            } else {
                h
            };
            pb.draw_num(&mut cur_x, glyphs, dh as u32, col, 2, 1);
            cur_x += 1;
            pb.draw_glyph(
                cur_x,
                (BAR_HEIGHT - glyphs.colon.height) / 2,
                &glyphs.colon,
                col,
            );
            cur_x += glyphs.colon.width + 1;
            pb.draw_num(&mut cur_x, glyphs, m as u32, col, 2, 1);
            cur_x += 1;
            cur_x += glyphs.space.width + 1;
            let ap = if h >= 12 { &glyphs.pm } else { &glyphs.am };
            pb.draw_glyph(cur_x, (BAR_HEIGHT - ap.height) / 2, ap, col);
            self.cache.hour = h;
            self.cache.minute = m;
            self.damage(tx, tw_max);
        }

        if bat_changed && bat_s != 255 {
            let bx = pb.width.saturating_sub(BATTERY_SLOT_MAX_WIDTH);
            pb.clear_rect(bx, BATTERY_SLOT_MAX_WIDTH);
            let col = COLOR_BAT;
            if bat_s == 3 {
                let cur_x = pb.width.saturating_sub(MARGIN_RIGHT + glyphs.full.width);
                pb.draw_glyph(
                    cur_x,
                    (BAR_HEIGHT - glyphs.full.height) / 2,
                    &glyphs.full,
                    col,
                );
            } else {
                let bw = PixelBuffer::measure_num(glyphs, bat_p as u32, 1, 1)
                    + 1
                    + glyphs.percent.width
                    + 3
                    + glyphs.plus.width
                    + 3
                    + PixelBuffer::measure_num(glyphs, (bat_e / 60) as u32, 2, 1)
                    + 1
                    + glyphs.colon.width
                    + 1
                    + PixelBuffer::measure_num(glyphs, (bat_e % 60) as u32, 2, 0);
                let mut cur_x = pb.width.saturating_sub(MARGIN_RIGHT + bw);
                pb.draw_num(&mut cur_x, glyphs, bat_p as u32, col, 1, 1);
                cur_x += 1;
                pb.draw_glyph(
                    cur_x,
                    (BAR_HEIGHT - glyphs.percent.height) / 2,
                    &glyphs.percent,
                    col,
                );
                cur_x += glyphs.percent.width + 3;
                let sg = if bat_s == 2 {
                    &glyphs.plus
                } else {
                    &glyphs.minus
                };
                pb.draw_glyph(cur_x, (BAR_HEIGHT - sg.height) / 2, sg, col);
                cur_x += sg.width + 3;
                pb.draw_num(&mut cur_x, glyphs, (bat_e / 60) as u32, col, 2, 1);
                cur_x += 1;
                pb.draw_glyph(
                    cur_x,
                    (BAR_HEIGHT - glyphs.colon.height) / 2,
                    &glyphs.colon,
                    col,
                );
                cur_x += glyphs.colon.width + 1;
                pb.draw_num(&mut cur_x, glyphs, (bat_e % 60) as u32, col, 2, 0);
            }
            self.cache.bat_percent = bat_p;
            self.cache.bat_state = bat_s;
            self.cache.bat_est_min = bat_e;
            self.damage(bx, BATTERY_SLOT_MAX_WIDTH);
        }

        self.force_full_redraw = false;
        true
    }

    fn damage(&self, x: usize, width: usize) {
        if let Some(surface) = &self.wl_surface {
            surface.damage_buffer(x as i32, 0, width as i32, self.height as i32);
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
            let h = if height == 0 {
                BAR_HEIGHT as u32
            } else {
                height
            };

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

wayland_client::delegate_noop!(AppState: ignore WlCompositor);
wayland_client::delegate_noop!(AppState: ignore WlShm);
wayland_client::delegate_noop!(AppState: ignore ZwlrLayerShellV1);
wayland_client::delegate_noop!(AppState: ignore WlSurface);
wayland_client::delegate_noop!(AppState: ignore WlBuffer);
wayland_client::delegate_noop!(AppState: ignore wayland_client::protocol::wl_shm_pool::WlShmPool);
