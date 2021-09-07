// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use vm_memory::GuestAddress;
use vmm_sys_util::eventfd::EventFd;

#[derive(Debug)]
pub enum HypervisorError {
    SetGsiRouting,
    RegisterIrqFd,
    UnregisterIrqFd,
}

impl std::fmt::Display for HypervisorError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match *self {
            HypervisorError::SetGsiRouting => {
                write!(f, "Failed to set routing")
            }
            HypervisorError::RegisterIrqFd => {
                write!(f, "Failed to register irq fd")
            }
            HypervisorError::UnregisterIrqFd => {
                write!(f, "Failed to unregister irq fd")
            }
        }
    }
}

/// Trait that abstracts high level Hypervisor functionality.
pub trait Hypervisor: Sync + Send {
    /// Platform specific IRQ routing rules. This is part of hypervisor specific API
    /// for setting up interrupt emulation.
    type IrqRouting;

    fn map_device_memory_region(
        &mut self,
        slot: u32,
        hva: u64,
        gpa: GuestAddress,
        len: u64,
        readonly: bool,
    ) -> std::result::Result<(), std::io::Error>;

    fn new_mem_slot(&mut self) -> u32;
    
    fn set_gsi_routing(
        &mut self,
        irq_routing: &Self::IrqRouting,
    ) -> std::result::Result<(), HypervisorError>;

    fn register_irqfd(&self, fd: &EventFd, gsi: u32) -> std::result::Result<(), HypervisorError>;
    fn unregister_irqfd(&self, fd: &EventFd, gsi: u32) -> std::result::Result<(), HypervisorError>;
}
