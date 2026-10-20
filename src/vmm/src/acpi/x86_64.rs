// Copyright 2024 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::mem::size_of;

use acpi_tables::fadt::IAPC_BOOT_ARG_FLAGS_VGA_NOT_PRESENT;
use acpi_tables::madt::{IoAPIC, LocalAPIC};
use acpi_tables::srat::{
    MemoryAffinity, ProcessorLocalApicAffinity, SRAT_AFFINITY_ENABLED, SRAT_MEMORY_HOT_PLUGGABLE,
};
use acpi_tables::{Fadt, aml};
use vm_memory::{Address, GuestAddress, GuestMemoryBackend, GuestMemoryRegion};
use zerocopy::IntoBytes;

use crate::arch::x86_64::layout;
use crate::device_manager::legacy::PortIODeviceManager;
use crate::vstate::memory::{GuestMemoryMmap, GuestRegionType};

/// The proximity domain of the vCPUs and of the boot memory in the SRAT
const SRAT_BOOT_PROXIMITY_DOMAIN: u32 = 0;

#[inline(always)]
pub(crate) fn setup_interrupt_controllers(nr_vcpus: u8) -> Vec<u8> {
    let mut ic =
        Vec::with_capacity(size_of::<IoAPIC>() + (nr_vcpus as usize) * size_of::<LocalAPIC>());

    ic.extend_from_slice(IoAPIC::new(0, layout::IOAPIC_ADDR).as_bytes());
    for i in 0..nr_vcpus {
        ic.extend_from_slice(LocalAPIC::new(i).as_bytes());
    }
    ic
}

/// The affinity structures of the SRAT: each vCPU (APIC id = vCPU index, as in the MADT) and each
/// range of boot memory in proximity domain 0, and the hotpluggable memory region in the given
/// proximity domain, hot-pluggable.
pub(crate) fn setup_srat_structures(
    mem: &GuestMemoryMmap,
    nr_vcpus: u8,
    hotplug_addr: GuestAddress,
    hotplug_size: u64,
    hotplug_proximity_domain: u32,
) -> Vec<u8> {
    let mut structures = Vec::new();
    for i in 0..nr_vcpus {
        structures.extend_from_slice(
            ProcessorLocalApicAffinity::new(i, SRAT_BOOT_PROXIMITY_DOMAIN).as_bytes(),
        );
    }
    for region in mem
        .iter()
        .filter(|region| region.region_type == GuestRegionType::Dram)
    {
        structures.extend_from_slice(
            MemoryAffinity::new(
                region.start_addr().raw_value(),
                region.len(),
                SRAT_BOOT_PROXIMITY_DOMAIN,
                SRAT_AFFINITY_ENABLED,
            )
            .as_bytes(),
        );
    }
    structures.extend_from_slice(
        MemoryAffinity::new(
            hotplug_addr.raw_value(),
            hotplug_size,
            hotplug_proximity_domain,
            SRAT_AFFINITY_ENABLED | SRAT_MEMORY_HOT_PLUGGABLE,
        )
        .as_bytes(),
    );
    structures
}

#[inline(always)]
pub(crate) fn setup_arch_fadt(fadt: &mut Fadt) {
    // Let the guest kernel know that there is not VGA hardware present
    // neither do we support ASPM, or MSI type of interrupts.
    // More info here:
    // https://uefi.org/specs/ACPI/6.5/05_ACPI_Software_Programming_Model.html?highlight=0a06#ia-pc-boot-architecture-flags
    fadt.setup_iapc_flags(1 << IAPC_BOOT_ARG_FLAGS_VGA_NOT_PRESENT);
}

#[inline(always)]
pub(crate) fn setup_arch_dsdt(dsdt_data: &mut Vec<u8>) -> Result<(), aml::AmlError> {
    PortIODeviceManager::append_aml_bytes(dsdt_data)
}

pub(crate) const fn apic_addr() -> u32 {
    layout::APIC_ADDR
}

pub(crate) const fn rsdp_addr() -> GuestAddress {
    GuestAddress(layout::RSDP_ADDR)
}
