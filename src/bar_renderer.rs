use std::sync::atomic::Ordering;

use crate::{
    BATTERY_MASK, DATE_MASK, TIME_MASK, WORKSPACES_MASK,
    font::{FontAtlas, GlyphId},
    unpack_battery, unpack_date, unpack_time, unpack_workspaces,
};

// Colors are 0xAARRGGBB
const COLOR_WS_FOCUSED: u32 = 0xffffffff;
const COLOR_WS_OPEN: u32 = 0xffcba6f7;
const COLOR_TIME: u32 = 0xffcba6f7;
const COLOR_DATE: u32 = 0xff74c7ec;
const COLOR_BAT: u32 = 0xffa6e3a1;

const MARGIN_LEFT: usize = 10;
const MARGIN_RIGHT: usize = 10;
const MARGIN_GAP: usize = 24;

const BATTERY_SLOT_MAX_WIDTH: usize = 180;

pub struct BarRenderer {
    pub atlas: FontAtlas,
    cached_ws_mask: u16,
    cached_ws_render_width: usize,
    cached_time_mask: u16,
    cached_date_mask: u32,
    cached_battery_mask: u32,
}

impl BarRenderer {
    pub fn new(atlas: FontAtlas) -> Self {
        Self {
            atlas,
            cached_ws_mask: 0,
            cached_ws_render_width: 0,
            cached_time_mask: 0,
            cached_date_mask: 0,
            cached_battery_mask: 0,
        }
    }

    pub fn render_frame(
        &mut self,
        pixels: &mut [u32],
        width: usize,
        height: usize,
        force_redraw: bool,
        mut damage_fn: impl FnMut(usize, usize),
    ) -> bool {
        let ws_mask = WORKSPACES_MASK.load(Ordering::Relaxed);
        let time_mask = TIME_MASK.load(Ordering::Relaxed);
        let date_mask = DATE_MASK.load(Ordering::Relaxed);
        let bat_mask = BATTERY_MASK.load(Ordering::Relaxed);

        let ws_changed = force_redraw || ws_mask != self.cached_ws_mask;
        let clock_changed = force_redraw || time_mask != self.cached_time_mask;
        let date_changed = force_redraw || date_mask != self.cached_date_mask;
        let bat_changed = force_redraw || bat_mask != self.cached_battery_mask;

        if !ws_changed && !clock_changed && !date_changed && !bat_changed {
            return false;
        }

        if ws_changed {
            self.draw_workspaces(pixels, width, height, ws_mask, &mut damage_fn);
        }

        let center = width / 2;
        if date_changed {
            self.draw_date(pixels, width, height, center, date_mask, &mut damage_fn);
        }

        if clock_changed {
            self.draw_clock(pixels, width, height, center, time_mask, &mut damage_fn);
        }

        if bat_mask != 0 && bat_changed {
            self.draw_battery(pixels, width, height, bat_mask, &mut damage_fn);
        }

        true
    }

    fn clear_rect(pixels: &mut [u32], stride: usize, height: usize, x: usize, width: usize) {
        if x >= stride || width == 0 {
            return;
        }
        let actual_w = width.min(stride - x);
        for row in pixels.chunks_exact_mut(stride).take(height) {
            row[x..x + actual_w].fill(0);
        }
    }

    fn draw_workspaces(
        &mut self,
        pixels: &mut [u32],
        stride: usize,
        height: usize,
        ws_mask: u16,
        damage_fn: &mut impl FnMut(usize, usize),
    ) {
        let (active_ws, mask) = unpack_workspaces(ws_mask);
        let mut total_width = 0;
        for num in 1..=10 {
            if (mask & (1 << (num - 1))) != 0 || active_ws == (num as u8) {
                let (digits, len) = FontAtlas::format_num(num, 1);
                total_width += self.atlas.measure_formatted_digits(&digits, len, 1) + 10;
            }
        }

        let old_width = self.cached_ws_render_width;
        self.cached_ws_mask = ws_mask;
        self.cached_ws_render_width = total_width;

        let clear_w = old_width.max(total_width);
        Self::clear_rect(pixels, stride, height, 0, clear_w);
        damage_fn(0, clear_w);

        let mut cursor_x = MARGIN_LEFT;
        for num in 1..=10 {
            let active = active_ws == (num as u8);
            if (mask & (1 << (num - 1))) != 0 || active {
                let color = if active {
                    COLOR_WS_FOCUSED
                } else {
                    COLOR_WS_OPEN
                };
                let (digits, len) = FontAtlas::format_num(num, 1);
                self.atlas.draw_formatted_digits(
                    pixels,
                    stride,
                    height,
                    &mut cursor_x,
                    &digits,
                    len,
                    color,
                    1,
                );
                cursor_x += 10;
            }
        }
    }

    fn draw_date(
        &mut self,
        pixels: &mut [u32],
        stride: usize,
        height: usize,
        center: usize,
        date_mask: u32,
        damage_fn: &mut impl FnMut(usize, usize),
    ) {
        let (day, month, year) = unpack_date(date_mask);

        let max_width = self.atlas.date_slot_max_width;
        let slot_x = center
            .saturating_sub(MARGIN_GAP / 2)
            .saturating_sub(max_width);
        Self::clear_rect(pixels, stride, height, slot_x, max_width);
        damage_fn(slot_x, max_width);

        let (day_d, day_l) = FontAtlas::format_num(day, 2);
        let (mon_d, mon_l) = FontAtlas::format_num(month, 2);
        let (yr_d, yr_l) = FontAtlas::format_num(year, 2);

        let slash_g = self.atlas.get_metrics(GlyphId::Slash);
        let content_width = self.atlas.measure_formatted_digits(&day_d, day_l, 1)
            + 1
            + slash_g.width
            + 1
            + self.atlas.measure_formatted_digits(&mon_d, mon_l, 1)
            + 1
            + slash_g.width
            + 1
            + self.atlas.measure_formatted_digits(&yr_d, yr_l, 0);

        let mut cursor_x = center
            .saturating_sub(MARGIN_GAP / 2)
            .saturating_sub(content_width);
        let color = COLOR_DATE;

        self.atlas.draw_formatted_digits(
            pixels,
            stride,
            height,
            &mut cursor_x,
            &day_d,
            day_l,
            color,
            1,
        );
        cursor_x += 1;

        let slash_y = (height.saturating_sub(slash_g.height)) / 2;
        self.atlas
            .draw_glyph(pixels, stride, height, cursor_x, slash_y, slash_g, color);
        cursor_x += slash_g.width + 1;

        self.atlas.draw_formatted_digits(
            pixels,
            stride,
            height,
            &mut cursor_x,
            &mon_d,
            mon_l,
            color,
            1,
        );
        cursor_x += 1;

        self.atlas
            .draw_glyph(pixels, stride, height, cursor_x, slash_y, slash_g, color);
        cursor_x += slash_g.width + 1;

        self.atlas.draw_formatted_digits(
            pixels,
            stride,
            height,
            &mut cursor_x,
            &yr_d,
            yr_l,
            color,
            0,
        );

        self.cached_date_mask = date_mask;
    }

    fn draw_clock(
        &mut self,
        pixels: &mut [u32],
        stride: usize,
        height: usize,
        center: usize,
        time_mask: u16,
        damage_fn: &mut impl FnMut(usize, usize),
    ) {
        let (hour, minute) = unpack_time(time_mask);

        let max_width = self.atlas.clock_slot_max_width;
        let slot_x = center + (MARGIN_GAP / 2);
        Self::clear_rect(pixels, stride, height, slot_x, max_width);
        damage_fn(slot_x, max_width);

        let mut cursor_x = slot_x;
        let color = COLOR_TIME;
        let hour_12 = match hour {
            0 => 12,
            13..=23 => hour - 12,
            _ => hour,
        };

        let (h_d, h_l) = FontAtlas::format_num(hour_12, 2);
        let (m_d, m_l) = FontAtlas::format_num(minute, 2);

        self.atlas.draw_formatted_digits(
            pixels,
            stride,
            height,
            &mut cursor_x,
            &h_d,
            h_l,
            color,
            1,
        );
        cursor_x += 1;

        let colon_g = self.atlas.get_metrics(GlyphId::Colon);
        let colon_y = (height.saturating_sub(colon_g.height)) / 2;
        self.atlas
            .draw_glyph(pixels, stride, height, cursor_x, colon_y, colon_g, color);
        cursor_x += colon_g.width + 1;

        self.atlas.draw_formatted_digits(
            pixels,
            stride,
            height,
            &mut cursor_x,
            &m_d,
            m_l,
            color,
            1,
        );
        cursor_x += 1;

        let space_g = self.atlas.get_metrics(GlyphId::Space);
        cursor_x += space_g.width + 1;

        let ampm_id = if hour >= 12 { GlyphId::Pm } else { GlyphId::Am };
        let ampm_g = self.atlas.get_metrics(ampm_id);
        let ampm_y = (height.saturating_sub(ampm_g.height)) / 2;
        self.atlas
            .draw_glyph(pixels, stride, height, cursor_x, ampm_y, ampm_g, color);

        self.cached_time_mask = time_mask;
    }

    fn draw_battery(
        &mut self,
        pixels: &mut [u32],
        stride: usize,
        height: usize,
        battery_mask: u32,
        damage_fn: &mut impl FnMut(usize, usize),
    ) {
        let (percent, state, estimate) = unpack_battery(battery_mask);
        let slot_x = stride.saturating_sub(BATTERY_SLOT_MAX_WIDTH);
        Self::clear_rect(pixels, stride, height, slot_x, BATTERY_SLOT_MAX_WIDTH);
        damage_fn(slot_x, BATTERY_SLOT_MAX_WIDTH);

        let color = COLOR_BAT;
        if state == 3 {
            let full_g = self.atlas.get_metrics(GlyphId::Full);
            let cursor_x = stride.saturating_sub(MARGIN_RIGHT + full_g.width);
            let y = (height.saturating_sub(full_g.height)) / 2;
            self.atlas
                .draw_glyph(pixels, stride, height, cursor_x, y, full_g, color);
        } else {
            let est_h = estimate / 60;
            let est_m = estimate % 60;

            let (pct_d, pct_l) = FontAtlas::format_num(percent, 1);
            let (h_d, h_l) = FontAtlas::format_num(est_h, 2);
            let (m_d, m_l) = FontAtlas::format_num(est_m, 2);

            let pct_g = self.atlas.get_metrics(GlyphId::Percent);
            let status_id = if state == 2 {
                GlyphId::Plus
            } else {
                GlyphId::Minus
            };
            let status_g = self.atlas.get_metrics(status_id);
            let colon_g = self.atlas.get_metrics(GlyphId::Colon);

            let content_width = self.atlas.measure_formatted_digits(&pct_d, pct_l, 1)
                + 1
                + pct_g.width
                + 3
                + status_g.width
                + 3
                + self.atlas.measure_formatted_digits(&h_d, h_l, 1)
                + 1
                + colon_g.width
                + 1
                + self.atlas.measure_formatted_digits(&m_d, m_l, 0);

            let mut cursor_x = stride.saturating_sub(MARGIN_RIGHT + content_width);

            self.atlas.draw_formatted_digits(
                pixels,
                stride,
                height,
                &mut cursor_x,
                &pct_d,
                pct_l,
                color,
                1,
            );
            cursor_x += 1;

            let pct_y = (height.saturating_sub(pct_g.height)) / 2;
            self.atlas
                .draw_glyph(pixels, stride, height, cursor_x, pct_y, pct_g, color);
            cursor_x += pct_g.width + 3;

            let status_y = (height.saturating_sub(status_g.height)) / 2;
            self.atlas
                .draw_glyph(pixels, stride, height, cursor_x, status_y, status_g, color);
            cursor_x += status_g.width + 3;

            self.atlas.draw_formatted_digits(
                pixels,
                stride,
                height,
                &mut cursor_x,
                &h_d,
                h_l,
                color,
                1,
            );
            cursor_x += 1;

            let colon_y = (height.saturating_sub(colon_g.height)) / 2;
            self.atlas
                .draw_glyph(pixels, stride, height, cursor_x, colon_y, colon_g, color);
            cursor_x += colon_g.width + 1;

            self.atlas.draw_formatted_digits(
                pixels,
                stride,
                height,
                &mut cursor_x,
                &m_d,
                m_l,
                color,
                0,
            );
        }
        self.cached_battery_mask = battery_mask;
    }
}
