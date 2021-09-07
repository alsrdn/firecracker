// Copyright 2021 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

//! Rust-vmm vm-device wrappers/adapters.

use crate::virtio::AsAny;
use kvm_bindings::kvm_irq_routing;
use vm_device::{MutDeviceMmio, MutDevicePio};

/// PioDevice will implement AsAny as required to cast Bus devices
/// to concrete MmioTransport later.
pub trait PioDevice: MutDevicePio + AsAny + Send {
    fn interrupt(&self, _irq_mask: u32) -> std::io::Result<()> {
        Ok(())
    }
}

pub trait MmioDevice: MutDeviceMmio + AsAny + Send {
    fn interrupt(&self, _irq_mask: u32) -> std::io::Result<()> {
        Ok(())
    }
}

impl crate::PioDevice for pci::PciConfigIo {}
impl crate::MmioDevice for pci::PciConfigMmio {}
impl MmioDevice for pci::VfioPciDevice<kvm_irq_routing> {}
