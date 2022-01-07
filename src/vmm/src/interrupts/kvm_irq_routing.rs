use crate::interrupts::kvm_irqchip::{IoApic, XtPic};
use kvm_bindings::{
    kvm_irq_routing, kvm_irq_routing_entry, KVM_IRQCHIP_IOAPIC, KVM_IRQCHIP_PIC_MASTER,
    KVM_IRQCHIP_PIC_SLAVE, KVM_IRQ_ROUTING_IRQCHIP, KVM_IRQ_ROUTING_MSI, KVM_MSI_VALID_DEVID,
};
use kvm_ioctls::VmFd;
use std::collections::HashMap;
use std::mem::size_of;
use std::sync::Arc;

pub enum Error {
    GsiRoutingError(std::io::Error),
}

// The kvm API has many structs that resemble the following `Foo` structure:
//
// ```
// #[repr(C)]
// struct Foo {
//    some_data: u32
//    entries: __IncompleteArrayField<__u32>,
// }
// ```
//
// In order to allocate such a structure, `size_of::<Foo>()` would be too small because it would not
// include any space for `entries`. To make the allocation large enough while still being aligned
// for `Foo`, a `Vec<Foo>` is created. Only the first element of `Vec<Foo>` would actually be used
// as a `Foo`. The remaining memory in the `Vec<Foo>` is for `entries`, which must be contiguous
// with `Foo`. This function is used to make the `Vec<Foo>` with enough space for `count` entries.
/// Helper function to create Vec of specific size.
pub fn vec_with_array_field<T: Default, F>(count: usize) -> Vec<T> {
    let element_space = count * size_of::<F>();
    let vec_size_bytes = size_of::<T>() + element_space;
    vec_with_size_in_bytes(vec_size_bytes)
}

// Returns a `Vec<T>` with a size in bytes at least as large as `size_in_bytes`.
fn vec_with_size_in_bytes<T: Default>(size_in_bytes: usize) -> Vec<T> {
    let rounded_size = (size_in_bytes + size_of::<T>() - 1) / size_of::<T>();
    let mut v = Vec::with_capacity(rounded_size);
    v.resize_with(rounded_size, T::default);
    v
}

/// Manages KVM GSI routing table entries.
/// See documentation for KVM_SET_GSI_ROUTING.
pub struct KvmIrqRoutingTable {
    vm_fd: Arc<VmFd>,
    routes: HashMap<u64, kvm_irq_routing_entry>,
    ioapic: IoApic,
    xt_pic: XtPic,
}

impl KvmIrqRoutingTable {
    pub const MAX_ROUTES: usize = 4096;

    fn hash_key(entry: &kvm_irq_routing_entry) -> u64 {
        let type1 = match entry.type_ {
            kvm_bindings::KVM_IRQ_ROUTING_IRQCHIP => unsafe { entry.u.irqchip.irqchip },
            _ => 0u32,
        };
        (u64::from(type1) << 48 | u64::from(entry.type_) << 32) | u64::from(entry.gsi)
    }

    pub fn new(vm_fd: Arc<VmFd>) -> Self {
        let table = KvmIrqRoutingTable {
            vm_fd,
            routes: HashMap::new(),
            ioapic: IoApic::new(),
            xt_pic: XtPic::new(),
        };
        table.set_routing().unwrap();

        table
    }

    pub fn route_msi(&mut self, gsi: u32, high_addr: u32, low_addr: u32, data: u32, devid: u32) {
        let mut entry = kvm_irq_routing_entry {
            gsi,
            type_: KVM_IRQ_ROUTING_MSI,
            ..Default::default()
        };

        entry.u.msi.address_lo = low_addr;
        entry.u.msi.address_hi = high_addr;
        entry.u.msi.data = data;

        entry.flags = KVM_MSI_VALID_DEVID;
        entry.u.msi.__bindgen_anon_1.devid = devid;

        let key = Self::hash_key(&entry);
        self.routes
            .entry(key)
            .and_modify(|e| *e = entry)
            .or_insert(entry);
        self.set_routing().unwrap();
    }

    pub fn route_intx(&mut self, gsi: u32, _intx: u8, pin: Option<u32>) {
        let mut entry = kvm_irq_routing_entry {
            gsi,
            type_: KVM_IRQ_ROUTING_IRQCHIP,
            ..Default::default()
        };
        entry.u.irqchip.irqchip = KVM_IRQCHIP_IOAPIC;
        entry.u.irqchip.pin = self.ioapic.allocate_pin(true, pin).unwrap() as u32;
        self.routes.insert(Self::hash_key(&entry), entry);
        self.set_routing().unwrap();
    }

    pub fn route_generic(&mut self, gsi: u32, pin: Option<u32>) -> u32 {
        let mut ioapic_request_pin = pin;
        let mut interrupt_line = 0;

        match self.xt_pic.allocate_pin(pin) {
            Some(pic_pin) => {
                interrupt_line = pic_pin;
                let mut pic_entry = kvm_irq_routing_entry {
                    gsi: gsi,
                    type_: KVM_IRQ_ROUTING_IRQCHIP,
                    ..Default::default()
                };
                match pic_pin {
                    0..=7 => {
                        pic_entry.u.irqchip.irqchip = KVM_IRQCHIP_PIC_MASTER;
                        pic_entry.u.irqchip.pin = pic_pin as u32;
                    }
                    8..=15 => {
                        pic_entry.u.irqchip.irqchip = KVM_IRQCHIP_PIC_SLAVE;
                        pic_entry.u.irqchip.pin = pic_pin as u32 - 8;
                    }
                    _ => {}
                };
                self.routes.insert(Self::hash_key(&pic_entry), pic_entry);
                ioapic_request_pin = Some(pic_pin);
            }
            None => {}
        };

        match self.ioapic.allocate_pin(false, ioapic_request_pin) {
            Some(ioapic_pin) => {
                interrupt_line = ioapic_pin;
                let mut ioapic_entry = kvm_irq_routing_entry {
                    gsi: ioapic_pin,
                    type_: KVM_IRQ_ROUTING_IRQCHIP,
                    ..Default::default()
                };
                ioapic_entry.u.irqchip.irqchip = KVM_IRQCHIP_IOAPIC;
                ioapic_entry.u.irqchip.pin = ioapic_pin as u32;
                self.routes
                    .insert(Self::hash_key(&ioapic_entry), ioapic_entry);
            }
            None => {}
        }
        self.set_routing().unwrap();
        interrupt_line
    }

    fn set_routing(&self) -> std::result::Result<(), std::io::Error> {
        let entry_vec = self
            .routes
            .values()
            .cloned()
            .collect::<Vec<kvm_irq_routing_entry>>();
        let mut irq_routing =
            vec_with_array_field::<kvm_irq_routing, kvm_irq_routing_entry>(entry_vec.len());
        irq_routing[0].nr = entry_vec.len() as u32;
        irq_routing[0].flags = 0;

        unsafe {
            let entries_slice: &mut [kvm_irq_routing_entry] =
                irq_routing[0].entries.as_mut_slice(entry_vec.len());
            entries_slice.copy_from_slice(&entry_vec);
        }

        self.vm_fd.set_gsi_routing(&irq_routing[0])?;

        Ok(())
    }

    pub fn add(
        &mut self,
        entry: &kvm_irq_routing_entry,
    ) -> std::result::Result<(), std::io::Error> {
        // Safe to unwrap because there's no legal way to break the mutex.
        //let mut routes = self.routes.lock().unwrap();
        let entry_key = Self::hash_key(entry);
        if self.routes.contains_key(&entry_key) || self.routes.len() == Self::MAX_ROUTES {
            return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
        } else {
            self.routes.insert(entry_key, *entry);
        }

        self.set_routing()
    }

    pub fn remove(
        &mut self,
        entries: &[kvm_irq_routing_entry],
    ) -> std::result::Result<(), std::io::Error> {
        // Safe to unwrap because there's no legal way to break the mutex.
        //let mut routes = self.routes.lock().unwrap();
        for entry in entries {
            let entry_key = Self::hash_key(entry);
            let _ = self.routes.remove(&entry_key);
        }
        self.set_routing()
    }

    pub fn modify(
        &mut self,
        entry: &kvm_irq_routing_entry,
    ) -> std::result::Result<(), std::io::Error> {
        // Safe to unwrap because there's no legal way to break the mutex.
        //let mut routes = self.routes.lock().unwrap();
        let entry_key = Self::hash_key(entry);
        if !self.routes.contains_key(&entry_key) {
            return Err(std::io::Error::from_raw_os_error(libc::ENOENT));
        }

        let _ = self.routes.insert(entry_key, *entry);
        self.set_routing()
    }
}
