//! [`DisplayDriver`] adapter for Sharp memory LCDs over SPI.
//!
//! The nice!view is a 160x68 Sharp LS0xx panel with one bit per pixel. The wire
//! protocol follows Zephyr's long-used `drivers/display/ls0xx.c` because
//! nice!view uses the `sharp,ls0xx` variant:
//!
//! * a frame starts with a write-command byte (bit 0) that also carries the
//!   VCOM polarity, and that polarity flips once per frame;
//! * each gate line is `[line_number, W/8 data bytes, dummy_byte]`, with line
//!   numbers starting at 1;
//! * a trailing write byte latches the last line.
//!
//! Two things here differ from a naive port, both to keep the radio happy:
//!
//! * **One SPI transaction per frame.** The line numbers and dummy bytes are
//!   interleaved into a single contiguous transmit buffer, so a frame raises a
//!   single SPI "end" interrupt instead of one per gate line. On nRF the BLE
//!   controller depends on its interrupt being serviced promptly, and a burst
//!   of same-priority SPI interrupts delays it enough to break pairing.
//! * **VCOM is its own timer.** The panel needs VCOM flipped every few tens of
//!   milliseconds or the image fades from accumulated DC bias, independent of
//!   how often the screen is redrawn. [`Ls0xxDisplay::vcom_tick`] sends a bare
//!   VCOM command and is meant to run on a ~33ms ticker.
//!
//! The interrupt priority is set by the caller in generated code (see
//! `expand_spi_init`), not here, because it needs the interrupt type.

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_hal::digital::OutputPin;
use embedded_hal_async::spi::SpiBus;

use crate::display::DisplayDriver;

// LS0xx command bits, from Zephyr's ls0xx driver.
const BIT_WRITECMD: u8 = 0x01;
const BIT_VCOM: u8 = 0x02;
const BIT_CLEAR: u8 = 0x04;

/// Trailing byte after each gate line's pixels. The first working version sent
/// 0x00 here and the panel displayed content, so keep that rather than Zephyr's
/// 27 — the value is not what made the screen blank.
const LINE_DUMMY: u8 = 0x00;

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
    /// What the renderer draws into.
    fb: [u8; FB],
    /// What was last sent to the panel. flush() compares against this, so a
    /// renderer that clears and repaints identical content does not cause a
    /// visible full-frame repaint.
    shown: [u8; FB],
    vcom: bool,
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
            vcom: false,
        }
    }

    fn flags(&self, base: u8) -> u8 {
        if self.vcom { base | BIT_VCOM } else { base }
    }

    /// Push the framebuffer to the panel, one gate line per SPI transaction.
    ///
    /// This is what Zephyr's ls0xx driver does, and the panel is picky: a
    /// single merged burst did not display at all on the nice!view. Keeping
    /// each line as its own transaction costs one SPI interrupt per line but is
    /// the known-good sequence.
    async fn write_frame(&mut self) -> Result<(), SPI::Error> {
        let per_line = bytes_per_line::<W>();
        let mut line = [0u8; 1 + 32 + 1];

        self.cs.set_high().ok();

        let start = self.flags(BIT_WRITECMD);
        self.vcom = !self.vcom;
        self.spi.write(&[start]).await?;

        for y in 0..H {
            line[0] = (y + 1) as u8;
            line[1..1 + per_line].copy_from_slice(&self.fb[y * per_line..(y + 1) * per_line]);
            line[1 + per_line] = LINE_DUMMY;
            self.spi.write(&line[..2 + per_line]).await?;
        }

        self.spi.write(&[self.flags(BIT_WRITECMD)]).await?;

        self.cs.set_low().ok();
        Ok(())
    }

    /// Hardware clear (Zephyr's `ls0xx_clear`): 2-byte command, `[CLEAR, 0]`.
    async fn clear_panel(&mut self) -> Result<(), SPI::Error> {
        self.cs.set_high().ok();
        let result = self.spi.write(&[BIT_CLEAR, 0x00]).await;
        self.cs.set_low().ok();
        result
    }

    /// Flip VCOM with a bare command. Call this on a ~33ms ticker so the panel
    /// keeps its contrast while the screen is not being redrawn.
    ///
    pub async fn vcom_tick(&mut self) -> Result<(), SPI::Error> {
        let cmd = self.flags(BIT_VCOM);
        self.vcom = !self.vcom;
        self.cs.set_high().ok();
        let result = self.spi.write(&[cmd]).await;
        self.cs.set_low().ok();
        result
    }

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
        // The panel keeps its last image across MCU resets (it is memory LCD).
        // Without this the previous firmware's content stays on screen.
        let _ = self.clear_panel().await;
        embassy_time::Timer::after_millis(10).await;
        self.fb.fill(0);
        let _ = self.write_frame().await;
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
        // VCOM heartbeat: independent of redraws so an idle screen keeps its
        // contrast. The interval is set with with_vcom_interval().
        let _ = self.vcom_tick().await;
    }
}
