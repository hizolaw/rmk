//! A "gem"-style status renderer modelled on the popular
//! [`nice-view-gem`](https://github.com/M165437/nice-view-gem) ZMK shield.
//!
//! Targets the nice!view in **portrait**: a 68x160 logical surface (the panel
//! is physically 160x68, rotated). Layout, top to bottom:
//!
//! ```text
//!  ┌────────────┐
//!  │ SIG ▁▃▅█   │  signal bars + battery
//!  │  ████ 78%  │
//!  ├────────────┤
//!  │    WPM     │
//!  │     62     │
//!  │  ▂▃▅▆▇█▆▅  │  history chart
//!  ├────────────┤
//!  │┌──────────┐│
//!  ││  QWERTY  ││  layer name
//!  │└──────────┘│
//!  │  ● ○ ○ ○   │  profile dots
//!  └────────────┘
//! ```
//!
//! Drawn with `embedded-graphics` primitives and laid out from the surface
//! size, so it adapts instead of using hard-coded pixel offsets.

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
#[cfg(feature = "_ble")]
use rmk_types::ble::BleState;

const FG: MonoTextStyle<'_, BinaryColor> = MonoTextStyle::new(&FONT_5X8, BinaryColor::On);
const STROKE: PrimitiveStyle<BinaryColor> = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
const FILL: PrimitiveStyle<BinaryColor> = PrimitiveStyle::with_fill(BinaryColor::On);

/// WPM history length used by the bar chart.
const HISTORY: usize = 12;
/// Number of BLE profiles drawn as dots at the bottom.
const PROFILES: u8 = 4;

/// Gem-style status renderer for a portrait memory LCD.
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

        // Bands: header (signal+battery), body (WPM), footer (layer+profiles).
        let header_h = 22;
        let footer_h = 32;
        let body_top = header_h;
        let body_bottom = h - footer_h;
        let footer_top = body_bottom;

        self.draw_header(ctx, display, w, header_h);
        self.draw_wpm(ctx, display, w, body_top, body_bottom);
        self.draw_footer(ctx, display, w, h, footer_top);

        Line::new(Point::new(0, header_h), Point::new(w - 1, header_h))
            .into_styled(STROKE)
            .draw(display)
            .ok();
        Line::new(Point::new(0, footer_top), Point::new(w - 1, footer_top))
            .into_styled(STROKE)
            .draw(display)
            .ok();
    }
}

impl GemRenderer {
    /// "SIG" plus connection bars on the left, battery icon + percent on the right.
    fn draw_header<D: DrawTarget<Color = BinaryColor>>(
        &self,
        ctx: &RenderContext,
        display: &mut D,
        w: i32,
        header_h: i32,
    ) {
        Text::new("SIG", Point::new(1, 9), FG).draw(display).ok();
        self.draw_signal(ctx, display, header_h);

        self.draw_battery(ctx, display, w, header_h);
    }

    /// Four ascending bars; the number of filled bars reflects the link state.
    fn draw_signal<D: DrawTarget<Color = BinaryColor>>(
        &self,
        ctx: &RenderContext,
        display: &mut D,
        header_h: i32,
    ) {
        #[cfg(feature = "_ble")]
        let bars: u8 = match ctx.ble_status.state {
            BleState::Connected => 4,
            BleState::Advertising => 2,
            BleState::Inactive => 1,
        };
        #[cfg(not(feature = "_ble"))]
        let bars: u8 = 0;

        let _ = &ctx;
        let base_y = header_h - 3;
        for i in 0..4u8 {
            let bh = 3 + i as i32 * 2;
            let x = 26 + i as i32 * 4;
            let rect = Rectangle::new(Point::new(x, base_y - bh), Size::new(3, bh as u32));
            if i < bars {
                rect.into_styled(FILL).draw(display).ok();
            } else {
                rect.into_styled(STROKE).draw(display).ok();
            }
        }
    }

    /// Battery outline with proportional fill, a nub, the percentage, and a
    /// bolt when charging.
    fn draw_battery<D: DrawTarget<Color = BinaryColor>>(
        &self,
        ctx: &RenderContext,
        display: &mut D,
        w: i32,
        header_h: i32,
    ) {
        let (level, charging): (Option<u8>, bool) = {
            #[cfg(feature = "_ble")]
            {
                match &ctx.battery.0 {
                    BatteryStatus::Available { charge_state, level } => {
                        (*level, matches!(charge_state, ChargeState::Charging))
                    }
                    BatteryStatus::Unavailable => (None, false),
                }
            }
            #[cfg(not(feature = "_ble"))]
            {
                let _ = ctx;
                (None, false)
            }
        };

        let bw = 22;
        let bh = 9;
        let bx = w - bw - 5;
        let by = 2;

        Rectangle::new(Point::new(bx, by), Size::new(bw as u32, bh as u32))
            .into_styled(STROKE)
            .draw(display)
            .ok();
        Rectangle::new(Point::new(bx + bw, by + 3), Size::new(2, 3))
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
            let mut pct: heapless::String<5> = heapless::String::new();
            write!(pct, "{}%", level).ok();
            let tx = bx - pct.len() as i32 * 5 - 2;
            Text::new(&pct, Point::new(tx.max(1), by + 8), FG).draw(display).ok();
        }

        if charging {
            // Small bolt between the battery and its percentage.
            let cx = bx + bw / 2;
            Line::new(Point::new(cx, by + 2), Point::new(cx - 2, by + 5))
                .into_styled(STROKE)
                .draw(display)
                .ok();
            Line::new(Point::new(cx - 2, by + 5), Point::new(cx + 1, by + 5))
                .into_styled(STROKE)
                .draw(display)
                .ok();
            Line::new(Point::new(cx + 1, by + 5), Point::new(cx - 1, by + 8))
                .into_styled(STROKE)
                .draw(display)
                .ok();
        }
    }

    /// "WPM", the current value, and a history bar chart.
    fn draw_wpm<D: DrawTarget<Color = BinaryColor>>(
        &self,
        ctx: &RenderContext,
        display: &mut D,
        w: i32,
        top: i32,
        bottom: i32,
    ) {
        Text::new("WPM", Point::new(1, top + 10), FG).draw(display).ok();

        let mut val: heapless::String<6> = heapless::String::new();
        write!(val, "{}", ctx.wpm).ok();
        let vx = (w - val.len() as i32 * 5 - 1).max(1);
        Text::new(&val, Point::new(vx, top + 10), FG).draw(display).ok();

        // Bar chart of recent WPM below the value.
        let chart_top = top + 14;
        let chart_bottom = bottom - 2;
        let chart_h = (chart_bottom - chart_top).max(1);
        let max = self.history_iter().max().unwrap_or(1).max(1) as i32;
        let slot = (w - 2) / HISTORY as i32;
        let bar_w = (slot - 1).max(1);
        let start = (self.hist_pos + HISTORY - self.hist_len) % HISTORY;
        for i in 0..self.hist_len {
            let v = self.wpm_history[(start + i) % HISTORY] as i32;
            let bh = (v * chart_h / max).max(0);
            if bh == 0 {
                continue;
            }
            let x = 1 + i as i32 * slot;
            Rectangle::new(Point::new(x, chart_bottom - bh), Size::new(bar_w as u32, bh as u32))
                .into_styled(FILL)
                .draw(display)
                .ok();
        }
    }

    /// Layer name in a rounded box, with profile dots underneath.
    fn draw_footer<D: DrawTarget<Color = BinaryColor>>(
        &self,
        ctx: &RenderContext,
        display: &mut D,
        w: i32,
        h: i32,
        top: i32,
    ) {
        // Layer name box.
        let box_h = 13;
        let layer_box = Rectangle::new(Point::new(1, top + 2), Size::new((w - 2) as u32, box_h as u32));
        layer_box.into_styled(STROKE).draw(display).ok();

        let mut lyr: heapless::String<10> = heapless::String::new();
        write!(lyr, "L{}", ctx.layer).ok();
        let lx = (w - lyr.len() as i32 * 5) / 2;
        Text::new(&lyr, Point::new(lx.max(1), top + 11), FG)
            .draw(display)
            .ok();

        // Profile dots: PROFILES circles, the active one filled.
        #[cfg(feature = "_ble")]
        let active = ctx.ble_status.profile;
        #[cfg(not(feature = "_ble"))]
        let active = 0u8;

        let dot = 5;
        let gap = 3;
        let total = PROFILES as i32 * dot + (PROFILES as i32 - 1) * gap;
        let mut x = (w - total) / 2;
        let y = top + box_h + 5;
        for i in 0..PROFILES {
            let r = Rectangle::new(Point::new(x, y), Size::new(dot as u32, dot as u32));
            if i == active {
                r.into_styled(FILL).draw(display).ok();
            } else {
                r.into_styled(STROKE).draw(display).ok();
            }
            x += dot + gap;
        }
        let _ = h;
    }
}
