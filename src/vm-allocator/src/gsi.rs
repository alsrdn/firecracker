// Copyright © 2019 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

#[cfg(target_arch = "x86_64")]
use std::result;

#[derive(Debug)]
pub enum Error {
    Overflow,
}

pub type Result = result::Result<u32, Error>;

/// Generic interrupt allocator.
pub trait GsiAllocator {
    /// Allocate one global system interrupt.
    fn allocate_gsi(&mut self) -> Result;
    /// Alllocate one irq.
    fn allocate_irq(&mut self) -> Result;
}

/// Default implementation for GsiAllocator
pub struct DefaultGsiAllocator {
    next_irq: u32,
    next_gsi: u32,
    max_irq: u32,
}

impl DefaultGsiAllocator {
    /// New GSI allocator
    pub fn new(max_irq: u32) -> Self {
        DefaultGsiAllocator {
            next_irq: arch::IRQ_BASE,
            #[cfg(target_arch = "x86_64")]
            next_gsi: arch::IRQ_MAX + 1,
            #[cfg(target_arch = "aarch64")]
            next_gsi: arch::IRQ_BASE,
            max_irq,
        }
    }
}

impl GsiAllocator for DefaultGsiAllocator {
    /// Allocate a GSI
    fn allocate_gsi(&mut self) -> Result {
        let gsi = self.next_gsi;
        self.next_gsi = self.next_gsi.checked_add(1).ok_or(Error::Overflow)?;
        Ok(gsi)
    }

    /// Allocate an IRQ
    fn allocate_irq(&mut self) -> Result {
        let irq = self.next_irq;
        if irq > self.max_irq {
            return Err(Error::Overflow);
        }

        // This is safe, as we per above check.
        self.next_irq = self.next_gsi + 1;
        Ok(irq)
    }
}
