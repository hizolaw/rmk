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

/// Bytes of pixel data per gate line: one bit per pixel, `W` is a multiple of 8.
const fn bytes_per_line<const W: usize>() -> usize {
    W >> 3
}

/// Bytes needed to send one whole frame in a single SPI transaction:
/// start command + `H` lines of `[line#, pixels, dummy]` + trailing latch.
const fn txn_len<const W: usize, const H: usize>() -> usize {
    1 + H * (bytes_per_line::<W>() + 2) + 1
}

/// A Sharp memory LCD over an async SPI bus.
///
/// `W`/`H` are the panel resolution; `FB` is `W * H / 8` (the framebuffer
/// length) and `TX` is [`txn_len`], both passed explicitly because stable Rust
/// does not allow const arithmetic on generic parameters.
pub struct Ls0xxDisplay<SPI, CS, const W: usize, const H: usize, const FB: usize, const TX: usize> {
    spi: SPI,
    cs: CS,
    fb: [u8; FB],
    tx: [u8; TX],
    vcom: bool,
    /// Set only when a pixel's bit actually changed, so an idle redraw can skip
    /// the whole frame and just flip VCOM.
    dirty: bool,
}

impl<SPI, CS, const W: usize, const H: usize, const FB: usize, const TX: usize>
    Ls0xxDisplay<SPI, CS, W, H, FB, TX>
where
    SPI: SpiBus<u8>,
    CS: OutputPin,
{
    pub fn new(spi: SPI, cs: CS) -> Self {
        Self {
            spi,
            cs,
            fb: [0; FB],
            tx: [0; TX],
            vcom: false,
            dirty: true,
        }
    }

    fn flags(&self, base: u8) -> u8 {
        if self.vcom { base | BIT_VCOM } else { base }
    }

    /// Pack the framebuffer into `tx` as one contiguous frame.
    fn pack(&mut self) {
        let per_line = bytes_per_line::<W>();

        // Frame start; this byte fixes the VCOM polarity for the frame.
        self.tx[0] = self.flags(BIT_WRITECMD);
        self.vcom = !self.vcom;

        let mut out = 1;
        for y in 0..H {
            let src = y * per_line;
            self.tx[out] = (y + 1) as u8; // 1-based line number
            self.tx[out + 1..out + 1 + per_line].copy_from_slice(&self.fb[src..src + per_line]);
            self.tx[out + 1 + per_line] = 0x00; // dummy byte
            out += per_line + 2;
        }

        // Trailing byte latches the last line; its value is ignored.
        self.tx[out] = self.flags(BIT_WRITECMD);
    }

    /// Push the framebuffer to the panel in a single SPI transaction.
    async fn write_frame(&mut self) -> Result<(), SPI::Error> {
        self.pack();
        self.cs.set_high().ok();
        let result = self.spi.write(&self.tx).await;
        self.cs.set_low().ok();
        result
    }

    /// Flip VCOM with a bare command. Call this on a ~33ms ticker so the panel
    /// keeps its contrast while the screen is not being redrawn.
    pub async fn vcom_tick(&mut self) -> Result<(), SPI::Error> {
        let cmd = self.flags(BIT_VCOM);
        self.vcom = !self.vcom;
        self.cs.set_high().ok();
        let result = self.spi.write(&[cmd]).await;
        self.cs.set_low().ok();
        result
    }
}

impl<SPI, CS, const W: usize, const H: usize, const FB: usize, const TX: usize> OriginDimensions
    for Ls0xxDisplay<SPI, CS, W, H, FB, TX>
{
    fn size(&self) -> Size {
        Size::new(W as u32, H as u32)
    }
}

impl<SPI, CS, const W: usize, const H: usize, const FB: usize, const TX: usize> DrawTarget
    for Ls0xxDisplay<SPI, CS, W, H, FB, TX>
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
            let mask = 0x80 >> (x % 8);
            let before = self.fb[index];
            match color {
                BinaryColor::On => self.fb[index] |= mask,
                BinaryColor::Off => self.fb[index] &= !mask,
            }
            if self.fb[index] != before {
                self.dirty = true;
            }
        }
        Ok(())
    }
}

impl<SPI, CS, const W: usize, const H: usize, const FB: usize, const TX: usize> DisplayDriver
    for Ls0xxDisplay<SPI, CS, W, H, FB, TX>
where
    SPI: SpiBus<u8>,
    CS: OutputPin,
{
    async fn init(&mut self) {
        self.fb.fill(0);
        self.dirty = true;
        let _ = self.write_frame().await;
        self.dirty = false;
    }

    async fn flush(&mut self) {
        if self.dirty {
            // Content changed: repaint the whole panel (one SPI transaction).
            let _ = self.write_frame().await;
            self.dirty = false;
        } else {
            // Nothing to redraw: just flip VCOM so the image keeps contrast.
            let _ = self.vcom_tick().await;
        }
    }
}
