// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

/// Follows a C3 template in setting up the CPUID.
pub mod c3;
/// Follows a T2 template in setting up the CPUID.
pub mod t2;
/// Follows a T2 template for setting up the CPUID with additional MSRs
/// that are speciffic to an Intel Skylake CPU.
pub mod t2s;

use crate::common::{get_vendor_id_from_host, VENDOR_ID_INTEL};
use crate::transformer::Error;

pub fn validate_vendor_id() -> Result<(), Error> {
    let vendor_id = get_vendor_id_from_host()?;
    if &vendor_id != VENDOR_ID_INTEL {
        return Err(Error::InvalidVendor);
    }

    Ok(())
}
