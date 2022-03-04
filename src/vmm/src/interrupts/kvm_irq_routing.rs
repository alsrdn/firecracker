use crate::interrupts::kvm_irqchip::{IoApic, XtPic};
use kvm_bindings::{
    kvm_irq_routing, kvm_irq_routing_entry, KVM_IRQCHIP_IOAPIC, KVM_IRQCHIP_PIC_MASTER,
    KVM_IRQCHIP_PIC_SLAVE, KVM_IRQ_ROUTING_IRQCHIP, KVM_IRQ_ROUTING_MSI, KVM_MSI_VALID_DEVID,
};
use kvm_ioctls::VmFd;
use std::collections::HashMap;
use std::fmt::{self, Display};
use std::mem::size_of;
use std::sync::Arc;

#[derive(Debug)]
pub enum Error {
    GsiRoutingError(std::io::Error),
    PinAllocationError,
}

impl std::error::Error for Error {}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::GsiRoutingError(err) => write!(f, "Cannot configure GSI routing: {}", err),
            Error::PinAllocationError => write!(
                f,
                "Interrupt cannot be routed because no free pin was found for the interrupt."
            ),
        }
    }
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
    /// Allocator for IoApic pins
    ioapic: IoApic,
    /// Allocator for XtPic pins
    xt_pic: XtPic,
}

impl KvmIrqRoutingTable {
    /// Maximum supported GSI routes on KVM
    pub const MAX_ROUTES: usize = 4096;

    /// Generates an unique hash key for a kvm_irq_routing_entry.
    ///
    /// In some cases the same GSI is used by multiple IRQ chips and require that we
    /// add more than one kvm_irq_routing_entry for each GSI.
    /// For example the first 16 pins of the Intel XtPIC are also routed to the IOAPIC.
    ///
    /// This hash function combines the entry type, irqchip and the GSI in order to
    /// obtain a unique id in the case of IRQCHIP routing.
    /// For other types of routing, hashing is based only on entry type and GSI.
    fn hash_key(entry: &kvm_irq_routing_entry) -> u64 {
        let irq_chip = match entry.type_ {
            // Safe because when the entry type is KVM_IRQ_ROUTING_IRQCHIP, the union
            // will contain a valid kvm_irq_routing_irqchip field.
            kvm_bindings::KVM_IRQ_ROUTING_IRQCHIP => unsafe { entry.u.irqchip.irqchip },
            _ => 0u32,
        };
        (u64::from(irq_chip) << 48 | u64::from(entry.type_) << 32) | u64::from(entry.gsi)
    }

    /// Create a new empty KVM IRQ routing table.
    ///
    /// This will reset any previous routing entries that were set for the `vm_fd`.
    /// Returns a `KvmIrqRoutingTable` object that can be used to manager IRQ routes.
    /// Returns an error if the current routing table cannot be reset.
    pub fn new(vm_fd: Arc<VmFd>) -> Result<Self, Error> {
        let table = KvmIrqRoutingTable {
            vm_fd,
            routes: HashMap::new(),
            ioapic: IoApic::new(),
            xt_pic: XtPic::new(),
        };
        table.set_routing().map_err(|e| Error::GsiRoutingError(e))?;

        Ok(table)
    }

    /// Add or modify a KVM routing entry for a MSI interrupt.
    pub fn route_msi(
        &mut self,
        gsi: u32,
        high_addr: u32,
        low_addr: u32,
        data: u32,
        devid: u32,
    ) -> Result<(), Error> {
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
        self.set_routing().map_err(|e| {
            self.routes.remove(&key);
            Error::GsiRoutingError(e)
        })
    }

    /// Add a routing entry for an INTx interrupt.
    ///
    /// INTx interrupts can be shared.
    /// We only add INTx interrupts to the IOAPIC.
    pub fn route_intx(&mut self, gsi: u32, _intx: u8, pin: Option<u32>) -> Result<u32, Error> {
        let mut entry = kvm_irq_routing_entry {
            gsi,
            type_: KVM_IRQ_ROUTING_IRQCHIP,
            ..Default::default()
        };
        entry.u.irqchip.irqchip = KVM_IRQCHIP_IOAPIC;
        if let Some(pin) = self.ioapic.allocate_pin(true, pin) {
            entry.u.irqchip.pin = pin;
            let key = Self::hash_key(&entry);
            self.routes.insert(key, entry);
            self.set_routing().map_err(|e| {
                self.routes.remove(&key);
                Error::GsiRoutingError(e)
            })?;
            Ok(pin)
        } else {
            Err(Error::PinAllocationError)
        }
    }

    /// Add a routing entry for a legacy interrupt.
    ///
    /// Legacy interrupts cannot be shared.
    /// Returns the interrupt line that was allocated for this interrupt.
    /// Callers need to know the interrupt line in some cases in order to configure
    /// interrupt tables or drivers. One example is adding virtio devices to the
    /// Linux kernel command line. The virtio device needs to know which interrupt line
    /// the device was allocated to in order to correctly perform `request_irq()`.
    pub fn route_generic(&mut self, gsi: u32, pin: Option<u32>) -> Result<u32, Error> {
        let mut ioapic_request_pin = pin;
        // The interrupt line was not set yet. The fu
        let mut interrupt_line = None;

        // First try to allocate a pin from the XT-PIC.
        // We add the XT-PIC entry for kernel that boot without IOAPIC support.
        match self.xt_pic.allocate_pin(pin) {
            Some(pic_pin) => {
                // If an available pin was found in the XT-PIC we add the routing entry
                let mut pic_entry = kvm_irq_routing_entry {
                    gsi: gsi,
                    type_: KVM_IRQ_ROUTING_IRQCHIP,
                    ..Default::default()
                };

                // Select the propper IRQ chip to add in the entry
                match pic_pin {
                    0..=7 => {
                        pic_entry.u.irqchip.irqchip = KVM_IRQCHIP_PIC_MASTER;
                        pic_entry.u.irqchip.pin = pic_pin;
                    }
                    8..=15 => {
                        pic_entry.u.irqchip.irqchip = KVM_IRQCHIP_PIC_SLAVE;
                        pic_entry.u.irqchip.pin = pic_pin - 8;
                    }
                    _ => {}
                };

                // Submit the entry to KVM
                let key = Self::hash_key(&pic_entry);
                self.routes.insert(key, pic_entry);
                self.set_routing().map_err(|e| {
                    self.routes.remove(&key);
                    Error::GsiRoutingError(e)
                })?;

                // The pin was assigned to the interrupt. Save it so it can be returned.
                interrupt_line = Some(pic_pin);
                // We'll want to add the same entry for the IOAPIC. Because the requested pin
                // may have been `None`, we'll save the value here.
                ioapic_request_pin = Some(pic_pin);
            }
            None => {}
        };

        // By this point an entry was added for the XT-PIC or it failed.
        // Either way `ioapic_request_pin` should have a valid value, so we can try to
        // assign the interrupt only to the IOAPIC. If this happens a guest won't work
        // properly if it was booted with the `noapic` kernel cmdline parameter but
        // will work just fine if ioapic support is enabled.
        match self.ioapic.allocate_pin(false, ioapic_request_pin) {
            Some(ioapic_pin) => {
                let mut ioapic_entry = kvm_irq_routing_entry {
                    gsi: ioapic_pin,
                    type_: KVM_IRQ_ROUTING_IRQCHIP,
                    ..Default::default()
                };
                ioapic_entry.u.irqchip.irqchip = KVM_IRQCHIP_IOAPIC;
                ioapic_entry.u.irqchip.pin = ioapic_pin as u32;
                let key = Self::hash_key(&ioapic_entry);
                self.routes.insert(key, ioapic_entry);

                // `set_routing` might fail but an entry may have already been succesfully
                // added for the XT-PIC. If that's not the case we return an error, otherwise
                // the routing partially succeded and there's no reason to return an error.
                match self.set_routing() {
                    Err(e) => {
                        self.routes.remove(&key);
                        if interrupt_line.is_none() {
                            return Err(Error::GsiRoutingError(e));
                        }
                    }
                    Ok(_) => {
                        interrupt_line = Some(ioapic_pin);
                    }
                }
            }
            None => {}
        }

        // Return an error in all allocations failed.
        interrupt_line.ok_or(Error::PinAllocationError)
    }

    /// Commit routing table to KVM
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
}
