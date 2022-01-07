// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.
#![cfg(target_arch = "x86_64")]

use devices::legacy::SerialDevice;
use devices::legacy::SerialEventsWrapper;
use logger::METRICS;
use std::fmt;
use std::sync::{Arc, Mutex};

use utils::eventfd::EventFd;
use vm_device::interrupt::{
    legacy::LegacyIrqConfig, ConfigurableInterrupt, Interrupt, InterruptSourceGroup,
};
use vm_superio::Serial;

use crate::{interrupts::KvmLegacyInterruptGroup, KvmInterruptManager, KvmLegacyInterrupt};

/// Errors corresponding to the `PortIODeviceManager`.
#[derive(Debug)]
pub enum Error {
    /// Cannot add legacy device to Bus.
    BusError(devices::BusError),
    /// Cannot create EventFd.
    EventFd(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::Error::*;

        match *self {
            BusError(ref err) => write!(f, "Failed to add legacy device to Bus: {}", err),
            EventFd(ref err) => write!(f, "Failed to create EventFd: {}", err),
        }
    }
}

type Result<T> = ::std::result::Result<T, Error>;

fn create_serial(com_event: Arc<KvmLegacyInterrupt>) -> Result<Arc<Mutex<SerialDevice<KvmLegacyInterrupt>>>> {
    let serial_device = Arc::new(Mutex::new(SerialDevice {
        serial: Serial::with_events(
            Some(com_event),
            SerialEventsWrapper {
                metrics: METRICS.uart.clone(),
                buffer_ready_event_fd: None,
            },
            Box::new(std::io::sink()),
        ),
        input: None,
    }));

    Ok(serial_device)
}

/// The `PortIODeviceManager` is a wrapper that is used for registering legacy devices
/// on an I/O Bus. It currently manages the uart and i8042 devices.
/// The `LegacyDeviceManger` should be initialized only by using the constructor.
pub struct PortIODeviceManager {
    pub io_bus: devices::Bus,
    pub stdio_serial: Arc<Mutex<SerialDevice<KvmLegacyInterrupt>>>,
    pub i8042: Arc<Mutex<devices::legacy::I8042Device>>,

    pub serial_irq_group: Arc<KvmLegacyInterruptGroup>,
    pub kbd_irq_group: Arc<KvmLegacyInterruptGroup>,
}

impl PortIODeviceManager {
    /// Create a new DeviceManager handling legacy devices (uart, i8042).
    pub fn new(
        serial: Arc<Mutex<SerialDevice<KvmLegacyInterrupt>>>,
        i8042_reset_evfd: EventFd,
        interrupt_manager: &KvmInterruptManager,
    ) -> Result<Self> {
        let io_bus = devices::Bus::new();
        // Interrupt group for COM ports
        let mut serial_irq_group = interrupt_manager.get_new_legacy_group().unwrap();
        // 1 IRQ for COM1 and COM3 + 1 IRQ for COM2 and COM4
        serial_irq_group.allocate_interrupts(2).unwrap();
        let irq = serial_irq_group.get(0).unwrap();
        irq.update(&LegacyIrqConfig {
            interrupt_line: Some(4),
            interrupt_pin: None,
        })
        .unwrap();

        {
            let mut locked_serial = serial.lock().expect("Cannot lock serial");
            locked_serial.set_interrupt_evt(irq);
        }

        let irq = serial_irq_group.get(1).unwrap();
        irq.update(&LegacyIrqConfig {
            interrupt_line: Some(3),
            interrupt_pin: None,
        })
        .unwrap();

        let mut kbd_irq_group = interrupt_manager.get_new_legacy_group().unwrap();
        kbd_irq_group.allocate_interrupts(1).unwrap();
        let kbd_irq = kbd_irq_group.get(0).unwrap();
        kbd_irq
            .update(&LegacyIrqConfig {
                interrupt_line: Some(1),
                interrupt_pin: None,
            })
            .unwrap();

        let i8042 = Arc::new(Mutex::new(devices::legacy::I8042Device::new(
            i8042_reset_evfd,
            kbd_irq
                .notifier()
                .unwrap()
                .try_clone()
                .map_err(Error::EventFd)?,
        )));

        Ok(PortIODeviceManager {
            io_bus,
            stdio_serial: serial,
            i8042,
            serial_irq_group: Arc::new(serial_irq_group),
            kbd_irq_group: Arc::new(kbd_irq_group),
        })
    }

    /// Register supported legacy devices.
    pub fn register_devices(&mut self) -> Result<()> {
        let com_1_3_irq = self.serial_irq_group.get(0 as usize).unwrap();
        let com_2_4_irq = self.serial_irq_group.get(1 as usize).unwrap();

        let serial_2_4 = create_serial(com_2_4_irq)?;
        let serial_1_3 = create_serial(com_1_3_irq)?;
        self.io_bus
            .insert(self.stdio_serial.clone(), 0x3f8, 0x8)
            .map_err(Error::BusError)?;
        self.io_bus
            .insert(serial_2_4.clone(), 0x2f8, 0x8)
            .map_err(Error::BusError)?;
        self.io_bus
            .insert(serial_1_3.clone(), 0x3e8, 0x8)
            .map_err(Error::BusError)?;
        self.io_bus
            .insert(serial_2_4, 0x2e8, 0x8)
            .map_err(Error::BusError)?;
        self.io_bus
            .insert(self.i8042.clone(), 0x060, 0x5)
            .map_err(Error::BusError)?;

        self.serial_irq_group.enable().unwrap();
        self.kbd_irq_group.enable().unwrap();

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::GuestAddress;

    #[test]
    fn test_register_legacy_devices() {
        let guest_mem =
            vm_memory::test_utils::create_anon_guest_memory(&[(GuestAddress(0x0), 0x1000)], false)
                .unwrap();
        let mut vm = crate::builder::setup_kvm_vm(&guest_mem, false).unwrap();
        crate::builder::setup_interrupt_controller(&mut vm).unwrap();
        let mut ldm = PortIODeviceManager::new(
            create_serial(EventFdTrigger::new(EventFd::new(EFD_NONBLOCK).unwrap())).unwrap(),
            EventFd::new(libc::EFD_NONBLOCK).unwrap(),
        )
        .unwrap();
        assert!(ldm.register_devices(vm.fd()).is_ok());
    }

    #[test]
    fn test_debug_error() {
        assert_eq!(
            format!("{}", Error::BusError(devices::BusError::Overlap)),
            format!(
                "Failed to add legacy device to Bus: {}",
                devices::BusError::Overlap
            )
        );
        assert_eq!(
            format!("{}", Error::EventFd(std::io::Error::from_raw_os_error(1))),
            format!(
                "Failed to create EventFd: {}",
                std::io::Error::from_raw_os_error(1)
            )
        );
    }
}
