//! [`DisplayDriver`] adapter for Sharp memory LCDs over SPI.
//!
//! The nice!view is a 160x68 Sharp LS0xx panel, one bit per pixel. The wire
//! protocol follows Zephyr's `ls0xx` driver (proven on this exact panel under
//! ZMK):
//!
//! * every command byte carries the VCOM polarity bit, which flips each time
//!   (`ls0xx_cmd`: `buf[0] |= vcom_state ? LS0XX_BIT_VCOM : 0`);
//! * pixel data is never inverted — "high/1 means white, low/0 means black"
//!   on the wire, independent of VCOM;
//! * each gate line is `[line_number, W/8 data bytes, dummy_byte]`, line
//!   numbers from 1, dummy byte 27;
//! * a trailing write byte latches the last line.
//!
//! VCOM maintenance: a memory LCD needs its drive polarity toggled at >= 1 Hz
//! per the datasheet or DC bias slowly damages the panel. That is *all* the
//! VCOM bit does — it is not part of the pixel data. [`DisplayDriver::tick`]
//! sends Zephyr's bare maintenance command, `[vcom_bit, 0x00]`, which keeps
//! the polarity alternating without touching a single gate line. ZMK runs this
//! at 30 Hz on the nice!view and it is invisible.

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

/// Whether a rotation swaps the logical width and height.
const fn rotation_swaps(rotation: u16) -> bool {
    matches!(rotation, 90 | 270)
}

/// Map a logical pixel to physical panel coordinates for a rotation.
const fn rotate_point(x: usize, y: usize, rotation: u16, w: usize, h: usize) -> (usize, usize) {
    match rotation {
        90 => (y, h - 1 - x),
        180 => (w - 1 - x, h - 1 - y),
        270 => (w - 1 - y, x),
        _ => (x, y),
    }
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
    /// idle ping), so the panel's drive polarity alternates uniformly no
    /// matter which path is talking.
    vcom_state: bool,
    /// Display rotation in degrees: 0, 90, 180, 270. The panel is physically
    /// W x H; for 90/270 the logical drawing surface is H x W (portrait).
    rotation: u16,
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
            rotation: 0,
        }
    }

    /// Set the display rotation (0, 90, 180, 270).
    pub fn with_rotation(mut self, rotation: u16) -> Self {
        self.rotation = rotation;
        self
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
    /// line. Commands carry VCOM (via `next_cmd`), the per-line buffers do
    /// not, and pixel data is written as-is — the VCOM bit is AC drive, not
    /// part of the image.
    ///
    /// With `full` unset, only lines whose content differs from `shown` are
    /// rewritten, so a content update does not flash the whole panel.
    async fn write_lines(&mut self, full: bool) -> Result<(), SPI::Error> {
        let per_line = bytes_per_line::<W>();
        let mut line = [0u8; 1 + 32 + 1];

        self.cs.set_high().ok();

        let start = self.next_cmd(BIT_WRITECMD);
        self.spi.write(&[start]).await?;

        for y in 0..H {
            let src = &self.fb[y * per_line..(y + 1) * per_line];
            if !full && src == &self.shown[y * per_line..(y + 1) * per_line] {
                continue;
            }
            for (dst, &b) in line[1..1 + per_line].iter_mut().zip(src) {
                // white background / black content = !b. Never inverted for
                // VCOM; Zephyr sends the same data on every refresh.
                *dst = !b;
            }
            line[0] = (y + 1) as u8;
            line[1 + per_line] = LINE_DUMMY;
            self.spi.write(&line[..2 + per_line]).await?;
        }

        let end = self.next_cmd(BIT_WRITECMD);
        self.spi.write(&[end]).await?;
        self.cs.set_low().ok();
        Ok(())
    }

    /// Write the framebuffer to the panel once. Content updates use
    /// `full = false` to skip unchanged lines.
    async fn refresh(&mut self, full: bool) {
        // `shown` is only updated afterwards, so a line that
        // changes-then-changes-back between two flushes is still written and
        // stays stable.
        let _ = self.write_lines(full).await;
        self.shown.copy_from_slice(&self.fb);
    }
}

impl<SPI, CS, const W: usize, const H: usize, const FB: usize> OriginDimensions
    for Ls0xxDisplay<SPI, CS, W, H, FB>
{
    fn size(&self) -> Size {
        if rotation_swaps(self.rotation) {
            Size::new(H as u32, W as u32)
        } else {
            Size::new(W as u32, H as u32)
        }
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
        let (lw, lh) = if rotation_swaps(self.rotation) { (H as i32, W as i32) } else { (W as i32, H as i32) };
        for Pixel(pt, color) in pixels {
            if pt.x < 0 || pt.y < 0 || pt.x >= lw || pt.y >= lh {
                continue;
            }
            let (x, y) = rotate_point(pt.x as usize, pt.y as usize, self.rotation, W, H);
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
        self.refresh(true).await;
    }

    async fn flush(&mut self) {
        // Only the changed gate lines are rewritten, so an update does not
        // flash the whole panel.
        if self.fb != self.shown {
            self.refresh(false).await;
        }
    }

    async fn tick(&mut self) {
        // VCOM maintenance (see the module docs): Zephyr's bare ping — a mode
        // byte carrying the alternating VCOM bit, no pixel data. It flips the
        // panel's drive polarity without rewriting any gate line, so it is
        // invisible (ZMK sends this at 30 Hz on this panel). Two bytes: the
        // command and one dummy byte, matching Zephyr's `empty_cmd[2]`.
        let cmd = self.next_cmd(0);
        self.cs.set_high().ok();
        let _ = self.spi.write(&[cmd, 0x00]).await;
        self.cs.set_low().ok();
    }
}
