// Copyright 2021 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::Deserialize;

#[derive(Debug, Deserialize, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
/// VFIO device configuration.
pub struct VfioDeviceConfig {
    /// PCI device path, example: /sys/bus/pci/devices/0000:18:00.0/
    pub path: String,
}
