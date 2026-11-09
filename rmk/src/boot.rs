pub fn jump_to_bootloader() {
    #[cfg(feature = "adafruit_bl")]
    // Reference: https://github.com/adafruit/Adafruit_nRF52_Bootloader/blob/d6b28e66053eea467166f44875e3c7ec741cb471/src/main.c#L107
    embassy_nrf::pac::POWER
        .gpregret()
        .write_value(embassy_nrf::pac::power::regs::Gpregret(0x57));

    #[cfg(feature = "rp2040")]
    // Jump to RP2040 bootloader
    embassy_rp::rom_data::reset_to_usb_boot(0, 0);

    #[cfg(all(
        feature = "zsa_voyager_bl",
        target_arch = "arm",
        target_os = "none",
        any(target_abi = "eabi", target_abi = "eabihf")
    ))]
    unsafe {
        const GPIOA_MODER: *mut u32 = 0x4800_0000 as *mut u32;
        const GPIOA_ODR: *mut u32 = 0x4800_0014 as *mut u32;
        // PA8 + PA9: push-pull output, drive high.
        let m = core::ptr::read_volatile(GPIOA_MODER);
        core::ptr::write_volatile(GPIOA_MODER, (m & !(0b1111 << 16)) | (0b0101 << 16));
        let d = core::ptr::read_volatile(GPIOA_ODR);
        core::ptr::write_volatile(GPIOA_ODR, d | (1 << 8) | (1 << 9));
        // 500 ms at 72 MHz SYSCLK charges the RC network past the bootloader's threshold.
        cortex_m::asm::delay(36_000_000);
        // PA9 low before reset discharges the cap.
        let d = core::ptr::read_volatile(GPIOA_ODR);
        core::ptr::write_volatile(GPIOA_ODR, d & !(1 << 9));
    }

    #[cfg(not(any(feature = "adafruit_bl", feature = "rp2040", feature = "zsa_voyager_bl")))]
    warn!("Please specify a bootloader to jump to!");

    reboot_keyboard();
}

pub(crate) fn reboot_keyboard() {
    warn!("Rebooting keyboard!");
    // For cortex-m:
    #[cfg(all(
        target_arch = "arm",
        target_os = "none",
        any(target_abi = "eabi", target_abi = "eabihf")
    ))]
    cortex_m::peripheral::SCB::sys_reset();

    #[cfg(feature = "_esp_ble")]
    esp_hal::system::software_reset();
}

/// GPIO sets used to wake from nRF System OFF: the matrix outputs to drive
/// active and the matrix inputs to sense. Registered once at startup by the
/// generated matrix-init code.
#[cfg(feature = "_nrf_ble")]
struct SystemOffPins {
    outputs: heapless::Vec<(u8, u8), 24>,
    inputs: heapless::Vec<(u8, u8), 24>,
}

#[cfg(feature = "_nrf_ble")]
static SYSTEM_OFF_PINS: embassy_sync::blocking_mutex::Mutex<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    core::cell::RefCell<Option<SystemOffPins>>,
> = embassy_sync::blocking_mutex::Mutex::new(core::cell::RefCell::new(None));

/// Output pins to return to their inactive level before System OFF — e.g. the
/// EXT_POWER rail feeding the display. The GPIO output state survives System
/// OFF, so a rail left driven keeps powering its peripherals the whole time
/// the keyboard is "off". The bool is the level to drive, already resolved
/// from the output's active polarity by the codegen.
#[cfg(feature = "_nrf_ble")]
static SYSTEM_OFF_LOW_PINS: embassy_sync::blocking_mutex::Mutex<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    core::cell::RefCell<Option<heapless::Vec<(u8, u8, bool), 8>>>,
> = embassy_sync::blocking_mutex::Mutex::new(core::cell::RefCell::new(None));

/// Record the (port, pin, level) triples `soft_off()` drives before System
/// OFF. Called by generated code; see rmk-macro's output pin initialization.
#[cfg(feature = "_nrf_ble")]
pub fn register_system_off_low_pins(pins: &[(u8, u8, bool)]) {
    SYSTEM_OFF_LOW_PINS.lock(|c| {
        let mut v = heapless::Vec::new();
        for &p in pins {
            let _ = v.push(p);
        }
        *c.borrow_mut() = Some(v);
    });
}

/// Record the (port, pin) pairs a later `soft_off()` must drive and sense.
/// Called by generated code; see rmk-macro's matrix pin initialization.
#[cfg(feature = "_nrf_ble")]
pub fn register_system_off_pins(outputs: &[(u8, u8)], inputs: &[(u8, u8)]) {
    SYSTEM_OFF_PINS.lock(|c| {
        let mut pins = SystemOffPins { outputs: heapless::Vec::new(), inputs: heapless::Vec::new() };
        for &p in outputs {
            let _ = pins.outputs.push(p);
        }
        for &p in inputs {
            let _ = pins.inputs.push(p);
        }
        *c.borrow_mut() = Some(pins);
    });
}

/// Enter nRF System OFF: the deepest power state, from which the chip wakes
/// with a full reset when a sensed matrix input goes active.
///
/// In System OFF the GPIO block retains its configuration, so driving every
/// matrix output active and arming SENSE on every input makes any key press
/// connect a driven output to a sensed input and assert DETECT. Without the
/// driven outputs the inputs would just float and no press could ever wake.
#[cfg(feature = "_nrf_ble")]
pub fn soft_off() -> ! {
    use embassy_nrf::pac;
    use embassy_nrf::pac::gpio::vals::{Dir, Sense};

    // Drop the power rails first: EXT_POWER feeds the display, and GPIO
    // output state is retained through System OFF, so without this the panel
    // stays powered (and DC-biased on a static image) until the next wake.
    SYSTEM_OFF_LOW_PINS.lock(|c| {
        if let Some(pins) = c.borrow().as_ref() {
            for &(port, pin, high) in pins.iter() {
                let p = if port == 0 { pac::P0 } else { pac::P1 };
                let n = pin as usize;
                p.pin_cnf(n).modify(|w| w.set_dir(Dir::Output));
                p.out().modify(|w| w.set_pin(n, high));
            }
        }
    });

    SYSTEM_OFF_PINS.lock(|c| {
        match c.borrow().as_ref() {
            Some(pins) => {
                for &(port, pin) in pins.outputs.iter() {
                    let p = if port == 0 { pac::P0 } else { pac::P1 };
                    let n = pin as usize;
                    p.pin_cnf(n).modify(|w| w.set_dir(Dir::Output));
                    p.out().modify(|w| w.set_pin(n, true));
                }
                for &(port, pin) in pins.inputs.iter() {
                    let p = if port == 0 { pac::P0 } else { pac::P1 };
                    p.pin_cnf(pin as usize).modify(|w| w.set_sense(Sense::High));
                }
            }
            None => warn!("soft_off: no wake pins registered, only reset will wake the keyboard"),
        }
    });

    pac::POWER.systemoff().write(|w| w.set_systemoff(true));
    // The write is not instantaneous; park until the chip actually powers down.
    loop {
        cortex_m::asm::wfe();
    }
}

/// Power the keyboard off. On nRF this is System OFF with matrix wake; on
/// other chips there is no equivalent yet, so fall back to a reboot.
pub fn power_off() -> ! {
    #[cfg(feature = "_nrf_ble")]
    soft_off();

    #[cfg(not(feature = "_nrf_ble"))]
    reboot_keyboard();

    #[allow(unreachable_code)]
    loop {}
}
