//! [`DisplayDriver`] adapter for Sharp memory LCDs over SPI.
//!
//! The nice!view is a 160x68 Sharp LS0xx panel, one bit per pixel. The wire
//! protocol mirrors the Rust `memory-lcd-spi` driver (the one known to work
//! with this panel) with Zephyr's command bit values:
//!
//! * every command byte carries the VCOM polarity bit, which flips each time;
//! * each gate line is `[line_number, W/8 data bytes, dummy_byte]`, line
//!   numbers from 1, dummy byte 27;
//! * a trailing write byte latches the last line.
//!
//! VCOM is carried by the real frame writes. There is intentionally **no**
//! standalone VCOM ping: a bare VCOM command with no pixel data made the panel
//! shimmer and fade, because this driver repaints only on state changes rather
//! than continuously like Zephyr's LVGL loop.

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_hal::digital::OutputPin;
use embedded_hal_async::spi::SpiBus;

use crate::display::DisplayDriver;

// LS0xx command bits, from Zephyr's ls0xx driver.
const BIT_WRITECMD: u8 = 0x01;
const BIT_VCOM: u8 = 0x02;
const BIT_CLEAR: u8 = 0x04;

/// Trailing byte after each gate line's pixels, exactly as Zephyr sends it.
const LINE_DUMMY: u8 = 27;

/// Bytes of pixel data per gate line: one bit per pixel, `W` is a multiple of 8.
const fn bytes_per_line<const W: usize>() -> usize {
    W >> 3
}

/// A Sharp memory LCD over an async SPI bus.
///
/// `W`/`H` are the panel resolution; `FB` is `W * H / 8` (the framebuffer
/// length), passed explicitly because stable Rust does not allow const
/// arithmetic on generic parameters.
pub struct Ls0xxDisplay<SPI, CS, const W: usize, const H: usize, const FB: usize> {
    spi: SPI,
    cs: CS,
    fb: [u8; FB],
    shown: [u8; FB],
    /// Single VCOM polarity, exactly like Zephyr's `ls0xx_data.vcom_state`:
    /// it flips on *every* command byte sent (clear, frame start, frame end,
    /// idle ping). Keeping one shared flag is what makes the AC drive correct;
    /// pixels only latch their new state when consecutive writes see opposite
    /// polarity, and a separate flag per path broke that parity.
    vcom_state: bool,
}

impl<SPI, CS, const W: usize, const H: usize, const FB: usize> Ls0xxDisplay<SPI, CS, W, H, FB>
where
    SPI: SpiBus<u8>,
    CS: OutputPin,
{
    pub fn new(spi: SPI, cs: CS) -> Self {
        Self {
            spi,
            cs,
            fb: [0; FB],
            shown: [0xFF; FB],
            vcom_state: false,
        }
    }

    /// Zephyr's `ls0xx_cmd`: OR the current VCOM polarity into the command
    /// byte, then flip it. Every command goes through here so the polarity
    /// alternates uniformly.
    fn next_cmd(&mut self, base: u8) -> u8 {
        let byte = if self.vcom_state { base | BIT_VCOM } else { base };
        self.vcom_state = !self.vcom_state;
        byte
    }

    /// Zephyr `ls0xx_clear`: `[0x04 | vcom, 0x00]`.
    async fn clear_panel(&mut self) -> Result<(), SPI::Error> {
        let cmd = self.next_cmd(BIT_CLEAR);
        self.cs.set_high().ok();
        let result = self.spi.write(&[cmd, 0x00]).await;
        self.cs.set_low().ok();
        result
    }

    /// Zephyr `ls0xx_update_display`: start command, one buffer per gate line
    /// `[line, pixels, dummy]`, then a trailing command that latches the last
    /// line. Commands carry VCOM, the per-line buffers do not.
    async fn write_frame(&mut self) -> Result<(), SPI::Error> {
        let per_line = bytes_per_line::<W>();
        let mut line = [0u8; 1 + 32 + 1];

        self.cs.set_high().ok();

        // Alternate the VCOM bit every frame: this is the AC drive a Sharp
        // memory LCD needs, and without it the contrast fades (washed out).
        // The panel does NOT invert the data when VCOM changes - it simply
        // latches the bits as sent - so the pixel data must be identical on
        // every frame. Keeping the data constant while VCOM alternates gives a
        // stable image with a proper AC field; flipping the data along with
        // VCOM made the screen flash black/white.
        //
        // Pixel convention on the wire: 1 = white, 0 = black. `fb` has bit 1
        // for "On" (drawn content), so `!b` renders white background with
        // black content on every frame.
        let vcom = self.vcom_state;
        self.vcom_state = !vcom;

        let start = if vcom { BIT_WRITECMD | BIT_VCOM } else { BIT_WRITECMD };
        self.spi.write(&[start]).await?;

        for y in 0..H {
            line[0] = (y + 1) as u8; // gate address is 1-based
            let src = &self.fb[y * per_line..(y + 1) * per_line];
            for (dst, &b) in line[1..1 + per_line].iter_mut().zip(src) {
                // Constant on every frame: white background / black content.
                *dst = !b;
            }
            line[1 + per_line] = LINE_DUMMY;
            self.spi.write(&line[..2 + per_line]).await?;
        }

        // Latch the last line; no VCOM here so the frame flips polarity once.
        self.spi.write(&[BIT_WRITECMD]).await?;

        self.cs.set_low().ok();
        Ok(())
    }

    // A standalone VCOM ping is deliberately NOT provided. Sending a bare
    // VCOM command with no pixel data (Zephyr does this from its own thread)
    // made the nice!view shimmer and fade here: that driver repaints whole
    // frames continuously through LVGL, while this one only repaints on state
    // changes. `memory-lcd-spi`, the driver that works with this panel in
    // Rust, likewise never pings - VCOM is carried by each real frame.
}

impl<SPI, CS, const W: usize, const H: usize, const FB: usize> OriginDimensions
    for Ls0xxDisplay<SPI, CS, W, H, FB>
{
    fn size(&self) -> Size {
        Size::new(W as u32, H as u32)
    }
}

impl<SPI, CS, const W: usize, const H: usize, const FB: usize> DrawTarget
    for Ls0xxDisplay<SPI, CS, W, H, FB>
{
    type Color = BinaryColor;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<BinaryColor>>,
    {
        for Pixel(pt, color) in pixels {
            if pt.x < 0 || pt.y < 0 || pt.x >= W as i32 || pt.y >= H as i32 {
                continue;
            }
            let (x, y) = (pt.x as usize, pt.y as usize);
            let index = y * bytes_per_line::<W>() + x / 8;
            // Byte bits are filled MSB-in-value; the bus is LSB-first (see
            // expand_spi_init), so this byte reverses to keep pixel order.
            let mask = 0x01 << (x % 8);
            match color {
                BinaryColor::On => self.fb[index] |= mask,
                BinaryColor::Off => self.fb[index] &= !mask,
            }
        }
        Ok(())
    }
}

impl<SPI, CS, const W: usize, const H: usize, const FB: usize> DisplayDriver
    for Ls0xxDisplay<SPI, CS, W, H, FB>
where
    SPI: SpiBus<u8>,
    CS: OutputPin,
{
    async fn init(&mut self) {
        // Wipe whatever the previous firmware left, then paint an empty frame.
        let _ = self.clear_panel().await;
        embassy_time::Timer::after_millis(10).await;

        self.fb.fill(0);
        for _ in 0..3 {
            let _ = self.write_frame().await;
            embassy_time::Timer::after_millis(5).await;
        }
        self.shown.copy_from_slice(&self.fb);
    }

    async fn flush(&mut self) {
        // Only touch the panel when the rendered image actually differs from
        // what is already on it (see `shown`).
        if self.fb != self.shown {
            let _ = self.write_frame().await;
            self.shown.copy_from_slice(&self.fb);
        }
    }

    async fn tick(&mut self) {
        // A memory LCD only holds its image while it keeps being driven with
        // alternating VCOM. A bare VCOM ping (no pixel data) shimmered, and
        // repainting every 33ms starved the BLE stack. So repaint the whole
        // frame on a slow timer (set via with_vcom_interval) - frequent enough
        // to hold contrast, cheap enough not to disturb the radio.
        let _ = self.write_frame().await;
        self.shown.copy_from_slice(&self.fb);
    }
}
