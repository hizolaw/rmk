use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use rmk_config::resolved::hardware::{
    ChipSeries, CommunicationProtocol, DisplayConfig, DisplayDriver, I2cConfig, SpiConfig,
};

use super::input_device::Initializer;

/// Expand display configuration into initialization code + a processor.
/// Returns (initialization_code, processor_initializer).
pub(crate) fn expand_display_config(
    chip_series: &ChipSeries,
    display_config: &DisplayConfig,
) -> (TokenStream, Initializer) {
    let protocol_init = expand_protocol_init(chip_series, &display_config.protocol);
    let display_init = expand_display_driver_init(display_config);
    let processor_init = expand_display_processor_init(display_config);

    let initialization = quote! {
        #protocol_init
        #display_init
    };

    let processor = Initializer {
        initializer: processor_init,
        var_name: format_ident!("display_processor"),
    };

    (initialization, processor)
}

/// Generate interrupt binding tokens for the display bus.
pub(crate) fn expand_display_interrupt(
    chip_series: &ChipSeries,
    display_config: &DisplayConfig,
) -> TokenStream {
    match &display_config.protocol {
        CommunicationProtocol::I2c(i2c) => expand_i2c_interrupt(chip_series, i2c),
        CommunicationProtocol::Spi(spi) => expand_spi_interrupt(chip_series, spi),
    }
}

/// Bind the SPI instance's interrupt, required by the async `Spim` driver.
fn expand_spi_interrupt(chip_series: &ChipSeries, spi: &SpiConfig) -> TokenStream {
    let instance = format_ident!("{}", spi.instance);
    match chip_series {
        ChipSeries::Nrf52 => quote! {
            #instance => ::embassy_nrf::spim::InterruptHandler<::embassy_nrf::peripherals::#instance>;
        },
        _ => panic!("SPI display is only implemented for nRF52"),
    }
}

fn expand_i2c_interrupt(chip_series: &ChipSeries, i2c: &I2cConfig) -> TokenStream {
    let instance = format_ident!("{}", i2c.instance);

    match chip_series {
        ChipSeries::Rp2040 => {
            let irq = format_ident!("{}_IRQ", i2c.instance);
            quote! {
                #irq => ::embassy_rp::i2c::InterruptHandler<::embassy_rp::peripherals::#instance>;
            }
        }
        ChipSeries::Nrf52 => {
            quote! {
                #instance => ::embassy_nrf::twim::InterruptHandler<::embassy_nrf::peripherals::#instance>;
            }
        }
        ChipSeries::Stm32 => {
            let ev_irq = format_ident!("{}_EV", i2c.instance);
            let er_irq = format_ident!("{}_ER", i2c.instance);
            quote! {
                #ev_irq => ::embassy_stm32::i2c::EventInterruptHandler<::embassy_stm32::peripherals::#instance>;
                #er_irq => ::embassy_stm32::i2c::ErrorInterruptHandler<::embassy_stm32::peripherals::#instance>;
            }
        }
        ChipSeries::Esp32 => {
            quote! {}
        }
    }
}

fn expand_protocol_init(chip_series: &ChipSeries, protocol: &CommunicationProtocol) -> TokenStream {
    match protocol {
        CommunicationProtocol::I2c(i2c) => expand_i2c_init(chip_series, i2c),
        CommunicationProtocol::Spi(spi) => expand_spi_init(chip_series, spi),
    }
}

/// Create an async SPI bus for an SPI display (nRF52 only for now).
///
/// Note: nRF52840's SPIM0 shares the `TWISPI0` peripheral, so `instance` in
/// `keyboard.toml` must be `"TWISPI0"` for that bus.
fn expand_spi_init(chip_series: &ChipSeries, spi: &SpiConfig) -> TokenStream {
    let instance = format_ident!("{}", spi.instance.to_uppercase());
    let sck = format_ident!("{}", spi.sck);
    let mosi = format_ident!("{}", spi.mosi);
    let miso = format_ident!("{}", spi.miso);

    let cs = spi
        .cs
        .as_ref()
        .unwrap_or_else(|| panic!("SPI display requires `cs` in [display.protocol.spi]"));

    match chip_series {
        ChipSeries::Nrf52 => {
            let cs = format_ident!("{}", cs);
            quote! {
                // Put the SPI interrupt below the BLE controller's. The SDC
                // services RADIO/RTC0/TIMER0 at Priority::P0 and pairing breaks
                // if a burst of SPI interrupts (a full frame) delays them; the
                // SPI interrupt must therefore never preempt them.
                {
                    use ::embassy_nrf::interrupt::typelevel::Interrupt as _;
                    ::embassy_nrf::interrupt::typelevel::#instance::set_priority(
                        ::embassy_nrf::interrupt::Priority::P7,
                    );
                }
                let display_spi = {
                    use ::embassy_nrf::spim::{Frequency, Spim, Config, MODE_0, BitOrder};
                    let mut cfg = Config::default();
                    cfg.frequency = Frequency::M1;
                    cfg.mode = MODE_0;
                    // Sharp LS0xx addresses line numbers LSB-first (Zephyr sets
                    // SPI_TRANSFER_LSB). The driver default is MSB-first, which
                    // reverses the line-address bits and skews the image.
                    cfg.bit_order = BitOrder::LsbFirst;
                    Spim::new(p.#instance, Irqs, p.#sck, p.#miso, p.#mosi, cfg)
                };
                let display_cs_pin = p.#cs;
            }
        }
        _ => panic!("SPI display is only implemented for nRF52"),
    }
}

fn expand_i2c_init(chip_series: &ChipSeries, i2c: &I2cConfig) -> TokenStream {
    let instance = format_ident!("{}", i2c.instance.to_uppercase());
    let sda = format_ident!("{}", i2c.sda);
    let scl = format_ident!("{}", i2c.scl);

    match chip_series {
        ChipSeries::Rp2040 => {
            quote! {
                let display_i2c = ::embassy_rp::i2c::I2c::new_async(
                    p.#instance, p.#scl, p.#sda, Irqs, ::embassy_rp::i2c::Config::default()
                );
            }
        }
        ChipSeries::Nrf52 => {
            quote! {
                static DISPLAY_I2C_BUF: ::static_cell::StaticCell<[u8; 256]> = ::static_cell::StaticCell::new();
                let display_i2c_buf = DISPLAY_I2C_BUF.init([0u8; 256]);
                let display_i2c = ::embassy_nrf::twim::Twim::new(
                    p.#instance, Irqs, p.#sda, p.#scl, ::embassy_nrf::twim::Config::default(), display_i2c_buf
                );
            }
        }
        ChipSeries::Stm32 => {
            quote! {
                let display_i2c = ::embassy_stm32::i2c::I2c::new(
                    p.#instance, p.#scl, p.#sda, Irqs,
                    ::embassy_stm32::i2c::Config::default(),
                );
            }
        }
        ChipSeries::Esp32 => {
            quote! {
                let display_i2c = ::esp_hal::i2c::master::I2c::new(
                    p.#instance, ::esp_hal::i2c::master::Config::default()
                )
                .with_sda(p.#sda)
                .with_scl(p.#scl);
            }
        }
    }
}

fn get_i2c_address(config: &DisplayConfig) -> u8 {
    match &config.protocol {
        CommunicationProtocol::I2c(i2c) => i2c.address,
        CommunicationProtocol::Spi(_) => {
            panic!("display driver '{}' uses I2C but [display.protocol.spi] was given", display_driver_name(config))
        }
    }
}

fn display_driver_name(config: &DisplayConfig) -> &'static str {
    match config.driver {
        DisplayDriver::Ssd1306 => "ssd1306",
        DisplayDriver::Sh1106 => "sh1106",
        DisplayDriver::Sh1107 => "sh1107",
        DisplayDriver::Sh1108 => "sh1108",
        DisplayDriver::Ssd1309 => "ssd1309",
        DisplayDriver::Ls0xx => "ls0xx",
    }
}

fn expand_display_driver_init(config: &DisplayConfig) -> TokenStream {
    // The Sharp memory LCD takes no I2C address and ignores rotation here.
    if let DisplayDriver::Ls0xx = config.driver {
        let (w, h) = parse_ls0xx_size(&config.size);
        let fb = w * h / 8;
        let rotation = config.rotation;
        if !matches!(rotation, 0 | 90 | 180 | 270) {
            panic!("Unsupported LS0xx rotation '{}'. Supported: 0, 90, 180, 270", rotation);
        }
        return quote! {
            let display = ::rmk::display::drivers::ls0xx::Ls0xxDisplay::<_, _, #w, #h, #fb>::new(
                display_spi,
                ::embassy_nrf::gpio::Output::new(display_cs_pin, ::embassy_nrf::gpio::Level::Low, ::embassy_nrf::gpio::OutputDrive::Standard),
            )
            .with_rotation(#rotation);
        };
    }

    let address = get_i2c_address(config);
    let rotation = match config.rotation {
        0 => quote! { Rotate0 },
        90 => quote! { Rotate90 },
        180 => quote! { Rotate180 },
        270 => quote! { Rotate270 },
        rotation => panic!(
            "Unsupported display rotation '{}'. Supported: 0, 90, 180, 270",
            rotation
        ),
    };

    match &config.driver {
        DisplayDriver::Ssd1306 => {
            let display_size = parse_ssd1306_size(&config.size);
            quote! {
                let display_interface = ::rmk::display::ssd1306::I2CDisplayInterface::new_custom_address(display_i2c, #address);
                let display = ::rmk::display::ssd1306::Ssd1306Async::new(
                    display_interface,
                    #display_size,
                    ::rmk::display::ssd1306::prelude::DisplayRotation::#rotation,
                ).into_buffered_graphics_mode();
            }
        }
        // Handled above; unreachable here.
        DisplayDriver::Ls0xx => unreachable!(),
        driver @ (DisplayDriver::Sh1106
        | DisplayDriver::Sh1107
        | DisplayDriver::Sh1108
        | DisplayDriver::Ssd1309) => {
            let display_variant_path = parse_oled_async_variant(driver, &config.size);
            quote! {
                let display_interface = ::rmk::display::display_interface_i2c::I2CInterface::new(display_i2c, #address, 0x40);
                let display: ::rmk::display::oled_async::mode::graphics::GraphicsMode<_, _> =
                    ::rmk::display::oled_async::Builder::new(#display_variant_path {})
                        .with_rotation(::rmk::display::oled_async::displayrotation::DisplayRotation::#rotation)
                        .connect(display_interface)
                        .into();
            }
        }
    }
}

fn expand_display_processor_init(config: &DisplayConfig) -> TokenStream {
    let constructor = if let Some(renderer_path) = &config.renderer {
        // Allow bare names like "OledRenderer" as shorthand for "::rmk::display::OledRenderer".
        let full_path = if renderer_path.contains("::") {
            renderer_path.clone()
        } else {
            format!("::rmk::display::{}", renderer_path)
        };
        let renderer_type: syn::Type = syn::parse_str(&full_path)
            .unwrap_or_else(|e| panic!("Invalid renderer type path '{}': {}", renderer_path, e));
        quote! {
            ::rmk::display::DisplayProcessor::with_renderer(
                display,
                #renderer_type::default(),
            )
        }
    } else {
        quote! {
            ::rmk::display::DisplayProcessor::new(display)
        }
    };

    let render_interval = config.render_interval.map(|ms| {
        quote! {
            .with_render_interval(::embassy_time::Duration::from_millis(#ms))
        }
    });

    let min_render_interval = config.min_render_interval.map(|ms| {
        quote! {
            .with_min_render_interval(::embassy_time::Duration::from_millis(#ms))
        }
    });

    // A memory LCD has to keep being driven or its image fades. Use
    // `render_interval` as the ls0xx refresh period (via with_vcom_interval)
    // and do NOT pass it to `with_render_interval`: this repaints a whole
    // frame, so the value should be slow (hundreds of ms) - fast full-frame
    // redraws starve the BLE stack.
    let vcom_interval = if matches!(config.driver, DisplayDriver::Ls0xx) {
        let ms = config.render_interval.unwrap_or(500);
        quote! {
            .with_vcom_interval(::embassy_time::Duration::from_millis(#ms))
        }
    } else {
        quote! {}
    };
    let render_interval = if matches!(config.driver, DisplayDriver::Ls0xx) {
        quote! {}
    } else {
        render_interval.unwrap_or_default()
    };

    quote! {
        let mut display_processor = #constructor
            #render_interval
            #min_render_interval
            #vcom_interval;
    }
}

/// Parse the panel size for the Sharp memory LCD. Only the nice!view's 160x68
/// is supported for now.
fn parse_ls0xx_size(size: &str) -> (usize, usize) {
    match size {
        "160x68" => (160, 68),
        _ => panic!("Unsupported LS0xx display size '{}'. Supported: 160x68", size),
    }
}

fn parse_ssd1306_size(size: &str) -> TokenStream {
    match size {
        "128x64" => quote! { ::rmk::display::ssd1306::prelude::DisplaySize128x64 },
        "128x32" => quote! { ::rmk::display::ssd1306::prelude::DisplaySize128x32 },
        "96x16" => quote! { ::rmk::display::ssd1306::prelude::DisplaySize96x16 },
        "72x40" => quote! { ::rmk::display::ssd1306::prelude::DisplaySize72x40 },
        "64x48" => quote! { ::rmk::display::ssd1306::prelude::DisplaySize64x48 },
        _ => panic!(
            "Unsupported SSD1306 display size '{}'. Supported: 128x64, 128x32, 96x16, 72x40, 64x48",
            size
        ),
    }
}

fn parse_oled_async_variant(driver: &DisplayDriver, size: &str) -> TokenStream {
    let (module, variant_name) = match (driver, size) {
        (DisplayDriver::Sh1106, "128x64") => ("sh1106", "Sh1106_128_64"),
        (DisplayDriver::Sh1107, "64x128") => ("sh1107", "Sh1107_64_128"),
        (DisplayDriver::Sh1107, "128x128") => ("sh1107", "Sh1107_128_128"),
        (DisplayDriver::Sh1108, "64x160") => ("sh1108", "Sh1108_64_160"),
        (DisplayDriver::Sh1108, "96x160") => ("sh1108", "Sh1108_96_160"),
        (DisplayDriver::Sh1108, "128x160") => ("sh1108", "Sh1108_128_160"),
        (DisplayDriver::Sh1108, "160x160") => ("sh1108", "Sh1108_160_160"),
        (DisplayDriver::Ssd1309, "128x64") => ("ssd1309", "Ssd1309_128_64"),
        (DisplayDriver::Sh1106, _) => {
            panic!(
                "Unsupported SH1106 display size '{}'. Supported: 128x64",
                size
            )
        }
        (DisplayDriver::Sh1107, _) => {
            panic!(
                "Unsupported SH1107 display size '{}'. Supported: 64x128, 128x128",
                size
            )
        }
        (DisplayDriver::Sh1108, _) => panic!(
            "Unsupported SH1108 display size '{}'. Supported: 64x160, 96x160, 128x160, 160x160",
            size
        ),
        (DisplayDriver::Ssd1309, _) => {
            panic!(
                "Unsupported SSD1309 display size '{}'. Supported: 128x64",
                size
            )
        }
        _ => unreachable!(),
    };

    let module_ident = format_ident!("{}", module);
    let variant_ident = format_ident!("{}", variant_name);

    quote! { ::rmk::display::oled_async::displays::#module_ident::#variant_ident }
}
