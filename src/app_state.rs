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
    DATE_YEAR, TIME_HOURS, TIME_MINUTES, WORKSPACES, error::LeanbarError, font_renderer,
};

const BAR_HEIGHT: usize = 28;
const MARGIN_LEFT: usize = 10;
const MARGIN_RIGHT: usize = 10;
const MARGIN_GAP: usize = 24;

const BATTERY_SLOT_MAX_WIDTH: usize = 180;

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
    pub last_ws_render_width: usize,
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
            last_ws_render_width: 0,
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
        if !self.configured {
            return;
        }

        let surface = self.wl_surface.clone();
        let buffer = self.buffer.clone();

        if let (Some(surface), Some(buffer)) = (surface, buffer)
            && self.draw_and_damage(&surface)
        {
            surface.attach(Some(&buffer), 0, 0);
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

    fn battery_content_width(
        glyphs: &font_renderer::GlyphCache,
        percent: u8,
        state: u8,
        est_m_total: u16,
    ) -> usize {
        if state == 3 {
            return glyphs.full.width;
        }

        let mut w = Self::battery_percent_width(glyphs, percent);

        w += glyphs.percent.width;

        // 3px gap around the separator
        w += 3;
        w += if state == 2 {
            glyphs.plus.width
        } else {
            glyphs.minus.width
        };
        w += 3;

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

    fn battery_percent_width(glyphs: &font_renderer::GlyphCache, percent: u8) -> usize {
        if percent == 100 {
            glyphs.numbers[1].width + 1 + glyphs.numbers[0].width + 1 + glyphs.numbers[0].width + 1
        } else if percent >= 10 {
            glyphs.numbers[(percent / 10) as usize].width
                + 1
                + glyphs.numbers[(percent % 10) as usize].width
                + 1
        } else {
            glyphs.numbers[percent as usize].width + 1
        }
    }

    fn clear_slot(
        pixels: &mut [u8],
        stride: usize,
        surface_width: usize,
        surface_height: usize,
        start_x: usize,
        slot_width: usize,
    ) -> usize {
        if start_x >= surface_width || slot_width == 0 {
            return 0;
        }

        let clipped_width = slot_width.min(surface_width - start_x);
        for y in 0..surface_height {
            let start = y * stride + start_x * 4;
            let end = start + clipped_width * 4;
            pixels[start..end].fill(0);
        }
        clipped_width
    }

    fn draw_run(
        pixels: &mut [u8],
        stride: usize,
        start_x: usize,
        color: [u8; 4],
        glyphs_with_margin: &[(&font_renderer::RasterizedGlyph, usize)],
    ) {
        let mut current_x = start_x;
        for (glyph, extra_margin) in glyphs_with_margin {
            let y = (BAR_HEIGHT.saturating_sub(glyph.height)) / 2;
            Self::draw_glyph(pixels, stride, current_x, y, color, glyph);
            current_x += glyph.width + *extra_margin;
        }
    }

    fn workspace_label_width(glyphs: &font_renderer::GlyphCache, ws_num: usize) -> usize {
        if ws_num == 10 {
            glyphs.numbers[1].width + 1 + glyphs.numbers[0].width
        } else {
            glyphs.numbers[ws_num].width
        }
    }

    fn workspace_content_width(
        glyphs: &font_renderer::GlyphCache,
        workspaces: &[bool; 10],
        active_ws: u8,
    ) -> usize {
        let mut width = 0;
        for (i, ws) in workspaces.iter().enumerate() {
            let ws_num = i + 1;
            if *ws || active_ws == ws_num as u8 {
                width += Self::workspace_label_width(glyphs, ws_num) + 10;
            }
        }
        width
    }

    fn append_battery_percent_run<'a>(
        run: &mut Vec<(&'a font_renderer::RasterizedGlyph, usize)>,
        glyphs: &'a font_renderer::GlyphCache,
        percent: u8,
    ) {
        if percent == 100 {
            run.push((&glyphs.numbers[1], 1));
            run.push((&glyphs.numbers[0], 1));
            run.push((&glyphs.numbers[0], 1));
        } else if percent >= 10 {
            run.push((&glyphs.numbers[(percent / 10) as usize], 1));
            run.push((&glyphs.numbers[(percent % 10) as usize], 1));
        } else {
            run.push((&glyphs.numbers[percent as usize], 1));
        }
    }

    fn draw_and_damage(&mut self, surface: &WlSurface) -> bool {
        if self.pixels.is_null() || self.width == 0 || self.height == 0 {
            return false;
        }

        let surface_width = self.width as usize;
        let surface_height = self.height as usize;
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
            return false;
        }

        let mut something_damaged = false;

        if let Some(glyphs) = &self.glyphs {
            let color_time = [0xf7, 0xa6, 0xcb, 0xff];
            let color_ws_focused = [0xff, 0xff, 0xff, 0xff];
            let color_ws_other = [0xf7, 0xa6, 0xcb, 0xff];
            let color_date = [0xec, 0xc7, 0x74, 0xff];
            let color_bat = [0xa1, 0xe3, 0xa6, 0xff];

            let screen_center = surface_width / 2;

            // Slots define clear/damage areas
            let date_slot_width = (glyphs.max_digit_width * 6) + (glyphs.slash.width * 2) + 7;
            let time_slot_width = (glyphs.max_digit_width * 4)
                + glyphs.colon.width
                + glyphs.space.width
                + glyphs.max_ampm_width
                + 6;

            let date_slot_x = screen_center
                .saturating_sub(MARGIN_GAP / 2)
                .saturating_sub(date_slot_width);
            let time_slot_x = screen_center + (MARGIN_GAP / 2);

            let bat_slot_x = surface_width.saturating_sub(BATTERY_SLOT_MAX_WIDTH);

            if ws_changed {
                let current_ws_width =
                    Self::workspace_content_width(glyphs, &current_ws, active_ws);
                let ws_clear_width = (MARGIN_LEFT
                    + current_ws_width.max(self.last_ws_render_width))
                .min(surface_width);
                let cleared_width = Self::clear_slot(
                    slice,
                    stride,
                    surface_width,
                    surface_height,
                    0,
                    ws_clear_width,
                );

                let mut current_x = MARGIN_LEFT;
                for (i, ws) in current_ws.iter().enumerate() {
                    let ws_num = i + 1;
                    if *ws || active_ws == ws_num as u8 {
                        let color = if active_ws == ws_num as u8 {
                            color_ws_focused
                        } else {
                            color_ws_other
                        };

                        if ws_num == 10 {
                            let y_offset1 =
                                (BAR_HEIGHT.saturating_sub(glyphs.numbers[1].height)) / 2;
                            Self::draw_glyph(
                                slice,
                                stride,
                                current_x,
                                y_offset1,
                                color,
                                &glyphs.numbers[1],
                            );
                            current_x += glyphs.numbers[1].width + 1;

                            let y_offset0 =
                                (BAR_HEIGHT.saturating_sub(glyphs.numbers[0].height)) / 2;
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
                                (BAR_HEIGHT.saturating_sub(glyphs.numbers[ws_num].height)) / 2;
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

                surface.damage_buffer(0, 0, cleared_width as i32, surface_height as i32);
                something_damaged = true;

                self.last_active_ws = active_ws;
                self.last_workspaces = current_ws;
                self.last_ws_render_width = current_ws_width;
            }

            if date_changed && date_slot_x < surface_width {
                let cleared_width = Self::clear_slot(
                    slice,
                    stride,
                    surface_width,
                    surface_height,
                    date_slot_x,
                    date_slot_width,
                );

                let date_content_width = Self::date_content_width(glyphs, day, month, year);
                // Align to the RIGHT against the gap
                let start_x = screen_center
                    .saturating_sub(MARGIN_GAP / 2)
                    .saturating_sub(date_content_width);

                Self::draw_run(
                    slice,
                    stride,
                    start_x,
                    color_date,
                    &[
                        (&glyphs.numbers[(day / 10) as usize], 1),
                        (&glyphs.numbers[(day % 10) as usize], 1),
                        (&glyphs.slash, 1),
                        (&glyphs.numbers[(month / 10) as usize], 1),
                        (&glyphs.numbers[(month % 10) as usize], 1),
                        (&glyphs.slash, 1),
                        (&glyphs.numbers[(year / 10) as usize], 1),
                        (&glyphs.numbers[(year % 10) as usize], 0),
                    ],
                );

                surface.damage_buffer(
                    date_slot_x as i32,
                    0,
                    cleared_width as i32,
                    surface_height as i32,
                );
                something_damaged = true;

                self.last_day = day;
                self.last_month = month;
                self.last_year = year;
            }

            if clock_changed && time_slot_x < surface_width {
                let cleared_width = Self::clear_slot(
                    slice,
                    stride,
                    surface_width,
                    surface_height,
                    time_slot_x,
                    time_slot_width,
                );

                // Align to the LEFT against the gap
                let start_x = screen_center + (MARGIN_GAP / 2);

                let display_h = if h == 0 {
                    12
                } else if h > 12 {
                    h - 12
                } else {
                    h
                };
                let am_pm = if h >= 12 { &glyphs.pm } else { &glyphs.am };

                Self::draw_run(
                    slice,
                    stride,
                    start_x,
                    color_time,
                    &[
                        (&glyphs.numbers[(display_h / 10) as usize], 1),
                        (&glyphs.numbers[(display_h % 10) as usize], 1),
                        (&glyphs.colon, 1),
                        (&glyphs.numbers[(m / 10) as usize], 1),
                        (&glyphs.numbers[(m % 10) as usize], 1),
                        (&glyphs.space, 2),
                        (am_pm, 0),
                    ],
                );

                surface.damage_buffer(
                    time_slot_x as i32,
                    0,
                    cleared_width as i32,
                    surface_height as i32,
                );
                something_damaged = true;

                self.last_h = h;
                self.last_m = m;
            }

            if bat_changed && bat_slot_x < surface_width && bat_state != 255 {
                let cleared_width = Self::clear_slot(
                    slice,
                    stride,
                    surface_width,
                    surface_height,
                    bat_slot_x,
                    BATTERY_SLOT_MAX_WIDTH,
                );

                let bat_content_width =
                    Self::battery_content_width(glyphs, bat_percent, bat_state, bat_est_m_total);
                let start_x = surface_width.saturating_sub(MARGIN_RIGHT + bat_content_width);

                if bat_state == 3 {
                    Self::draw_run(slice, stride, start_x, color_bat, &[(&glyphs.full, 0)]);
                } else {
                    let est_h = (bat_est_m_total / 60) as u8;
                    let est_m = (bat_est_m_total % 60) as u8;
                    let state_glyph = if bat_state == 2 {
                        &glyphs.plus
                    } else {
                        &glyphs.minus
                    };

                    let mut run: Vec<(&font_renderer::RasterizedGlyph, usize)> =
                        Vec::with_capacity(10);
                    Self::append_battery_percent_run(&mut run, glyphs, bat_percent);
                    run.push((&glyphs.percent, 3));
                    run.push((state_glyph, 3));
                    run.push((&glyphs.numbers[(est_h / 10) as usize], 1));
                    run.push((&glyphs.numbers[(est_h % 10) as usize], 1));
                    run.push((&glyphs.colon, 1));
                    run.push((&glyphs.numbers[(est_m / 10) as usize], 1));
                    run.push((&glyphs.numbers[(est_m % 10) as usize], 0));

                    Self::draw_run(slice, stride, start_x, color_bat, &run);
                }

                surface.damage_buffer(
                    bat_slot_x as i32,
                    0,
                    cleared_width as i32,
                    surface_height as i32,
                );
                something_damaged = true;

                self.last_bat_percent = bat_percent;
                self.last_bat_state = bat_state;
                self.last_bat_est_m = bat_est_m_total;
            }

            self.force_full_redraw = false;
        }

        something_damaged
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
            if py >= BAR_HEIGHT {
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
