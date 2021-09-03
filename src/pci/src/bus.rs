// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE-BSD-3-Clause file.

use crate::configuration::{
    PciBridgeSubclass, PciCapability, PciCapabilityId, PciClassCode, PciConfiguration,
    PciHeaderType,
};
use crate::device::PciDevice;
use byteorder::{ByteOrder, LittleEndian};
use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Barrier, Mutex};
use vm_device::{bus::MmioAddress, bus::PioAddress, MutDeviceMmio, MutDevicePio};
use vm_memory::ByteValued;

const VENDOR_ID_INTEL: u16 = 0x8086;
const DEVICE_ID_IOH_EPORT: u16 = 0x3420;
const DEVICE_ID_IOH_REV: u8 = 0x2;
const NUM_DEVICE_IDS: usize = 32;

/// Errors for device manager.
#[derive(Debug)]
pub enum PciRootError {
    /// Could not allocate an IRQ number.
    AllocateIrq,
    /// Could not find an available device slot on the PCI bus.
    NoPciDeviceSlotAvailable,
    /// Invalid PCI device identifier provided.
    InvalidPciDeviceSlot(usize),
    /// Valid PCI device identifier but already used.
    AlreadyInUsePciDeviceSlot(usize),
}
pub type Result<T> = std::result::Result<T, PciRootError>;

#[repr(packed)]
#[derive(Clone, Copy, Default)]
#[allow(dead_code)]
struct SsvidCap {
    reserved: u16,
    svid: u16,
    ssid: u16,
}

unsafe impl ByteValued for SsvidCap {}

impl PciCapability for SsvidCap {
    fn bytes(&self) -> &[u8] {
        self.as_slice()
    }

    fn id(&self) -> PciCapabilityId {
        PciCapabilityId::BridgeSubsystemVendorDeviceId
    }
}

#[repr(packed)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct PcieCap {
    ptype: u16,
    data: [u8; 59],
}

unsafe impl ByteValued for PcieCap {}

impl PciCapability for PcieCap {
    fn bytes(&self) -> &[u8] {
        self.as_slice()
    }

    fn id(&self) -> PciCapabilityId {
        PciCapabilityId::PciExpress
    }
}

impl Default for PcieCap {
    #[inline]
    fn default() -> PcieCap {
        PcieCap {
            ptype: 0x42, //Express (v2) Root Port
            data: [0; 59],
        }
    }
}

/// Emulates the PCI Root bridge device.
pub struct PciRoot {
    /// Configuration space.
    config: PciConfiguration,
}

impl PciRoot {
    /// Create an empty PCI root bridge.
    pub fn new(config: Option<PciConfiguration>) -> Self {
        if let Some(config) = config {
            PciRoot { config }
        } else {
            let mut root = PciRoot {
                config: PciConfiguration::new(
                    VENDOR_ID_INTEL,
                    DEVICE_ID_IOH_EPORT,
                    DEVICE_ID_IOH_REV,
                    PciClassCode::BridgeDevice,
                    &PciBridgeSubclass::PciToPciBridge,
                    None,
                    PciHeaderType::Bridge,
                    VENDOR_ID_INTEL,
                    0,
                    None,
                ),
            };
            let ssvid_cap = SsvidCap {
                reserved: 0,
                svid: VENDOR_ID_INTEL,
                ssid: 0,
            };
            root.config.add_capability(&ssvid_cap).unwrap();
            let pcie_cap = PcieCap::default();
            root.config.add_capability(&pcie_cap).unwrap();
            root
        }
    }
}

impl PciDevice for PciRoot {
    fn write_config_register(
        &mut self,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
    ) -> Option<Arc<Barrier>> {
        self.config.write_config_register(reg_idx, offset, data);
        None
    }

    fn read_config_register(&mut self, reg_idx: usize) -> u32 {
        self.config.read_reg(reg_idx)
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

pub struct PciBus {
    /// Devices attached to this bus.
    /// Device 0 is host bridge.
    devices: HashMap<u32, Arc<Mutex<dyn PciDevice>>>,
    device_ids: Vec<bool>,
}

impl PciBus {
    pub fn new(pci_root: Arc<Mutex<dyn PciDevice>>) -> Self {
        let mut devices: HashMap<u32, Arc<Mutex<dyn PciDevice>>> = HashMap::new();
        let mut device_ids: Vec<bool> = vec![false; NUM_DEVICE_IDS];

        devices.insert(0, pci_root);
        device_ids[0] = true;

        PciBus {
            devices,
            device_ids,
        }
    }

    pub fn add_device(
        &mut self,
        pci_device_bdf: u32,
        device: Arc<Mutex<dyn PciDevice>>,
    ) -> Result<()> {
        self.devices.insert(pci_device_bdf >> 3, device);
        Ok(())
    }

    pub fn remove_by_device(&mut self, device: &Arc<Mutex<dyn PciDevice>>) -> Result<()> {
        self.devices.retain(|_, dev| !Arc::ptr_eq(dev, device));
        Ok(())
    }

    pub fn next_device_id(&mut self) -> Result<u32> {
        for (idx, device_id) in self.device_ids.iter_mut().enumerate() {
            if !(*device_id) {
                *device_id = true;
                return Ok(idx as u32);
            }
        }

        Err(PciRootError::NoPciDeviceSlotAvailable)
    }

    pub fn get_device_id(&mut self, id: usize) -> Result<()> {
        if id < NUM_DEVICE_IDS {
            if !self.device_ids[id] {
                self.device_ids[id] = true;
                Ok(())
            } else {
                Err(PciRootError::AlreadyInUsePciDeviceSlot(id))
            }
        } else {
            Err(PciRootError::InvalidPciDeviceSlot(id))
        }
    }

    pub fn put_device_id(&mut self, id: usize) -> Result<()> {
        if id < NUM_DEVICE_IDS {
            self.device_ids[id] = false;
            Ok(())
        } else {
            Err(PciRootError::InvalidPciDeviceSlot(id))
        }
    }
}

pub struct PciConfigIo {
    /// Config space register.
    config_address: u32,
    pci_bus: Arc<Mutex<PciBus>>,
}

impl PciConfigIo {
    pub fn new(pci_bus: Arc<Mutex<PciBus>>) -> Self {
        PciConfigIo {
            pci_bus,
            config_address: 0,
        }
    }

    pub fn config_space_read(&self) -> u32 {
        let enabled = (self.config_address & 0x8000_0000) != 0;
        if !enabled {
            return 0xffff_ffff;
        }

        let (bus, device, function, register) =
            parse_io_config_address(self.config_address & !0x8000_0000);

        error!(
            "config space read {}:{}:{} reg {}",
            bus, device, function, register
        );

        // Only support one bus.
        if bus != 0 {
            return 0xffff_ffff;
        }

        // Don't support multi-function devices.
        if function > 0 {
            return 0xffff_ffff;
        }

        self.pci_bus
            .lock()
            .unwrap()
            .devices
            .get(&(device as u32))
            .map_or(0xffff_ffff, |d| {
                d.lock().unwrap().read_config_register(register)
            })
    }

    pub fn config_space_write(&mut self, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        if offset as usize + data.len() > 4 {
            return None;
        }

        let enabled = (self.config_address & 0x8000_0000) != 0;
        if !enabled {
            return None;
        }

        let (bus, device, _function, register) =
            parse_io_config_address(self.config_address & !0x8000_0000);

        // Only support one bus.
        if bus != 0 {
            return None;
        }

        let pci_bus = self.pci_bus.lock().unwrap();
        if let Some(d) = pci_bus.devices.get(&(device as u32)) {
            let mut device = d.lock().unwrap();

            if let Some(params) = device.detect_bar_reprogramming(register, data) {
                // if let Err(e) = pci_bus.device_reloc.move_bar(
                //     params.old_base,
                //     params.new_base,
                //     params.len,
                //     device.deref_mut(),
                //     params.region_type,
                // ) {
                //     error!(
                //         "Failed moving device BAR: {}: 0x{:x}->0x{:x}(0x{:x})",
                //         e, params.old_base, params.new_base, params.len
                //     );
                // }
                error!(
                    "Failed moving device BAR: 0x{:x}->0x{:x}(0x{:x})",
                    params.old_base, params.new_base, params.len
                );
            }
            // Update the register value
            device.write_config_register(register, offset, data)
        } else {
            None
        }
    }

    fn set_config_address(&mut self, offset: u64, data: &[u8]) {
        if offset as usize + data.len() > 4 {
            return;
        }
        let (mask, value): (u32, u32) = match data.len() {
            1 => (
                0x0000_00ff << (offset * 8),
                u32::from(data[0]) << (offset * 8),
            ),
            2 => (
                0x0000_ffff << (offset * 16),
                (u32::from(data[1]) << 8 | u32::from(data[0])) << (offset * 16),
            ),
            4 => (0xffff_ffff, LittleEndian::read_u32(data)),
            _ => return,
        };
        self.config_address = (self.config_address & !mask) | value;
    }
}

impl MutDevicePio for PciConfigIo {
    fn pio_read(&mut self, _: PioAddress, offset: u16, data: &mut [u8]) {
        // `offset` is relative to 0xcf8
        let value = match offset {
            0..=3 => self.config_address,
            4..=7 => self.config_space_read(),
            _ => 0xffff_ffff,
        };

        // Only allow reads to the register boundary.
        let start = offset as usize % 4;
        let end = start + data.len();
        if end <= 4 {
            for i in start..end {
                data[i - start] = (value >> (i * 8)) as u8;
            }
        } else {
            for d in data {
                *d = 0xff;
            }
        }
    }

    fn pio_write(&mut self, _: PioAddress, offset: u16, data: &[u8]) {
        // `offset` is relative to 0xcf8
        match offset {
            o @ 0..=3 => {
                self.set_config_address(o.into(), data);
            }
            o @ 4..=7 => {
                self.config_space_write((o - 4).into(), data);
            }
            _ => {}
        }
    }
}

/// Emulates PCI memory-mapped configuration access mechanism.
pub struct PciConfigMmio {
    pci_bus: Arc<Mutex<PciBus>>,
}

impl PciConfigMmio {
    pub fn new(pci_bus: Arc<Mutex<PciBus>>) -> Self {
        PciConfigMmio { pci_bus }
    }

    fn config_space_read(&self, config_address: u32) -> u32 {
        let (bus, device, _function, register) = parse_mmio_config_address(config_address);

        // Only support one bus.
        if bus != 0 {
            return 0xffff_ffff;
        }

        self.pci_bus
            .lock()
            .unwrap()
            .devices
            .get(&(device as u32))
            .map_or(0xffff_ffff, |d| {
                d.lock().unwrap().read_config_register(register)
            })
    }

    fn config_space_write(&mut self, config_address: u32, offset: u64, data: &[u8]) {
        if offset as usize + data.len() > 4 {
            return;
        }

        let (bus, device, _function, register) = parse_mmio_config_address(config_address);

        // Only support one bus.
        if bus != 0 {
            return;
        }

        let pci_bus = self.pci_bus.lock().unwrap();
        if let Some(d) = pci_bus.devices.get(&(device as u32)) {
            let mut device = d.lock().unwrap();

            if let Some(params) = device.detect_bar_reprogramming(register, data) {
                // if let Err(e) = pci_bus.device_reloc.move_bar(
                //     params.old_base,
                //     params.new_base,
                //     params.len,
                //     device.deref_mut(),
                //     params.region_type,
                // ) {
                //     error!(
                //         "Failed moving device BAR: {}: 0x{:x}->0x{:x}(0x{:x})",
                //         e, params.old_base, params.new_base, params.len
                //     );
                // }
                error!(
                    "Failed moving device BAR: 0x{:x}->0x{:x}(0x{:x})",
                    params.old_base, params.new_base, params.len
                );
            }

            // Update the register value
            device.write_config_register(register, offset, data);
        }
    }
}

impl MutDeviceMmio for PciConfigMmio {
    fn mmio_read(&mut self, _: MmioAddress, offset: u64, data: &mut [u8]) {
        // Only allow reads to the register boundary.
        let start = offset as usize % 4;
        let end = start + data.len();
        if end > 4 || offset > u64::from(u32::max_value()) {
            for d in data {
                *d = 0xff;
            }
            return;
        }

        let value = self.config_space_read(offset as u32);
        for i in start..end {
            data[i - start] = (value >> (i * 8)) as u8;
        }
    }

    fn mmio_write(&mut self, _: MmioAddress, offset: u64, data: &[u8]) {
        if offset > u64::from(u32::max_value()) {
            return;
        }
        self.config_space_write(offset as u32, offset % 4, data);
    }
}

fn shift_and_mask(value: u32, offset: usize, mask: u32) -> usize {
    ((value >> offset) & mask) as usize
}

// Parse the MMIO address offset to a (bus, device, function, register) tuple.
// See section 7.2.2 PCI Express Enhanced Configuration Access Mechanism (ECAM)
// from the Pci Express Base Specification Revision 5.0 Version 1.0.
fn parse_mmio_config_address(config_address: u32) -> (usize, usize, usize, usize) {
    const BUS_NUMBER_OFFSET: usize = 20;
    const BUS_NUMBER_MASK: u32 = 0x00ff;
    const DEVICE_NUMBER_OFFSET: usize = 15;
    const DEVICE_NUMBER_MASK: u32 = 0x1f;
    const FUNCTION_NUMBER_OFFSET: usize = 12;
    const FUNCTION_NUMBER_MASK: u32 = 0x07;
    const REGISTER_NUMBER_OFFSET: usize = 2;
    const REGISTER_NUMBER_MASK: u32 = 0x3ff;

    (
        shift_and_mask(config_address, BUS_NUMBER_OFFSET, BUS_NUMBER_MASK),
        shift_and_mask(config_address, DEVICE_NUMBER_OFFSET, DEVICE_NUMBER_MASK),
        shift_and_mask(config_address, FUNCTION_NUMBER_OFFSET, FUNCTION_NUMBER_MASK),
        shift_and_mask(config_address, REGISTER_NUMBER_OFFSET, REGISTER_NUMBER_MASK),
    )
}

// Parse the CONFIG_ADDRESS register to a (bus, device, function, register) tuple.
fn parse_io_config_address(config_address: u32) -> (usize, usize, usize, usize) {
    const BUS_NUMBER_OFFSET: usize = 16;
    const BUS_NUMBER_MASK: u32 = 0x00ff;
    const DEVICE_NUMBER_OFFSET: usize = 11;
    const DEVICE_NUMBER_MASK: u32 = 0x1f;
    const FUNCTION_NUMBER_OFFSET: usize = 8;
    const FUNCTION_NUMBER_MASK: u32 = 0x07;
    const REGISTER_NUMBER_OFFSET: usize = 2;
    const REGISTER_NUMBER_MASK: u32 = 0x3f;

    (
        shift_and_mask(config_address, BUS_NUMBER_OFFSET, BUS_NUMBER_MASK),
        shift_and_mask(config_address, DEVICE_NUMBER_OFFSET, DEVICE_NUMBER_MASK),
        shift_and_mask(config_address, FUNCTION_NUMBER_OFFSET, FUNCTION_NUMBER_MASK),
        shift_and_mask(config_address, REGISTER_NUMBER_OFFSET, REGISTER_NUMBER_MASK),
    )
}