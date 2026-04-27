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

/// A thin wrapper around the raw pixel buffer for drawing operations.
struct PixelBuffer<'a> {
    pixels: &'a mut [u32],
    width: usize,
    height: usize,
}

/// Stores the last rendered state to enable efficient partial updates (damage tracking).
struct DrawCache {
    active_ws: u8,
    workspaces: u16, // Bitmask of occupied workspaces
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
            workspaces: 0,
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
        let color_a = (color >> 24) & 0xFF;
        let color_r = (color >> 16) & 0xFF;
        let color_g = (color >> 8) & 0xFF;
        let color_b = color & 0xFF;

        let mask = &glyph.coverage;
        for gy in 0..glyph.height {
            let py = y + gy;
            if py >= self.height {
                break;
            }
            for gx in 0..glyph.width {
                let px = x + gx;
                if px >= self.width {
                    continue;
                }
                let mask_idx = gy * glyph.width + gx;
                let alpha = mask[mask_idx] as u32;
                if alpha == 0 {
                    continue;
                }

                let r = (color_r * alpha) / 255;
                let g = (color_g * alpha) / 255;
                let b = (color_b * alpha) / 255;
                let a = (color_a * alpha) / 255;

                let dst_idx = py * self.width + px;
                self.pixels[dst_idx] = (a << 24) | (r << 16) | (g << 8) | b;
            }
        }
    }

    fn draw_centered(
        &mut self,
        x: &mut usize,
        glyph: &font_renderer::RasterizedGlyph,
        color: u32,
        trailing: usize,
    ) {
        self.draw_glyph(
            *x,
            (BAR_HEIGHT.saturating_sub(glyph.height)) / 2,
            glyph,
            color,
        );
        *x += glyph.width + trailing;
    }

    fn get_digits(num: u32, pad: usize) -> ([u8; 11], usize) {
        let mut digits = [0u8; 11];
        let mut len = 0;
        let mut temp = num;
        if temp == 0 {
            digits[0] = 0;
            len = 1;
        } else {
            while temp > 0 {
                digits[len] = (temp % 10) as u8;
                temp /= 10;
                len += 1;
            }
        }
        while len < pad {
            digits[len] = 0;
            len += 1;
        }
        (digits, len)
    }

    fn measure_num(
        glyphs: &font_renderer::GlyphCache,
        num: u32,
        pad: usize,
        spacing: usize,
    ) -> usize {
        let (digits, len) = Self::get_digits(num, pad);
        let mut width = 0;
        for i in (0..len).rev() {
            width += glyphs.numbers[digits[i] as usize].width;
            if i > 0 {
                width += spacing;
            }
        }
        width
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
        let (digits, len) = Self::get_digits(num, pad);
        for i in (0..len).rev() {
            let g = &glyphs.numbers[digits[i] as usize];
            self.draw_centered(x, g, color, if i > 0 { spacing } else { 0 });
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
        let hour = TIME_HOURS.load(Ordering::Acquire);
        let minute = TIME_MINUTES.load(Ordering::Acquire);
        let day = DATE_DAY.load(Ordering::Acquire);
        let month = DATE_MONTH.load(Ordering::Acquire);
        let year = DATE_YEAR.load(Ordering::Acquire);
        let battery_percent = BATTERY_PERCENT.load(Ordering::Acquire);
        let battery_state = BATTERY_STATE.load(Ordering::Acquire);
        let battery_estimate = BATTERY_ESTIMATE_M.load(Ordering::Acquire);

        let mut current_ws_mask: u16 = 0;
        for (i, ws) in WORKSPACES.iter().enumerate() {
            if ws.load(Ordering::Acquire) {
                current_ws_mask |= 1 << i;
            }
        }

        let ws_changed = self.force_full_redraw
            || current_ws_mask != self.cache.workspaces
            || active_ws != self.cache.active_ws;
        let clock_changed =
            self.force_full_redraw || hour != self.cache.hour || minute != self.cache.minute;
        let date_changed = self.force_full_redraw
            || day != self.cache.day
            || month != self.cache.month
            || year != self.cache.year;
        let bat_changed = self.force_full_redraw
            || battery_percent != self.cache.bat_percent
            || battery_state != self.cache.bat_state
            || battery_estimate != self.cache.bat_est_min;

        if !ws_changed && !clock_changed && !date_changed && !bat_changed {
            return false;
        }

        let slice = unsafe {
            std::slice::from_raw_parts_mut(self.pixels, (self.width * self.height) as usize)
        };
        let mut pb = PixelBuffer::new(slice, self.width as usize, self.height as usize);
        let glyphs = self.glyphs.as_ref().unwrap();

        let mut renderer = Renderer {
            pb: &mut pb,
            glyphs,
            cache: &mut self.cache,
            surface: self.wl_surface.as_ref(),
            height: self.height,
        };

        if ws_changed {
            renderer.draw_workspaces(active_ws, current_ws_mask);
        }

        let center = renderer.pb.width / 2;
        if date_changed {
            renderer.draw_date_module(center, day, month, year);
        }

        if clock_changed {
            renderer.draw_clock_module(center, hour, minute);
        }

        if bat_changed && battery_state != 255 {
            renderer.draw_battery_module(battery_percent, battery_state, battery_estimate);
        }

        self.force_full_redraw = false;
        true
    }
}

// helper to coordinate drawing a single frame.
struct Renderer<'a> {
    pb: &'a mut PixelBuffer<'a>,
    glyphs: &'a font_renderer::GlyphCache,
    cache: &'a mut DrawCache,
    surface: Option<&'a WlSurface>,
    height: u32,
}

impl<'a> Renderer<'a> {
    fn clear_and_damage_slot(&mut self, x: usize, width: usize) {
        self.pb.clear_rect(x, width);
        if let Some(surface) = self.surface {
            surface.damage_buffer(x as i32, 0, width as i32, self.height as i32);
        }
    }

    fn draw_workspaces(&mut self, active_ws: u8, mask: u16) {
        let mut total_width = 0;
        for i in 0..10 {
            let num = (i + 1) as u8;
            if (mask & (1 << i)) != 0 || active_ws == num {
                total_width += PixelBuffer::measure_num(self.glyphs, num as u32, 1, 1) + 10;
            }
        }

        let old_width = self.cache.ws_render_width;
        self.cache.workspaces = mask;
        self.cache.active_ws = active_ws;
        self.cache.ws_render_width = total_width;

        self.clear_and_damage_slot(0, old_width.max(total_width));

        let mut cursor_x = MARGIN_LEFT;
        for i in 0..10 {
            let num = (i + 1) as u8;
            if (mask & (1 << i)) != 0 || active_ws == num {
                let color = if active_ws == num {
                    COLOR_WS_FOCUSED
                } else {
                    COLOR_WS_OPEN
                };
                self.pb
                    .draw_num(&mut cursor_x, self.glyphs, num as u32, color, 1, 1);
                cursor_x += 10;
            }
        }
    }

    fn draw_date_module(&mut self, center: usize, day: u8, month: u8, year: u8) {
        let max_width = (self.glyphs.max_digit_width * 6) + (self.glyphs.slash.width * 2) + 10;
        let slot_x = center
            .saturating_sub(MARGIN_GAP / 2)
            .saturating_sub(max_width);
        self.clear_and_damage_slot(slot_x, max_width);

        let content_width = PixelBuffer::measure_num(self.glyphs, day as u32, 2, 1)
            + 1
            + self.glyphs.slash.width
            + 1
            + PixelBuffer::measure_num(self.glyphs, month as u32, 2, 1)
            + 1
            + self.glyphs.slash.width
            + 1
            + PixelBuffer::measure_num(self.glyphs, year as u32, 2, 0);
        let mut cursor_x = center
            .saturating_sub(MARGIN_GAP / 2)
            .saturating_sub(content_width);

        let color = COLOR_DATE;
        self.pb
            .draw_num(&mut cursor_x, self.glyphs, day as u32, color, 2, 1);
        cursor_x += 1;
        self.pb
            .draw_centered(&mut cursor_x, &self.glyphs.slash, color, 1);
        self.pb
            .draw_num(&mut cursor_x, self.glyphs, month as u32, color, 2, 1);
        cursor_x += 1;
        self.pb
            .draw_centered(&mut cursor_x, &self.glyphs.slash, color, 1);
        self.pb
            .draw_num(&mut cursor_x, self.glyphs, year as u32, color, 2, 0);

        self.cache.day = day;
        self.cache.month = month;
        self.cache.year = year;
    }

    fn draw_clock_module(&mut self, center: usize, hour: u8, minute: u8) {
        let max_width = (self.glyphs.max_digit_width * 4)
            + self.glyphs.colon.width
            + self.glyphs.space.width
            + self.glyphs.max_ampm_width
            + 10;
        let slot_x = center + (MARGIN_GAP / 2);
        self.clear_and_damage_slot(slot_x, max_width);

        let mut cursor_x = slot_x;
        let color = COLOR_TIME;
        let hour_12 = if hour == 0 {
            12
        } else if hour > 12 {
            hour - 12
        } else {
            hour
        };
        self.pb
            .draw_num(&mut cursor_x, self.glyphs, hour_12 as u32, color, 2, 1);
        cursor_x += 1;
        self.pb
            .draw_centered(&mut cursor_x, &self.glyphs.colon, color, 1);
        self.pb
            .draw_num(&mut cursor_x, self.glyphs, minute as u32, color, 2, 1);
        cursor_x += 1;
        cursor_x += self.glyphs.space.width + 1;
        let ampm_glyph = if hour >= 12 {
            &self.glyphs.pm
        } else {
            &self.glyphs.am
        };
        self.pb.draw_centered(&mut cursor_x, ampm_glyph, color, 0);

        self.cache.hour = hour;
        self.cache.minute = minute;
    }

    fn draw_battery_module(&mut self, percent: u8, state: u8, estimate: u16) {
        let slot_x = self.pb.width.saturating_sub(BATTERY_SLOT_MAX_WIDTH);
        self.clear_and_damage_slot(slot_x, BATTERY_SLOT_MAX_WIDTH);
        let color = COLOR_BAT;

        if state == 3 {
            let mut cursor_x = self
                .pb
                .width
                .saturating_sub(MARGIN_RIGHT + self.glyphs.full.width);
            self.pb
                .draw_centered(&mut cursor_x, &self.glyphs.full, color, 0);
        } else {
            let content_width = PixelBuffer::measure_num(self.glyphs, percent as u32, 1, 1)
                + 1
                + self.glyphs.percent.width
                + 3
                + self.glyphs.plus.width
                + 3
                + PixelBuffer::measure_num(self.glyphs, (estimate / 60) as u32, 2, 1)
                + 1
                + self.glyphs.colon.width
                + 1
                + PixelBuffer::measure_num(self.glyphs, (estimate % 60) as u32, 2, 0);
            let mut cursor_x = self.pb.width.saturating_sub(MARGIN_RIGHT + content_width);
            self.pb
                .draw_num(&mut cursor_x, self.glyphs, percent as u32, color, 1, 1);
            cursor_x += 1;
            self.pb
                .draw_centered(&mut cursor_x, &self.glyphs.percent, color, 3);
            let status_glyph = if state == 2 {
                &self.glyphs.plus
            } else {
                &self.glyphs.minus
            };
            self.pb.draw_centered(&mut cursor_x, status_glyph, color, 3);
            self.pb.draw_num(
                &mut cursor_x,
                self.glyphs,
                (estimate / 60) as u32,
                color,
                2,
                1,
            );
            cursor_x += 1;
            self.pb
                .draw_centered(&mut cursor_x, &self.glyphs.colon, color, 1);
            self.pb.draw_num(
                &mut cursor_x,
                self.glyphs,
                (estimate % 60) as u32,
                color,
                2,
                0,
            );
        }
        self.cache.bat_percent = percent;
        self.cache.bat_state = state;
        self.cache.bat_est_min = estimate;
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
