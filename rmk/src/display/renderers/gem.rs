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

/// Number of BLE profiles drawn as dots at the bottom.
const PROFILES: u8 = crate::NUM_BLE_PROFILE as u8;

/// WPM samples kept for the history chart. At 3px per bar plus a 2px gap,
/// 12 bars fit the 68px-wide portrait surface with margins.
const WPM_HIST: usize = 12;

/// Gem-style status renderer for a portrait memory LCD.
///
/// Keeps a short WPM history for the body-band chart. Renders are triggered
/// by state changes (a WPM update is one), rate-limited by the processor, so
/// while typing the samples land at roughly the render cadence.
#[derive(Default)]
pub struct GemRenderer {
    /// Oldest-to-newest WPM samples; the first `wpm_len` entries are valid.
    wpm_hist: [u8; WPM_HIST],
    wpm_len: usize,
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
        let _ = body_bottom;
        self.draw_wpm(ctx, display, w, body_top);
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
    /// Record a WPM sample, dropping the oldest once the window is full.
    /// Unchanged values are not repeated, so the chart reflects typing
    /// bursts rather than wall-clock time.
    fn push_wpm(&mut self, wpm: u16) {
        let wpm = wpm.min(u8::MAX as u16) as u8;
        if self.wpm_len > 0 && self.wpm_hist[self.wpm_len - 1] == wpm {
            return;
        }
        if self.wpm_len == WPM_HIST {
            self.wpm_hist.copy_within(1.., 0);
            self.wpm_len -= 1;
        }
        self.wpm_hist[self.wpm_len] = wpm;
        self.wpm_len += 1;
    }

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
        _header_h: i32,
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
    ) {
        Text::new("WPM", Point::new(1, top + 10), FG).draw(display).ok();

        let mut val: heapless::String<6> = heapless::String::new();
        write!(val, "{}", ctx.wpm).ok();
        let vx = (w - val.len() as i32 * 5 - 1).max(1);
        Text::new(&val, Point::new(vx, top + 10), FG).draw(display).ok();

        // History chart: one bar per sample, oldest on the left, scaled to
        // the largest sample in the window.
        let hist = &self.wpm_hist[..self.wpm_len];
        let max = hist.iter().copied().max().unwrap_or(0) as i32;
        if max == 0 {
            return;
        }
        let chart_top = top + 16;
        let chart_h = 18;
        let bar_w = 3;
        let gap = 2;
        let total = hist.len() as i32 * (bar_w + gap) - gap;
        let mut x = ((w - total) / 2).max(1);
        for &v in hist {
            let bh = v as i32 * chart_h / max;
            if bh > 0 {
                Rectangle::new(Point::new(x, chart_top + chart_h - bh), Size::new(bar_w as u32, bh as u32))
                    .into_styled(FILL)
                    .draw(display)
                    .ok();
            }
            x += bar_w + gap;
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
