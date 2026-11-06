//! A "gem"-style status renderer, inspired by the popular
//! [`nice-view-gem`](https://github.com/M165437/nice-view-gem) ZMK shield.
//!
//! It is written for the nice!view in **portrait** (a 68x160 logical surface,
//! i.e. `rotation = 90`): the panel is physically 160x68, turned on its side.
//! Layout, top to bottom:
//!
//! ```text
//!  ┌──────────┐
//!  │ SIG  ███ │  connection / battery
//!  ├──────────┤
//!  │  BAT 78% │
//!  │          │
//!  │   WPM    │
//!  │   62     │
//!  │  ▁▂▃▅▇   │  sparkline
//!  ├──────────┤
//!  │ PROFILE  │
//!  │   2      │
//!  │ QWERTY   │  layer
//!  └──────────┘
//! ```
//!
//! Everything is drawn with `embedded-graphics` primitives, so it works with
//! any monochrome `DisplayDriver`; the layout keys off the surface size rather
//! than hard-coded offsets.

use core::fmt::Write as _;

use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::mono_font::ascii::FONT_5X8;
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Line, PrimitiveStyle, Rectangle};
use embedded_graphics::text::Text;

use crate::display::{DisplayRenderer, RenderContext};
#[cfg(feature = "_ble")]
use rmk_types::battery::{BatteryStatus, ChargeState};

const FG: MonoTextStyle<'_, BinaryColor> = MonoTextStyle::new(&FONT_5X8, BinaryColor::On);
const STROKE: PrimitiveStyle<BinaryColor> = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
const FILL: PrimitiveStyle<BinaryColor> = PrimitiveStyle::with_fill(BinaryColor::On);

/// WPM history used by the sparkline. Sized small to keep RAM low.
const HISTORY: usize = 12;

/// Gem-style renderer with a WPM sparkline and battery/connection header.
#[derive(Default)]
pub struct GemRenderer {
    wpm_history: [u8; HISTORY],
    hist_len: usize,
    hist_pos: usize,
}

impl GemRenderer {
    fn push_wpm(&mut self, wpm: u16) {
        let v = wpm.min(u8::MAX as u16) as u8;
        self.wpm_history[self.hist_pos] = v;
        self.hist_pos = (self.hist_pos + 1) % HISTORY;
        if self.hist_len < HISTORY {
            self.hist_len += 1;
        }
    }

    fn history_iter(&self) -> impl Iterator<Item = u8> + '_ {
        let start = (self.hist_pos + HISTORY - self.hist_len) % HISTORY;
        (0..self.hist_len).map(move |i| self.wpm_history[(start + i) % HISTORY])
    }
}

impl DisplayRenderer<BinaryColor> for GemRenderer {
    fn render<D: DrawTarget<Color = BinaryColor>>(&mut self, ctx: &RenderContext, display: &mut D) {
        let bbox = display.bounding_box();
        let w = bbox.size.width as i32;
        let h = bbox.size.height as i32;

        display.clear(BinaryColor::Off).ok();
        if ctx.sleeping {
            return;
        }

        self.push_wpm(ctx.wpm);

        // Split vertically into three bands.
        let top_h = 20;
        let mid_h = (h - top_h) / 2;
        let sep1 = top_h;
        let sep2 = top_h + mid_h;

        // --- Top: signal label + battery ---
        Text::new("SIG", Point::new(2, 10), FG).draw(display).ok();
        self.draw_battery(ctx, display, w, top_h);

        // --- Middle: WPM value + sparkline ---
        let mut buf: heapless::String<8> = heapless::String::new();
        write!(buf, "{}", ctx.wpm).ok();
        // Right-align the number.
        let num_x = (w - buf.len() as i32 * 5).max(2);
        Text::new(&buf, Point::new(num_x, sep1 + 16), FG).draw(display).ok();
        Text::new("WPM", Point::new(2, sep1 + 16), FG).draw(display).ok();
        self.draw_sparkline(display, w, sep1 + 20, sep2);

        // --- Bottom: profile + layer ---
        #[cfg(feature = "_ble")]
        {
            let mut p: heapless::String<8> = heapless::String::new();
            write!(p, "P{}", ctx.ble_status.profile).ok();
            Text::new(&p, Point::new(2, sep2 + 12), FG).draw(display).ok();
            let (dot_x, dot_y) = (w - 8, sep2 + 7);
            if matches!(
                ctx.ble_status.state,
                rmk_types::ble::BleState::Connected
            ) {
                Rectangle::new(Point::new(dot_x, dot_y), Size::new(5, 5))
                    .into_styled(FILL)
                    .draw(display)
                    .ok();
            } else {
                Rectangle::new(Point::new(dot_x, dot_y), Size::new(5, 5))
                    .into_styled(STROKE)
                    .draw(display)
                    .ok();
            }
        }

        let mut lyr: heapless::String<12> = heapless::String::new();
        write!(lyr, "L{}", ctx.layer).ok();
        Text::new(&lyr, Point::new(2, h - 4), FG).draw(display).ok();

        // Separators.
        Line::new(Point::new(0, sep1), Point::new(w - 1, sep1))
            .into_styled(STROKE)
            .draw(display)
            .ok();
        Line::new(Point::new(0, sep2), Point::new(w - 1, sep2))
            .into_styled(STROKE)
            .draw(display)
            .ok();
    }
}

impl GemRenderer {
    #[cfg(feature = "_ble")]
    fn draw_battery<D: DrawTarget<Color = BinaryColor>>(
        &self,
        ctx: &RenderContext,
        display: &mut D,
        w: i32,
        top_h: i32,
    ) {
        let (level, charging) = match &ctx.battery.0 {
            BatteryStatus::Available { charge_state, level } => {
                (*level, matches!(charge_state, ChargeState::Charging))
            }
            BatteryStatus::Unavailable => (None, false),
        };

        // Battery icon: a small rectangle with a fill proportional to level.
        let bw = 26;
        let bh = 10;
        let bx = w - bw - 4;
        let by = (top_h - bh) / 2;

        Rectangle::new(Point::new(bx, by), Size::new(bw as u32, bh as u32))
            .into_styled(STROKE)
            .draw(display)
            .ok();
        // Nub.
        Rectangle::new(Point::new(bx + bw, by + 3), Size::new(2, 4))
            .into_styled(FILL)
            .draw(display)
            .ok();

        if let Some(level) = level {
            let inner_w = ((bw - 4) as u32 * level as u32) / 100;
            if inner_w > 0 {
                Rectangle::new(Point::new(bx + 2, by + 2), Size::new(inner_w, (bh - 4) as u32))
                    .into_styled(FILL)
                    .draw(display)
                    .ok();
            }
        }
        // A charging bolt: a short vertical bar when charging.
        if charging {
            Rectangle::new(Point::new(bx + bw / 2 - 1, by + 2), Size::new(2, (bh - 4) as u32))
                .into_styled(STROKE)
                .draw(display)
                .ok();
        }
    }

    #[cfg(not(feature = "_ble"))]
    fn draw_battery<D: DrawTarget<Color = BinaryColor>>(
        &self,
        _ctx: &RenderContext,
        _display: &mut D,
        _w: i32,
        _top_h: i32,
    ) {
    }

    fn draw_sparkline<D: DrawTarget<Color = BinaryColor>>(
        &self,
        display: &mut D,
        w: i32,
        top: i32,
        bottom: i32,
    ) {
        let max = self.history_iter().max().unwrap_or(1).max(1) as i32;
        let n = self.hist_len;
        if n < 2 {
            return;
        }
        let height = (bottom - top - 3).max(1);
        let step = ((w - 4) / (HISTORY as i32 - 1)).max(1);
        let mut prev: Option<Point> = None;
        for (i, v) in self.history_iter().enumerate() {
            let x = 2 + i as i32 * step;
            let y = bottom - 2 - (v as i32 * height / max);
            let p = Point::new(x, y);
            if let Some(q) = prev {
                Line::new(q, p).into_styled(STROKE).draw(display).ok();
            } else {
                Rectangle::new(p, Size::new(2, 2)).into_styled(FILL).draw(display).ok();
            }
            prev = Some(p);
        }
    }
}
