use allocators::GsiAllocator;
use kvm_ioctls::VmFd;
use std::result::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use utils::eventfd::EventFd;
use vm_device::interrupt::Error as VmDeviceError;
use vm_device::interrupt::{
    legacy::LegacyIrqConfig, msi::MsiIrqConfig, ConfigurableInterrupt, Interrupt,
    InterruptSourceGroup, MaskableInterrupt,
};

use crate::interrupts::kvm_irq_routing::KvmIrqRoutingTable;

pub mod kvm_irq_routing;
pub mod kvm_irqchip;

pub struct KvmInterrupt {
    gsi: u32,
    irq_fd: EventFd,
    vm_fd: Arc<VmFd>,
    routing_table: Arc<Mutex<KvmIrqRoutingTable>>,
    registered: AtomicBool,
    configured: AtomicBool,
}

pub struct KvmMsiInterrupt {
    irq: KvmInterrupt,
    config: Mutex<Option<MsiIrqConfig>>,
}

impl KvmMsiInterrupt {
    pub fn new(
        gsi: u32,
        vm_fd: Arc<VmFd>,
        routing_table: Arc<Mutex<KvmIrqRoutingTable>>,
    ) -> Result<Self, std::io::Error> {
        let interrupt = KvmInterrupt::new(gsi, vm_fd, routing_table).unwrap();
        Ok(KvmMsiInterrupt {
            irq: interrupt,
            config: Mutex::new(None),
        })
    }
}

impl Interrupt for KvmMsiInterrupt {
    type NotifierType = EventFd;

    fn trigger(&self) -> Result<(), VmDeviceError> {
        self.irq.irq_fd.write(1).map_err(|_| VmDeviceError::InterruptNotTriggered)?;
        Ok(())
    }

    fn notifier(&self) -> Option<EventFd> {
        Some(
            self.irq
                .irq_fd
                .try_clone()
                .expect("Failed cloning interrupt's EventFd"),
        )
    }

    fn enable(&self) -> Result<(), VmDeviceError> {
        self.irq
            .register_irqfd()
            .map_err(|_| VmDeviceError::InterruptNotChanged)?;
        Ok(())
    }

    fn disable(&self) -> Result<(), VmDeviceError> {
        self.irq
            .unregister_irqfd()
            .map_err(|_| VmDeviceError::InterruptNotChanged)?;
        Ok(())
    }
}

impl MaskableInterrupt for KvmMsiInterrupt {
    fn mask(&self) -> Result<(), VmDeviceError> {
        self.disable()
    }

    fn unmask(&self) -> Result<(), VmDeviceError> {
        self.enable()
    }
}

impl ConfigurableInterrupt for KvmMsiInterrupt {
    type Cfg = MsiIrqConfig;

    fn update(&self, cfg: &MsiIrqConfig) -> Result<(), VmDeviceError> {
        if self.irq.registered.load(Ordering::Acquire) {
            return Err(VmDeviceError::InvalidConfiguration);
        }
        let mut routing_table = self.irq.routing_table.lock().expect("kk");
        routing_table.route_msi(
            self.irq.gsi,
            cfg.low_addr,
            cfg.high_addr,
            cfg.data,
            cfg.devid,
        );
        Ok(())
    }

    fn get_config(&self) -> Result<Self::Cfg, VmDeviceError> {
        let config = self.config.lock().expect("Poisoned Lock");
        if let Some(cfg) = *config {
            Ok(cfg)
        } else {
            Err(VmDeviceError::InvalidConfiguration)
        }
    }
}

pub struct KvmLegacyInterrupt {
    irq: KvmInterrupt,
    config: Mutex<Option<LegacyIrqConfig>>,
}

impl KvmLegacyInterrupt {
    pub fn new(
        gsi: u32,
        vm_fd: Arc<VmFd>,
        routing_table: Arc<Mutex<KvmIrqRoutingTable>>,
    ) -> Result<Self, std::io::Error> {
        let interrupt = KvmInterrupt::new(gsi, vm_fd, routing_table).unwrap();
        Ok(KvmLegacyInterrupt {
            irq: interrupt,
            config: Mutex::new(None),
        })
    }
}

impl Interrupt for KvmLegacyInterrupt {
    type NotifierType = EventFd;

    fn trigger(&self) -> Result<(), VmDeviceError> {
        self.irq.irq_fd.write(1).map_err(|_| VmDeviceError::InterruptNotTriggered)?;
        Ok(())
    }

    fn notifier(&self) -> Option<Self::NotifierType> {
        Some(
            self.irq
                .irq_fd
                .try_clone()
                .expect("Failed cloning interrupt's EventFd"),
        )
    }

    fn enable(&self) -> Result<(), VmDeviceError> {
        if !self.irq.configured.load(Ordering::Acquire) {
            return Err(VmDeviceError::InterruptNotChanged);
        }

        self.irq
            .register_irqfd()
            .map_err(|_| VmDeviceError::InterruptNotChanged)?;
        Ok(())
    }

    fn disable(&self) -> Result<(), VmDeviceError> {
        self.irq
            .unregister_irqfd()
            .map_err(|_| VmDeviceError::InterruptNotChanged)?;
        Ok(())
    }
}

impl ConfigurableInterrupt for KvmLegacyInterrupt {
    type Cfg = LegacyIrqConfig;

    fn update(&self, cfg: &LegacyIrqConfig) -> Result<(), VmDeviceError> {
        let gsi = self.irq.gsi;
        let mut routing_table = self.irq.routing_table.lock().expect("Poisoned Lock");
        let mut config = self.config.lock().expect("Poisoned Lock");

        if let Some(intx) = cfg.interrupt_pin {
            routing_table.route_intx(gsi, intx as u8, cfg.interrupt_line);
            *config = Some(*cfg);
        } else {
            let line = routing_table.route_generic(gsi, cfg.interrupt_line);
            *config = Some(LegacyIrqConfig {
                interrupt_line: Some(line),
                interrupt_pin: None,
            });
        }
        self.irq.configured.store(true, Ordering::Release);

        Ok(())
    }

    fn get_config(&self) -> Result<Self::Cfg, VmDeviceError> {
        let config = self.config.lock().expect("Poisoned Lock");

        if let Some(cfg) = *config {
            Ok(cfg)
        } else {
            Err(VmDeviceError::InvalidConfiguration)
        }
    }
}

impl KvmInterrupt {
    pub fn new(
        gsi: u32,
        vm_fd: Arc<VmFd>,
        routing_table: Arc<Mutex<KvmIrqRoutingTable>>,
    ) -> Result<Self, std::io::Error> {
        let irq_fd = EventFd::new(libc::EFD_NONBLOCK)?;
        Ok(KvmInterrupt {
            gsi,
            irq_fd,
            vm_fd: vm_fd.clone(),
            routing_table,
            registered: AtomicBool::new(false),
            configured: AtomicBool::new(false),
        })
    }

    fn register_irqfd(&self) -> Result<(), std::io::Error> {
        if !self.registered.load(Ordering::Acquire) {
            self.vm_fd.register_irqfd(&self.irq_fd, self.gsi)?;

            // Update internals to track the irq_fd as "registered".
            self.registered.store(true, Ordering::Release);
        }

        Ok(())
    }

    fn unregister_irqfd(&self) -> Result<(), std::io::Error> {
        if self.registered.load(Ordering::Acquire) {
            self.vm_fd.unregister_irqfd(&self.irq_fd, self.gsi)?;

            // Update internals to track the irq_fd as "unregistered".
            self.registered.store(false, Ordering::Release);
        }

        Ok(())
    }
}

pub struct KvmMsiInterruptGroup {
    allocator: Arc<Mutex<GsiAllocator>>,
    routing_table: Arc<Mutex<KvmIrqRoutingTable>>,
    interrupts: Vec<Arc<KvmMsiInterrupt>>,
    vm_fd: Arc<VmFd>,
}

impl KvmMsiInterruptGroup {
    pub fn new(
        allocator: Arc<Mutex<GsiAllocator>>,
        routing_table: Arc<Mutex<KvmIrqRoutingTable>>,
        vm_fd: Arc<VmFd>,
    ) -> Self {
        KvmMsiInterruptGroup {
            allocator,
            routing_table,
            interrupts: Vec::new(),
            vm_fd,
        }
    }
}

impl InterruptSourceGroup for KvmMsiInterruptGroup {
    type InterruptType = KvmMsiInterrupt;
    type InterruptWrapper = Arc<Self::InterruptType>;

    fn is_empty(&self) -> bool {
        self.interrupts.is_empty()
    }

    fn len(&self) -> usize {
        self.interrupts.len() as usize
    }

    fn enable(&self) -> std::result::Result<(), VmDeviceError> {
        for int in &self.interrupts {
            int.enable()?;
        }
        Ok(())
    }

    fn disable(&self) -> std::result::Result<(), VmDeviceError> {
        for int in self.interrupts.iter() {
            int.disable()?;
        }
        Ok(())
    }

    fn get(&self, index: usize) -> Option<Self::InterruptWrapper> {
        let int = self.interrupts.get(index as usize).unwrap();
        Some(int.clone())
    }

    fn allocate_interrupts(&mut self, size: usize) -> Result<(), vm_device::interrupt::Error> {
        let mut allocator = self.allocator.lock().unwrap();

        for _ in 0..size {
            let gsi = allocator.allocate_gsi().unwrap();

            let interrupt =
                KvmMsiInterrupt::new(gsi, self.vm_fd.clone(), self.routing_table.clone()).unwrap();
            self.interrupts.push(Arc::new(interrupt));
        }
        Ok(())
    }

    fn free_interrupts(&mut self) -> Result<(), VmDeviceError> {
        Ok(())
    }
}

pub struct KvmLegacyInterruptGroup {
    allocator: Arc<Mutex<GsiAllocator>>,
    routing_table: Arc<Mutex<KvmIrqRoutingTable>>,
    interrupts: Vec<Arc<KvmLegacyInterrupt>>,
    vm_fd: Arc<VmFd>,
}

impl KvmLegacyInterruptGroup {
    pub fn new(
        allocator: Arc<Mutex<GsiAllocator>>,
        routing_table: Arc<Mutex<KvmIrqRoutingTable>>,
        vm_fd: Arc<VmFd>,
    ) -> Self {
        KvmLegacyInterruptGroup {
            allocator,
            routing_table,
            interrupts: Vec::new(),
            vm_fd,
        }
    }
}

impl InterruptSourceGroup for KvmLegacyInterruptGroup {
    type InterruptType = KvmLegacyInterrupt;
    type InterruptWrapper = Arc<Self::InterruptType>;

    fn is_empty(&self) -> bool {
        self.interrupts.is_empty()
    }

    fn len(&self) -> usize {
        self.interrupts.len()
    }

    fn enable(&self) -> std::result::Result<(), vm_device::interrupt::Error> {
        for int in &self.interrupts {
            int.enable()?;
        }
        Ok(())
    }

    fn disable(&self) -> std::result::Result<(), vm_device::interrupt::Error> {
        for int in self.interrupts.iter() {
            int.disable()?;
        }
        Ok(())
    }

    fn get(&self, index: usize) -> Option<Self::InterruptWrapper> {
        let int = self.interrupts.get(index as usize).unwrap();
        Some(int.clone())
    }

    fn allocate_interrupts(&mut self, size: usize) -> Result<(), vm_device::interrupt::Error> {
        let mut allocator = self.allocator.lock().unwrap();

        for _ in 0..size {
            let gsi = allocator.allocate_gsi().unwrap();

            let interrupt =
                KvmLegacyInterrupt::new(gsi, self.vm_fd.clone(), self.routing_table.clone())
                    .unwrap();
            self.interrupts.push(Arc::new(interrupt));
        }
        Ok(())
    }

    fn free_interrupts(&mut self) -> Result<(), VmDeviceError> {
        Ok(())
    }
}

pub struct KvmInterruptManager {
    allocator: Arc<Mutex<GsiAllocator>>,
    vm_fd: Arc<VmFd>,
    routing_table: Arc<Mutex<KvmIrqRoutingTable>>,
}

impl KvmInterruptManager {
    pub fn new(vm_fd: Arc<VmFd>) -> Self {
        KvmInterruptManager {
            allocator: Arc::new(Mutex::new(GsiAllocator::new(1, 1024))),
            vm_fd: vm_fd.clone(),
            routing_table: Arc::new(Mutex::new(KvmIrqRoutingTable::new(vm_fd))),
        }
    }

    pub fn get_new_msi_group(&self) -> crate::Result<KvmMsiInterruptGroup> {
        let new_grp = KvmMsiInterruptGroup::new(
            self.allocator.clone(),
            self.routing_table.clone(),
            self.vm_fd.clone(),
        );
        Ok(new_grp)
    }

    pub fn get_new_legacy_group(&self) -> crate::Result<KvmLegacyInterruptGroup> {
        let new_grp = KvmLegacyInterruptGroup::new(
            self.allocator.clone(),
            self.routing_table.clone(),
            self.vm_fd.clone(),
        );
        Ok(new_grp)
    }
}
