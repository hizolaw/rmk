//! [`DisplayDriver`] adapter for Sharp memory LCDs over SPI.
//!
//! The nice!view is a 160x68 Sharp LS0xx panel with one bit per pixel. The wire
//! protocol follows Zephyr's long-used `drivers/display/ls0xx.c` because
//! nice!view uses the `sharp,ls0xx` variant (the generic `memory-lcd-spi` crate
//! targets a slightly different command set):
//!
//! * a frame starts with a write-command byte (bit 0) that also carries the
//!   VCOM polarity, and that polarity flips once per frame;
//! * each gate line is `[line_number, W/8 data bytes, dummy_byte]`, with line
//!   numbers starting at 1;
//! * a trailing write byte latches the last line.
//!
//! The panel needs its VCOM polarity flipped regularly or it accumulates a DC
//! bias and fades. [`DisplayDriver::flush`] flips it every frame, so configure a
//! periodic redraw (`render_interval` in `keyboard.toml`) to keep it healthy
//! while idle.
//!
//! All bus access is `async`; nothing here blocks the executor.

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_hal::digital::OutputPin;
use embedded_hal_async::spi::SpiBus;

use crate::display::DisplayDriver;

// LS0xx command bits, from Zephyr's ls0xx driver.
const BIT_WRITECMD: u8 = 0x01;
const BIT_VCOM: u8 = 0x02;

/// Bytes per gate line: one bit per pixel, `W` is a multiple of 8.
const fn bytes_per_line<const W: usize>() -> usize {
    W >> 3
}

/// A Sharp memory LCD over an async SPI bus.
///
/// `W`/`H` are the panel resolution in pixels; the framebuffer is `W * H / 8`
/// bytes (one bit per pixel). `SPI` is an async SPI bus (e.g.
/// `embassy_nrf::spim::Spim<'static, Async>`), `CS` an active-high chip select.
pub struct Ls0xxDisplay<SPI, CS, const W: usize, const H: usize, const FB: usize> {
    spi: SPI,
    cs: CS,
    fb: [u8; FB],
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
            vcom: false,
        }
    }

    /// Command/flags byte carrying the current VCOM polarity.
    fn flags(&self, base: u8) -> u8 {
        if self.vcom { base | BIT_VCOM } else { base }
    }

    /// Send the whole framebuffer to the panel, flipping VCOM for this frame.
    async fn write_frame(&mut self) -> Result<(), SPI::Error> {
        let per_line = bytes_per_line::<W>();
        // One gate line: 1 line-number byte + pixels + 1 dummy byte.
        let mut line = [0u8; 1 + 32 + 1];

        self.cs.set_high().ok();

        // Frame start; this byte decides the VCOM polarity for the frame.
        let start = self.flags(BIT_WRITECMD);
        self.vcom = !self.vcom;
        self.spi.write(&[start]).await?;

        for y in 0..H {
            line[0] = (y + 1) as u8;
            line[1..1 + per_line].copy_from_slice(&self.fb[y * per_line..(y + 1) * per_line]);
            line[1 + per_line] = 0x00;
            self.spi.write(&line[..2 + per_line]).await?;
        }

        // Trailing byte latches the last line; its value is ignored.
        self.spi.write(&[self.flags(BIT_WRITECMD)]).await?;

        self.cs.set_low().ok();
        Ok(())
    }
}

impl<SPI, CS, const W: usize, const H: usize, const FB: usize> OriginDimensions for Ls0xxDisplay<SPI, CS, W, H, FB> {
    fn size(&self) -> Size {
        Size::new(W as u32, H as u32)
    }
}

impl<SPI, CS, const W: usize, const H: usize, const FB: usize> DrawTarget for Ls0xxDisplay<SPI, CS, W, H, FB> {
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
            match color {
                BinaryColor::On => self.fb[index] |= mask,
                BinaryColor::Off => self.fb[index] &= !mask,
            }
        }
        Ok(())
    }
}

impl<SPI, CS, const W: usize, const H: usize, const FB: usize> DisplayDriver for Ls0xxDisplay<SPI, CS, W, H, FB>
where
    SPI: SpiBus<u8>,
    CS: OutputPin,
{
    async fn init(&mut self) {
        // Start from a known-blank panel.
        self.fb.fill(0);
        let _ = self.write_frame().await;
    }

    async fn flush(&mut self) {
        let _ = self.write_frame().await;
    }
}
