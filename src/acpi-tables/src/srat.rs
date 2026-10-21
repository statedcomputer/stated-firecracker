// Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
//
// SPDX-License-Identifier: Apache-2.0

use std::mem::size_of;

use vm_memory::{Address, Bytes, GuestAddress, GuestMemory};
use zerocopy::little_endian::{U32, U64};
use zerocopy::{Immutable, IntoBytes};

use crate::{AcpiError, Result, Sdt, SdtHeader, checksum};

/// The structure is enabled (both structure types).
pub const SRAT_AFFINITY_ENABLED: u32 = 1 << 0;
/// The memory range is hot-pluggable (Memory Affinity structure only).
pub const SRAT_MEMORY_HOT_PLUGGABLE: u32 = 1 << 1;

// clippy doesn't understand that we actually "use" the fields of this struct when we serialize
// them as bytes in guest memory, so here we just ignore dead code to avoid having to name
// everything with an underscore prefix
#[allow(dead_code)]
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, Default, IntoBytes, Immutable)]
pub struct ProcessorLocalApicAffinity {
    r#type: u8,
    length: u8,
    proximity_domain_lo: u8,
    apic_id: u8,
    flags: U32,
    local_sapic_eid: u8,
    proximity_domain_hi: [u8; 3],
    clock_domain: U32,
}

impl ProcessorLocalApicAffinity {
    /// Processor Local APIC/SAPIC Affinity structure (type 0) of an enabled processor
    pub fn new(apic_id: u8, proximity_domain: u32) -> Self {
        let [lo, hi @ ..] = proximity_domain.to_le_bytes();
        Self {
            r#type: 0,
            length: 16,
            proximity_domain_lo: lo,
            apic_id,
            flags: U32::new(SRAT_AFFINITY_ENABLED),
            local_sapic_eid: 0,
            proximity_domain_hi: hi,
            clock_domain: U32::ZERO,
        }
    }
}

// clippy doesn't understand that we actually "use" the fields of this struct when we serialize
// them as bytes in guest memory, so here we just ignore dead code to avoid having to name
// everything with an underscore prefix
#[allow(dead_code)]
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, Default, IntoBytes, Immutable)]
pub struct MemoryAffinity {
    r#type: u8,
    length: u8,
    proximity_domain: U32,
    reserved1: [u8; 2],
    base_address: U64,
    range_length: U64,
    reserved2: U32,
    flags: U32,
    reserved3: U64,
}

impl MemoryAffinity {
    /// Memory Affinity structure (type 1) of the range [base_address, base_address + length)
    pub fn new(base_address: u64, length: u64, proximity_domain: u32, flags: u32) -> Self {
        Self {
            r#type: 1,
            length: 40,
            proximity_domain: U32::new(proximity_domain),
            reserved1: [0; 2],
            base_address: U64::new(base_address),
            range_length: U64::new(length),
            reserved2: U32::ZERO,
            flags: U32::new(flags),
            reserved3: U64::ZERO,
        }
    }
}

// clippy doesn't understand that we actually "use" the fields of this struct when we serialize
// them as bytes in guest memory, so here we just ignore dead code to avoid having to name
// everything with an underscore prefix
#[allow(dead_code)]
#[repr(C, packed)]
#[derive(Debug, IntoBytes, Immutable)]
struct SratHeader {
    sdt: SdtHeader,
    // Must be 1 for backward compatibility
    reserved1: U32,
    reserved2: U64,
}

/// System Resource Affinity Table (SRAT)
///
/// This table associates processors and memory ranges with proximity domains (NUMA nodes).
/// More information about this table can be found in the ACPI specification:
/// https://uefi.org/specs/ACPI/6.5/05_ACPI_Software_Programming_Model.html#system-resource-affinity-table-srat
#[derive(Debug)]
pub struct Srat {
    header: SratHeader,
    structures: Vec<u8>,
}

impl Srat {
    /// Creates the table from `structures`, the encoded affinity structures. They describe a few
    /// vCPUs and memory ranges, so the table's length (a few KiB at most) fits its u32 field.
    pub fn new(
        oem_id: [u8; 6],
        oem_table_id: [u8; 8],
        oem_revision: u32,
        structures: Vec<u8>,
    ) -> Self {
        let length = size_of::<SratHeader>() + structures.len();
        let sdt_header = SdtHeader::new(
            *b"SRAT",
            length.try_into().unwrap(),
            3,
            oem_id,
            oem_table_id,
            oem_revision,
        );

        let mut header = SratHeader {
            sdt: sdt_header,
            reserved1: U32::new(1),
            reserved2: U64::ZERO,
        };

        header.sdt.checksum = checksum(&[header.as_bytes(), structures.as_bytes()]);

        Srat { header, structures }
    }
}

impl Sdt for Srat {
    fn len(&self) -> usize {
        self.header.sdt.length.get().try_into().unwrap()
    }

    fn write_to_guest<M: GuestMemory>(&mut self, mem: &M, address: GuestAddress) -> Result<()> {
        mem.write_slice(self.header.as_bytes(), address)?;
        let address = address
            .checked_add(size_of::<SratHeader>() as u64)
            .ok_or(AcpiError::InvalidGuestAddress)?;
        mem.write_slice(self.structures.as_bytes(), address)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
    use zerocopy::IntoBytes;

    use super::*;

    fn u32_at(b: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
    }

    fn u64_at(b: &[u8], off: usize) -> u64 {
        u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
    }

    #[test]
    fn test_structure_sizes() {
        assert_eq!(size_of::<SratHeader>(), 48);
        assert_eq!(size_of::<ProcessorLocalApicAffinity>(), 16);
        assert_eq!(size_of::<MemoryAffinity>(), 40);
    }

    #[test]
    fn test_processor_affinity_domain_bytes() {
        let b = ProcessorLocalApicAffinity::new(7, 0x0403_0201);
        let b = b.as_bytes();
        assert_eq!(b[0], 0);
        assert_eq!(b[1], 16);
        assert_eq!(b[2], 0x01);
        assert_eq!(b[3], 7);
        assert_eq!(u32_at(b, 4), SRAT_AFFINITY_ENABLED);
        assert_eq!(b[8], 0);
        assert_eq!(&b[9..12], &[0x02, 0x03, 0x04]);
        assert_eq!(u32_at(b, 12), 0);
    }

    // 2 vCPUs in domain 0, one boot memory range in domain 0, one hot-pluggable range in
    // domain 1
    #[test]
    fn test_srat_small() {
        let mut structures = Vec::new();
        for cpu in 0..2u8 {
            structures.extend_from_slice(ProcessorLocalApicAffinity::new(cpu, 0).as_bytes());
        }
        structures.extend_from_slice(
            MemoryAffinity::new(0, 0x4000_0000, 0, SRAT_AFFINITY_ENABLED).as_bytes(),
        );
        structures.extend_from_slice(
            MemoryAffinity::new(
                0x80_0000_0000,
                0x4000_0000,
                1,
                SRAT_AFFINITY_ENABLED | SRAT_MEMORY_HOT_PLUGGABLE,
            )
            .as_bytes(),
        );
        let mut srat = Srat::new(*b"FIRECK", *b"FCVMSRAT", 0, structures);
        assert_eq!(srat.len(), 48 + 2 * 16 + 2 * 40);

        let mem: GuestMemoryMmap =
            GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1000)]).unwrap();
        srat.write_to_guest(&mem, GuestAddress(0x100)).unwrap();
        let mut b = vec![0u8; srat.len()];
        mem.read_slice(&mut b, GuestAddress(0x100)).unwrap();

        // Header
        assert_eq!(&b[0..4], b"SRAT");
        assert_eq!(u32_at(&b, 4) as usize, b.len());
        assert_eq!(b[8], 3);
        assert_eq!(b.iter().fold(0u8, |acc, x| acc.wrapping_add(*x)), 0);
        assert_eq!(&b[10..16], b"FIRECK");
        assert_eq!(&b[16..24], b"FCVMSRAT");
        assert_eq!(u32_at(&b, 36), 1);
        assert_eq!(u64_at(&b, 40), 0);

        // Processor Local APIC/SAPIC Affinity structures
        for cpu in 0..2usize {
            let s = &b[48 + cpu * 16..48 + (cpu + 1) * 16];
            assert_eq!(s[0], 0);
            assert_eq!(s[1], 16);
            assert_eq!(s[2], 0);
            assert_eq!(s[3] as usize, cpu);
            assert_eq!(u32_at(s, 4), SRAT_AFFINITY_ENABLED);
            assert_eq!(&s[9..12], &[0, 0, 0]);
        }

        // Memory Affinity structures
        let boot = &b[80..120];
        assert_eq!(boot[0], 1);
        assert_eq!(boot[1], 40);
        assert_eq!(u32_at(boot, 2), 0);
        assert_eq!(u64_at(boot, 8), 0);
        assert_eq!(u64_at(boot, 16), 0x4000_0000);
        assert_eq!(u32_at(boot, 28), SRAT_AFFINITY_ENABLED);

        let hotplug = &b[120..160];
        assert_eq!(hotplug[0], 1);
        assert_eq!(hotplug[1], 40);
        assert_eq!(u32_at(hotplug, 2), 1);
        assert_eq!(u64_at(hotplug, 8), 0x80_0000_0000);
        assert_eq!(u64_at(hotplug, 16), 0x4000_0000);
        assert_eq!(u32_at(hotplug, 28), 0b11);
        assert_eq!(u64_at(hotplug, 32), 0);
    }
}
