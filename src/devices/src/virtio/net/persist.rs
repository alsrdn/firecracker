// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Defines the structures needed for saving/restoring net devices.

use std::io;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use mmds::{ns::MmdsNetworkStack, persist::MmdsNetworkStackState};
use rate_limiter::{persist::RateLimiterState, RateLimiter};
use snapshot::Persist;
use utils::net::mac::{MacAddr, MAC_ADDR_LEN};
use versionize::{VersionMap, Versionize, VersionizeResult};
use versionize_derive::Versionize;
use vm_device::interrupt::Interrupt;
use vm_memory::GuestMemoryMmap;

use super::device::{ConfigSpace, Net};
use super::{NUM_QUEUES, QUEUE_SIZE};

use crate::virtio::persist::{Error as VirtioStateError, VirtioDeviceState};
use crate::virtio::{DeviceState, TYPE_NET};

#[derive(Clone, Versionize)]
// NOTICE: Any changes to this structure require a snapshot version bump.
pub struct NetConfigSpaceState {
    guest_mac: [u8; MAC_ADDR_LEN],
}

#[derive(Clone, Versionize)]
// NOTICE: Any changes to this structure require a snapshot version bump.
pub struct NetState {
    id: String,
    tap_if_name: String,
    rx_rate_limiter_state: RateLimiterState,
    tx_rate_limiter_state: RateLimiterState,
    mmds_ns: Option<MmdsNetworkStackState>,
    config_space: NetConfigSpaceState,
    virtio_state: VirtioDeviceState,
}

pub struct NetConstructorArgs {
    pub mem: GuestMemoryMmap,
}

#[derive(Debug)]
pub enum Error {
    CreateNet(super::Error),
    CreateRateLimiter(io::Error),
    VirtioState(VirtioStateError),
}

impl<I> Persist<'_> for Net<I>
where
    I: Interrupt + 'static,
{
    type State = NetState;
    type ConstructorArgs = NetConstructorArgs;
    type Error = Error;

    fn save(&self) -> Self::State {
        NetState {
            id: self.id().clone(),
            tap_if_name: self.iface_name(),
            rx_rate_limiter_state: self.rx_rate_limiter.save(),
            tx_rate_limiter_state: self.tx_rate_limiter.save(),
            mmds_ns: self.mmds_ns.as_ref().map(|mmds| mmds.save()),
            config_space: NetConfigSpaceState {
                guest_mac: self.config_space.guest_mac,
            },
            virtio_state: VirtioDeviceState::from_device(self),
        }
    }

    fn restore(
        constructor_args: Self::ConstructorArgs,
        state: &Self::State,
    ) -> std::result::Result<Self, Self::Error> {
        // RateLimiter::restore() can fail at creating a timerfd.
        let rx_rate_limiter = RateLimiter::restore((), &state.rx_rate_limiter_state)
            .map_err(Error::CreateRateLimiter)?;
        let tx_rate_limiter = RateLimiter::restore((), &state.tx_rate_limiter_state)
            .map_err(Error::CreateRateLimiter)?;
        let mut net = Net::new_with_tap(
            state.id.clone(),
            state.tap_if_name.clone(),
            None,
            rx_rate_limiter,
            tx_rate_limiter,
            state.mmds_ns.is_some(),
        )
        .map_err(Error::CreateNet)?;

        // Safe to unwrap because MmdsNetworkStack::restore() cannot fail.
        net.mmds_ns = state
            .mmds_ns
            .as_ref()
            .map(|mmds_state| MmdsNetworkStack::restore((), &mmds_state).unwrap());

        net.queues = state
            .virtio_state
            .build_queues_checked(&constructor_args.mem, TYPE_NET, NUM_QUEUES, QUEUE_SIZE)
            .map_err(Error::VirtioState)?;
        net.irq_trigger.irq_status =
            Arc::new(AtomicUsize::new(state.virtio_state.interrupt_status));
        net.avail_features = state.virtio_state.avail_features;
        net.acked_features = state.virtio_state.acked_features;
        net.config_space = ConfigSpace {
            guest_mac: state.config_space.guest_mac,
        };

        net.guest_mac = Some(MacAddr::from_bytes_unchecked(
            &state.config_space.guest_mac[..MAC_ADDR_LEN],
        ));

        if state.virtio_state.activated {
            net.device_state = DeviceState::Activated(constructor_args.mem);
        }

        Ok(net)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::device::VirtioDevice;

    use crate::virtio::net::test_utils::{default_guest_memory, default_net};
    use std::sync::atomic::Ordering;

    #[test]
    fn test_persistence() {
        let guest_mem = default_guest_memory();
        let mut mem = vec![0; 4096];
        let version_map = VersionMap::new();

        let id;
        let tap_if_name;
        let allow_mmds_requests;
        let virtio_state;

        // Create and save the net device.
        {
            let net = default_net();

            <Net as Persist>::save(&net)
                .serialize(&mut mem.as_mut_slice(), &version_map, 1)
                .unwrap();

            // Save some fields that we want to check later.
            id = net.id.clone();
            tap_if_name = net.iface_name();
            allow_mmds_requests = net.mmds_ns.is_some();
            virtio_state = VirtioDeviceState::from_device(&net);
        }

        // Deserialize and restore the net device.
        {
            let restored_net = Net::restore(
                NetConstructorArgs { mem: guest_mem },
                &NetState::deserialize(&mut mem.as_slice(), &version_map, 1).unwrap(),
            )
            .unwrap();

            // Test that virtio specific fields are the same.
            assert_eq!(restored_net.device_type(), TYPE_NET);
            assert_eq!(restored_net.avail_features(), virtio_state.avail_features);
            assert_eq!(restored_net.acked_features(), virtio_state.acked_features);
            assert_eq!(
                restored_net.interrupt_status().load(Ordering::Relaxed),
                virtio_state.interrupt_status
            );
            assert_eq!(restored_net.is_activated(), virtio_state.activated);

            // Test that net specific fields are the same.
            assert_eq!(&restored_net.id, &id);
            assert_eq!(&restored_net.iface_name(), &tap_if_name);
            assert_eq!(restored_net.mmds_ns.is_some(), allow_mmds_requests);
            assert_eq!(restored_net.rx_rate_limiter, RateLimiter::default());
            assert_eq!(restored_net.tx_rate_limiter, RateLimiter::default());
        }
    }
}
