use std::collections::{BTreeSet, VecDeque};

/// Struct used for managing IOAPIC pins
pub struct IoApic {
    /// Pins that have not been allocated and are available for use
    available_pins: BTreeSet<u32>,
    /// Pins that are used but can be shared
    shared_pins: VecDeque<u32>,
}

impl IoApic {
    pub fn new() -> Self {
        let mut ioapic = IoApic {
            available_pins: BTreeSet::new(),
            shared_pins: VecDeque::new(),
        };

        // All pins are are available when creating a new IOAPIC
        for i in 1..=arch::IRQ_MAX {
            // IRQ2 is used for XT-PIC chaining
            if i != 2 {
                ioapic.available_pins.insert(i);
            }
        }

        ioapic
    }

    /// Finds an available pin on the IOAPIC and reserves it for use.
    /// If the pin can be shared, it will first try to allocate a pin
    /// that wasn't used. If it can't find one, it will allocate the 
    /// least recently used shared pin.
    pub fn allocate_pin(&mut self, shared: bool, requested_pin: Option<u32>) -> Option<u32> {
        if let Some(rpin) = requested_pin {
            // Check if the requested pin is available
            if self.available_pins.contains(&rpin) {
                self.available_pins.remove(&rpin);
                // Add it to the shared pin list if required
                if shared {
                    self.shared_pins.push_back(rpin);
                }
                return Some(rpin);
            } else if shared {
                // If the pin is not available but can be shared,
                // look it up in the shared pins
                match self.shared_pins.iter().position(|&p| p == rpin) {
                    Some(idx) => {
                        // Add it to the end of the queue
                        self.shared_pins.remove(idx).unwrap();
                        self.shared_pins.push_back(rpin);
                        return Some(rpin);
                    },
                    None => return None,
                }
            }
        }

        // Allocate the next available pin
        if let Some(available_pin) = self.available_pins.iter().next() {
            let pin = *available_pin;
            self.available_pins.remove(&pin);
            if shared {
                self.shared_pins.push_back(pin);
            }
            return Some(pin);
        }

        // If pin is sharable, allocate a pin from the shared list
        if shared {
            let pin = self.shared_pins.pop_front().unwrap();
            self.shared_pins.push_back(pin);
            return Some(pin);
        }

        None
    }
}

/// Struct used for managing XT-PIC pins
/// The XT-PIC is constructed by connecting two Intel 8259 PICs
/// The output of the slave is connected to IRQ2 of the master
pub struct XtPic {
    available_pins: BTreeSet<u32>,
}

impl XtPic {
    pub fn new() -> Self {
        let mut xt_pic = XtPic {
            available_pins: BTreeSet::new(),
        };

        for i in 1..=15 {
            // IRQ2 is used for XT-PIC chaining
            if i != 2 {
                xt_pic.available_pins.insert(i);
            }
        }

        xt_pic
    }

    /// Finds an available pin on the XT-PIC and reserves it for use.
    pub fn allocate_pin(&mut self, requested_pin: Option<u32>) -> Option<u32> {
        if let Some(pin) = requested_pin {
            if self.available_pins.contains(&pin) {
                self.available_pins.remove(&pin);
                return Some(pin);
            } else {
                return None;
            }
        }

        if let Some(available_pin) = self.available_pins.iter().next() {
            let pin = *available_pin;
            self.available_pins.remove(&pin);
            return Some(pin);
        }
        None
    }
}
