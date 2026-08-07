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
    BATTERY_MASK, COLOR_BAT, COLOR_DATE, COLOR_TIME, COLOR_WS_FOCUSED, COLOR_WS_OPEN, DATE_MASK,
    TIME_MASK, WORKSPACES_MASK, error::LeanbarError, font::GlyphCache, font::RasterizedGlyph,
    unpack_battery, unpack_date, unpack_time, unpack_workspaces,
};

const BAR_HEIGHT: usize = 28;
const MARGIN_LEFT: usize = 10;
const MARGIN_RIGHT: usize = 10;
const MARGIN_GAP: usize = 24;

const BATTERY_SLOT_MAX_WIDTH: usize = 180;

/// Safe wrapper around shared memory map for Wayland pixel buffer.
pub struct ShmBuffer {
    ptr: *mut u32,
    size: usize,
}

impl ShmBuffer {
    pub fn new(memfd: &rustix::fd::OwnedFd, size: usize) -> Result<Self, LeanbarError> {
        let ptr = unsafe {
            mmap(
                ptr::null_mut(),
                size,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED,
                memfd,
                0,
            )?
        };
        Ok(Self {
            ptr: ptr.cast(),
            size,
        })
    }

    pub fn as_slice_mut(&mut self) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.size / 4) }
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.size > 0 {
            let _ = unsafe { munmap(self.ptr.cast(), self.size) };
        }
    }
}

/// A thin wrapper around the raw pixel buffer for drawing operations.
struct PixelBuffer<'a> {
    pixels: &'a mut [u32],
    width: usize,
    height: usize,
}

/// Stores the last rendered state to enable efficient partial updates (damage tracking).
#[derive(Default)]
struct DrawCache {
    ws_mask: u16, // bits 13..10: active_ws, bits 9..0: occupied mask
    ws_render_width: usize,
    time_mask: u16,
    date_mask: u32,
    battery_mask: u32,
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
        for row in self.pixels.chunks_exact_mut(self.width).take(self.height) {
            row[x..x + actual_w].fill(0);
        }
    }

    fn draw_glyph(&mut self, x: usize, y: usize, glyph: &RasterizedGlyph, color: u32) {
        if glyph.coverage.is_empty() || x >= self.width || y >= self.height {
            return;
        }
        let color_a = (color >> 24) & 0xFF;
        let color_r = (color >> 16) & 0xFF;
        let color_g = (color >> 8) & 0xFF;
        let color_b = color & 0xFF;

        let mask = &glyph.coverage;
        let max_gy = glyph.height.min(self.height - y);
        let max_gx = glyph.width.min(self.width - x);

        let mut row_start = y * self.width + x;
        let mut mask_start = 0;

        for _ in 0..max_gy {
            let pixel_row = &mut self.pixels[row_start..row_start + max_gx];
            let mask_row = &mask[mask_start..mask_start + max_gx];

            for (px_out, &alpha_u8) in pixel_row.iter_mut().zip(mask_row) {
                let alpha = alpha_u8 as u32;
                match alpha {
                    0 => continue,
                    255 => *px_out = color,
                    _ => {
                        let r = (color_r * alpha + 128) >> 8;
                        let g = (color_g * alpha + 128) >> 8;
                        let b = (color_b * alpha + 128) >> 8;
                        let a = (color_a * alpha + 128) >> 8;

                        *px_out = (a << 24) | (r << 16) | (g << 8) | b;
                    }
                }
            }
            row_start += self.width;
            mask_start += glyph.width;
        }
    }

    fn draw_centered(
        &mut self,
        x: &mut usize,
        glyph: &RasterizedGlyph,
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

    fn get_digits(num: u32, pad: usize) -> ([u8; 10], usize) {
        let mut digits = [0u8; 10];
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

    fn measure_num(glyphs: &GlyphCache, num: u32, pad: usize, spacing: usize) -> usize {
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
        glyphs: &GlyphCache,
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
    pub pixels: Option<ShmBuffer>,
    pub width: u32,
    pub height: u32,
    pub configured: bool,

    pub force_full_redraw: bool,
    cache: DrawCache,

    pub glyphs: Option<GlyphCache>,
}

impl AppState {
    pub fn new(glyphs: Option<GlyphCache>) -> Self {
        Self {
            compositor: None,
            shm: None,
            layer_shell: None,
            layer_surface: None,
            wl_surface: None,
            buffer: None,
            pixels: None,
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
        let pixels_buf = match self.pixels.as_mut() {
            Some(p) if self.width > 0 => p,
            _ => return false,
        };
        if self.glyphs.is_none() {
            return false;
        }

        let current_ws_mask = WORKSPACES_MASK.load(Ordering::Relaxed);
        let time_mask = TIME_MASK.load(Ordering::Relaxed);
        let date_mask = DATE_MASK.load(Ordering::Relaxed);
        let battery_mask = BATTERY_MASK.load(Ordering::Relaxed);

        let ws_changed = self.force_full_redraw || current_ws_mask != self.cache.ws_mask;
        let clock_changed = self.force_full_redraw || time_mask != self.cache.time_mask;
        let date_changed = self.force_full_redraw || date_mask != self.cache.date_mask;
        let bat_changed = self.force_full_redraw || battery_mask != self.cache.battery_mask;

        if !ws_changed && !clock_changed && !date_changed && !bat_changed {
            return false;
        }

        let slice = pixels_buf.as_slice_mut();
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
            renderer.draw_workspaces(current_ws_mask);
        }

        let center = renderer.pb.width / 2;
        if date_changed {
            renderer.draw_date_module(center, date_mask);
        }

        if clock_changed {
            renderer.draw_clock_module(center, time_mask);
        }

        if bat_changed && battery_mask != 0 {
            renderer.draw_battery_module(battery_mask);
        }

        self.force_full_redraw = false;
        true
    }
}

// helper to coordinate drawing a single frame.
struct Renderer<'a> {
    pb: &'a mut PixelBuffer<'a>,
    glyphs: &'a GlyphCache,
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

    fn draw_workspaces(&mut self, ws_mask: u16) {
        let (active_ws, mask) = unpack_workspaces(ws_mask);
        let mut total_width = 0;
        for num in 1..=10 {
            if (mask & (1 << (num - 1))) != 0 || active_ws == num {
                total_width += PixelBuffer::measure_num(self.glyphs, num as u32, 1, 1) + 10;
            }
        }

        let old_width = self.cache.ws_render_width;
        self.cache.ws_mask = ws_mask;
        self.cache.ws_render_width = total_width;

        self.clear_and_damage_slot(0, old_width.max(total_width));

        let mut cursor_x = MARGIN_LEFT;
        for num in 1..=10 {
            if (mask & (1 << (num - 1))) != 0 || active_ws == num {
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

    fn draw_date_module(&mut self, center: usize, date_mask: u32) {
        let (day, month, year) = unpack_date(date_mask);
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

        self.cache.date_mask = date_mask;
    }

    fn draw_clock_module(&mut self, center: usize, time_mask: u16) {
        let (hour, minute) = unpack_time(time_mask);
        let max_width = (self.glyphs.max_digit_width * 4)
            + self.glyphs.colon.width
            + self.glyphs.space.width
            + self.glyphs.max_ampm_width
            + 10;
        let slot_x = center + (MARGIN_GAP / 2);
        self.clear_and_damage_slot(slot_x, max_width);

        let mut cursor_x = slot_x;
        let color = COLOR_TIME;
        let hour_12 = match hour {
            0 => 12,
            13..=23 => hour - 12,
            _ => hour,
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

        self.cache.time_mask = time_mask;
    }

    fn draw_battery_module(&mut self, battery_mask: u32) {
        let (percent, state, estimate) = unpack_battery(battery_mask);

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
            let est_h = (estimate / 60) as u32;
            let est_m = (estimate % 60) as u32;
            let content_width = PixelBuffer::measure_num(self.glyphs, percent as u32, 1, 1)
                + 1
                + self.glyphs.percent.width
                + 3
                + self.glyphs.plus.width
                + 3
                + PixelBuffer::measure_num(self.glyphs, est_h, 2, 1)
                + 1
                + self.glyphs.colon.width
                + 1
                + PixelBuffer::measure_num(self.glyphs, est_m, 2, 0);
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
            self.pb
                .draw_num(&mut cursor_x, self.glyphs, est_h, color, 2, 1);
            cursor_x += 1;
            self.pb
                .draw_centered(&mut cursor_x, &self.glyphs.colon, color, 1);
            self.pb
                .draw_num(&mut cursor_x, self.glyphs, est_m, color, 2, 0);
        }
        self.cache.battery_mask = battery_mask;
    }
}

impl Drop for AppState {
    fn drop(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
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

                state.pixels = None;

                state.width = w;
                state.height = h;

                let stride = w * 4;
                let size = (stride * h) as usize;

                let memfd = memfd_create("leanbar-shm", MemfdFlags::CLOEXEC).unwrap();
                ftruncate(&memfd, size as u64).unwrap();

                state.pixels = Some(ShmBuffer::new(&memfd, size).unwrap());

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
