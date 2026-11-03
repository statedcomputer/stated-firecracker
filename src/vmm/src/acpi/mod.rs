// Copyright 2024 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use acpi_tables::fadt::{FADT_F_HW_REDUCED_ACPI, FADT_F_PWR_BUTTON, FADT_F_SLP_BUTTON};
use acpi_tables::{Dsdt, Fadt, Madt, Mcfg, Rsdp, Sdt, Srat, Xsdt, aml};
use vm_allocator::AllocPolicy;

use crate::Vcpu;
use crate::acpi::x86_64::{
    apic_addr, rsdp_addr, setup_arch_dsdt, setup_arch_fadt, setup_interrupt_controllers,
    setup_srat_structures,
};
use crate::arch::x86_64::layout;
use crate::device_manager::DeviceManager;
use crate::devices::virtio::mem::{VIRTIO_MEM_DEV_ID, VIRTIO_MEM_NUMA_NODE_PXM, VirtioMem};
use crate::logger::{debug, error};
use crate::utils::u32_mib_to_bytes;
use crate::vstate::memory::{GuestAddress, GuestMemoryMmap};
use crate::vstate::resources::ResourceAllocator;

mod x86_64;

// Our (Original Equipment Manufacturer" (OEM) name. OEM is how ACPI names the manufacturer of the
// hardware that is exposed to the OS, through ACPI tables. The OEM name is passed in every ACPI
// table, to let the OS know that we are the owner of the table.
const OEM_ID: [u8; 6] = *b"FIRECK";

// In reality the OEM revision is per table and it defines the revision of the OEM's implementation
// of the particular ACPI table. For our purpose, we can set it to a fixed value for all the tables
const OEM_REVISION: u32 = 0;

// This is needed for an entry in the FADT table. Populating this entry in FADT is a way to let the
// guest know that it runs within a Firecracker microVM.
const HYPERVISOR_VENDOR_ID: [u8; 8] = *b"FIRECKVM";

#[derive(Debug, thiserror::Error, displaydoc::Display)]
/// Error type for ACPI related operations
pub enum AcpiError {
    /// Could not allocate resources: {0}
    VmAllocator(#[from] vm_allocator::Error),
    /// ACPI tables error: {0}
    AcpiTables(#[from] acpi_tables::AcpiError),
    /// Error creating AML bytecode: {0}
    AmlError(#[from] aml::AmlError),
}

/// Helper type that holds the guest memory in which we write the tables in and a resource
/// allocator for allocating space for the tables
struct AcpiTableWriter<'a> {
    mem: &'a GuestMemoryMmap,
}

impl AcpiTableWriter<'_> {
    /// Write a table in guest memory
    ///
    /// This will allocate enough space inside guest memory and write the table in the allocated
    /// buffer. It returns the address in which it wrote the table.
    fn write_acpi_table<S>(
        &mut self,
        resource_allocator: &mut ResourceAllocator,
        table: &mut S,
    ) -> Result<u64, AcpiError>
    where
        S: Sdt,
    {
        let addr = resource_allocator
            .system_memory
            .allocate(table.len().try_into().unwrap(), 1, AllocPolicy::FirstMatch)?
            .start();

        table
            .write_to_guest(self.mem, GuestAddress(addr))
            .inspect_err(|err| error!("acpi: Could not write table in guest memory: {err}"))?;

        debug!(
            "acpi: Wrote table ({} bytes) at address: {:#010x}",
            table.len(),
            addr
        );

        Ok(addr)
    }

    /// Build the DSDT table for the guest
    fn build_dsdt(
        &mut self,
        device_manager: &mut DeviceManager,
        resource_allocator: &mut ResourceAllocator,
    ) -> Result<u64, AcpiError> {
        let mut dsdt_data = Vec::new();

        // Device DSDT data.
        device_manager.append_aml_bytes(&mut dsdt_data)?;

        // Architecture specific DSDT data
        setup_arch_dsdt(&mut dsdt_data)?;

        let mut dsdt = Dsdt::new(OEM_ID, *b"FCVMDSDT", OEM_REVISION, dsdt_data);
        self.write_acpi_table(resource_allocator, &mut dsdt)
    }

    /// Build the FADT table for the guest
    ///
    /// This includes a pointer with the location of the DSDT in guest memory
    fn build_fadt(
        &mut self,
        resource_allocator: &mut ResourceAllocator,
        dsdt_addr: u64,
    ) -> Result<u64, AcpiError> {
        let mut fadt = Fadt::new(OEM_ID, *b"FCVMFADT", OEM_REVISION);
        fadt.set_hypervisor_vendor_id(HYPERVISOR_VENDOR_ID);
        fadt.set_x_dsdt(dsdt_addr);
        fadt.set_flags(
            (1 << FADT_F_HW_REDUCED_ACPI) | (1 << FADT_F_PWR_BUTTON) | (1 << FADT_F_SLP_BUTTON),
        );
        setup_arch_fadt(&mut fadt);
        self.write_acpi_table(resource_allocator, &mut fadt)
    }

    /// Build the MADT table for the guest
    ///
    /// This includes information about the interrupt controllers supported in the platform
    fn build_madt(
        &mut self,
        resource_allocator: &mut ResourceAllocator,
        nr_vcpus: u8,
    ) -> Result<u64, AcpiError> {
        let mut madt = Madt::new(
            OEM_ID,
            *b"FCVMMADT",
            OEM_REVISION,
            apic_addr(),
            setup_interrupt_controllers(nr_vcpus),
        );
        self.write_acpi_table(resource_allocator, &mut madt)
    }

    /// Build the SRAT table for the guest
    ///
    /// This puts the vCPUs and the boot memory in proximity domain 0 and the hotpluggable memory
    /// region in a proximity domain of its own
    fn build_srat(
        &mut self,
        resource_allocator: &mut ResourceAllocator,
        nr_vcpus: u8,
        hotplug_addr: GuestAddress,
        hotplug_size: u64,
    ) -> Result<u64, AcpiError> {
        let mut srat = Srat::new(
            OEM_ID,
            *b"FCVMSRAT",
            OEM_REVISION,
            setup_srat_structures(
                self.mem,
                nr_vcpus,
                hotplug_addr,
                hotplug_size,
                u32::from(VIRTIO_MEM_NUMA_NODE_PXM),
            ),
        );
        self.write_acpi_table(resource_allocator, &mut srat)
    }

    /// Build the XSDT table for the guest
    ///
    /// We pass to the guest the FADT, MADT and MCFG tables, and the SRAT when there is one.
    fn build_xsdt(
        &mut self,
        resource_allocator: &mut ResourceAllocator,
        tables: Vec<u64>,
    ) -> Result<u64, AcpiError> {
        let mut xsdt = Xsdt::new(OEM_ID, *b"FCMVXSDT", OEM_REVISION, tables);
        self.write_acpi_table(resource_allocator, &mut xsdt)
    }

    /// Build the MCFG table for the guest.
    fn build_mcfg(
        &mut self,
        resource_allocator: &mut ResourceAllocator,
        pci_mmio_config_addr: u64,
    ) -> Result<u64, AcpiError> {
        let mut mcfg = Mcfg::new(OEM_ID, *b"FCMVMCFG", OEM_REVISION, pci_mmio_config_addr);
        self.write_acpi_table(resource_allocator, &mut mcfg)
    }

    /// Build the RSDP pointer for the guest.
    ///
    /// This will build the RSDP pointer which points to the XSDT table and write it in guest
    /// memory. The address in which we write RSDP is pre-determined for every architecture.
    /// We will not allocate arbitrary memory for it
    fn build_rsdp(&mut self, xsdt_addr: u64) -> Result<(), AcpiError> {
        let mut rsdp = Rsdp::new(OEM_ID, xsdt_addr);
        rsdp.write_to_guest(self.mem, rsdp_addr())
            .inspect_err(|err| error!("acpi: Could not write RSDP in guest memory: {err}"))?;

        debug!(
            "acpi: Wrote RSDP ({} bytes) at address: {:#010x}",
            rsdp.len(),
            rsdp_addr().0
        );
        Ok(())
    }
}

/// Create ACPI tables for the guest
///
/// This will create the ACPI tables needed to describe to the guest OS the available hardware,
/// such as interrupt controllers, vCPUs and VirtIO devices.
pub(crate) fn create_acpi_tables(
    mem: &GuestMemoryMmap,
    device_manager: &mut DeviceManager,
    resource_allocator: &mut ResourceAllocator,
    vcpus: &[Vcpu],
) -> Result<(), AcpiError> {
    let mut writer = AcpiTableWriter { mem };
    let nr_vcpus: u8 = vcpus.len().try_into().unwrap();
    let dsdt_addr = writer.build_dsdt(device_manager, resource_allocator)?;

    let fadt_addr = writer.build_fadt(resource_allocator, dsdt_addr)?;
    let madt_addr = writer.build_madt(resource_allocator, nr_vcpus)?;
    let mcfg_addr = writer.build_mcfg(resource_allocator, layout::PCI_MMCONFIG_START)?;
    let mut tables = vec![fadt_addr, madt_addr, mcfg_addr];
    if let Some((hotplug_addr, hotplug_size)) = numa_node_hotplug_region(device_manager) {
        let srat_addr =
            writer.build_srat(resource_allocator, nr_vcpus, hotplug_addr, hotplug_size)?;
        tables.push(srat_addr);
    }
    let xsdt_addr = writer.build_xsdt(resource_allocator, tables)?;
    writer.build_rsdp(xsdt_addr)
}

/// The region of the virtio-mem device (its guest physical address and size), if there is a
/// device and its memory is a NUMA node of its own.
fn numa_node_hotplug_region(device_manager: &DeviceManager) -> Option<(GuestAddress, u64)> {
    device_manager
        .with_virtio_device(VIRTIO_MEM_DEV_ID, |mem: &mut VirtioMem| {
            mem.numa_node()
                .then(|| (mem.guest_address(), u32_mib_to_bytes(mem.total_size_mib())))
        })
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use acpi_tables::Sdt;
    use vm_memory::{Bytes, GuestAddress};

    use crate::acpi::x86_64::{rsdp_addr, setup_srat_structures};
    use crate::acpi::{AcpiError, AcpiTableWriter, create_acpi_tables};
    use crate::arch::x86_64::layout::{SYSTEM_MEM_SIZE, SYSTEM_MEM_START};
    use crate::builder::tests::{default_kernel_cmdline, default_vmm, insert_virtio_mem_device};
    use crate::device_manager::tests::default_device_manager;
    use crate::test_utils::multi_region_mem;
    use crate::utils::{mib_to_bytes, u64_to_usize};
    use crate::vmm_config::boot_source::BootSourceConfig;
    use crate::vmm_config::instance_info::InstanceInfo;
    use crate::vmm_config::machine_config::MachineConfig;
    use crate::vmm_config::memory_hotplug::MemoryHotplugConfig;
    use crate::vstate::memory::GuestMemoryMmap;
    use crate::vstate::resources::ResourceAllocator;
    use crate::vstate::vm::Vm;
    use crate::vstate::vm::tests::setup_vm_with_memory;
    use crate::{EventManager, Vmm};

    struct MockSdt(Vec<u8>);

    impl Sdt for MockSdt {
        fn len(&self) -> usize {
            self.0.len()
        }

        fn write_to_guest<M: vm_memory::GuestMemory>(
            &mut self,
            mem: &M,
            address: vm_memory::GuestAddress,
        ) -> acpi_tables::Result<()> {
            mem.write_slice(&self.0, address)?;
            Ok(())
        }
    }

    // Currently we are allocating up to SYSTEM_MEM_SIZE memory for ACPI tables. We are allocating
    // using the FirstMatch policy, with an 1 byte alignment. This test checks that we are able to
    // allocate up to this size, and get back the expected addresses.
    #[test]
    fn test_write_acpi_table_memory_allocation() {
        // A mocke Vmm object with 128MBs of memory
        let vmm = default_vmm();
        let mut writer = AcpiTableWriter {
            mem: vmm.vm.as_kvm().unwrap().guest_memory(),
        };
        let mut resource_allocator = vmm.vm.as_kvm().unwrap().resource_allocator();

        // This should succeed
        let mut sdt = MockSdt(vec![0; 4096]);
        let addr = writer
            .write_acpi_table(&mut resource_allocator, &mut sdt)
            .unwrap();
        assert_eq!(addr, SYSTEM_MEM_START);

        // Let's try to write two 4K pages plus one byte
        let mut sdt = MockSdt(vec![0; usize::try_from(SYSTEM_MEM_SIZE + 1).unwrap()]);
        let err = writer
            .write_acpi_table(&mut resource_allocator, &mut sdt)
            .unwrap_err();
        assert!(
            matches!(
                err,
                AcpiError::VmAllocator(vm_allocator::Error::ResourceNotAvailable)
            ),
            "{:?}",
            err
        );

        // We are allocating memory for tables with alignment of 1 byte. All of these should
        // succeed.
        let mut sdt = MockSdt(vec![0; 5]);
        let addr = writer
            .write_acpi_table(&mut resource_allocator, &mut sdt)
            .unwrap();
        assert_eq!(addr, SYSTEM_MEM_START + 4096);
        let mut sdt = MockSdt(vec![0; 2]);
        let addr = writer
            .write_acpi_table(&mut resource_allocator, &mut sdt)
            .unwrap();
        assert_eq!(addr, SYSTEM_MEM_START + 4101);
        let mut sdt = MockSdt(vec![0; 4]);
        let addr = writer
            .write_acpi_table(&mut resource_allocator, &mut sdt)
            .unwrap();
        assert_eq!(addr, SYSTEM_MEM_START + 4103);
        let mut sdt = MockSdt(vec![0; 8]);
        let addr = writer
            .write_acpi_table(&mut resource_allocator, &mut sdt)
            .unwrap();
        assert_eq!(addr, SYSTEM_MEM_START + 4107);
        let mut sdt = MockSdt(vec![0; 16]);
        let addr = writer
            .write_acpi_table(&mut resource_allocator, &mut sdt)
            .unwrap();
        assert_eq!(addr, SYSTEM_MEM_START + 4115);
    }

    // If, for whatever weird reason, we end up with microVM that has less memory than the maximum
    // address we allocate for ACPI tables, we would be able to allocate the tables but we would
    // not be able to write them. This is practically impossible in our case. If we get such a
    // guest memory, we won't be able to load the guest kernel, but the function does
    // return an error on this case, so let's just check that in case any of these assumptions
    // change in the future.
    #[test]
    fn test_write_acpi_table_small_memory() {
        let vm = setup_vm_with_memory(u64_to_usize(SYSTEM_MEM_START + SYSTEM_MEM_SIZE - 4096));
        let mut writer = AcpiTableWriter {
            mem: vm.guest_memory(),
        };
        let mut resource_allocator = ResourceAllocator::new();

        let mut sdt = MockSdt(vec![0; usize::try_from(SYSTEM_MEM_SIZE).unwrap()]);
        let err = writer
            .write_acpi_table(&mut resource_allocator, &mut sdt)
            .unwrap_err();
        assert!(
            matches!(
                err,
                AcpiError::AcpiTables(acpi_tables::AcpiError::GuestMemory(
                    vm_memory::GuestMemoryError::PartialBuffer {
                        expected: 263168,  // SYSTEM_MEM_SIZE
                        completed: 259072  // SYSTEM_MEM_SIZE - 4096
                    },
                ))
            ),
            "{:?}",
            err
        );
    }

    fn read_u32(mem: &GuestMemoryMmap, addr: u64) -> u32 {
        mem.read_obj(GuestAddress(addr)).unwrap()
    }

    fn read_u64(mem: &GuestMemoryMmap, addr: u64) -> u64 {
        mem.read_obj(GuestAddress(addr)).unwrap()
    }

    fn read_bytes(mem: &GuestMemoryMmap, addr: u64, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        mem.read_slice(&mut buf, GuestAddress(addr)).unwrap();
        buf
    }

    /// Builds the ACPI tables of a microVM with 2 vCPUs and 128 MiB of boot memory, with no
    /// virtio-mem device (None) or with one whose `numa_node` is given, and returns the guest
    /// memory and the address and signature of each table the XSDT lists, found through the RSDP.
    fn acpi_tables(numa_node: Option<bool>) -> (GuestMemoryMmap, Vec<(u64, [u8; 4])>) {
        let mut vm = setup_vm_with_memory(mib_to_bytes(128));
        let vcpus = vm.create_vcpus(2).unwrap();
        let vm = Arc::new(vm);
        let mut vmm = Vmm {
            checkpoint: Default::default(),
            instance_info: InstanceInfo::default(),
            machine_config: MachineConfig::default(),
            boot_source_config: BootSourceConfig::default(),
            shutdown_exit_code: None,
            vm: Vm::Kvm(vm.clone()),
            device_manager: default_device_manager(),
            #[cfg(target_arch = "x86_64")]
            stopped_clock: None,
            resume_refused: None,
            filler: Default::default(),
        };
        let mut event_manager = EventManager::new().unwrap();
        let mut cmdline = default_kernel_cmdline();
        if let Some(numa_node) = numa_node {
            let config = MemoryHotplugConfig {
                total_size_mib: 1024,
                block_size_mib: 2,
                slot_size_mib: 128,
                numa_node,
            };
            insert_virtio_mem_device(&mut vmm, &mut cmdline, &mut event_manager, config);
        }

        let mem = vm.guest_memory();
        create_acpi_tables(
            mem,
            &mut vmm.device_manager,
            &mut vm.resource_allocator(),
            &vcpus,
        )
        .unwrap();

        // XSDT address in the RSDP
        let xsdt_addr = read_u64(mem, rsdp_addr().0 + 24);
        assert_eq!(read_bytes(mem, xsdt_addr, 4), b"XSDT");
        let xsdt_len = u64::from(read_u32(mem, xsdt_addr + 4));
        let tables = (0..(xsdt_len - 36) / 8)
            .map(|i| {
                let addr = read_u64(mem, xsdt_addr + 36 + 8 * i);
                let sig: [u8; 4] = read_bytes(mem, addr, 4).try_into().unwrap();
                (addr, sig)
            })
            .collect();
        (mem.clone(), tables)
    }

    #[test]
    fn test_xsdt_srat_only_with_numa_node() {
        let signatures =
            |tables: &[(u64, [u8; 4])]| tables.iter().map(|(_, sig)| *sig).collect::<Vec<_>>();
        let v1_17 = vec![*b"FACP", *b"APIC", *b"MCFG"];

        // No hotplug, and hotplug without numa_node: the tables of v1.17.0
        let (_, no_hotplug) = acpi_tables(None);
        assert_eq!(signatures(&no_hotplug), v1_17);
        let (mem_off, hotplug) = acpi_tables(Some(false));
        assert_eq!(signatures(&hotplug), v1_17);

        // With numa_node: the same tables at the same addresses and with the same bytes, then
        // the SRAT
        let (mem, numa) = acpi_tables(Some(true));
        assert_eq!(
            signatures(&numa),
            vec![*b"FACP", *b"APIC", *b"MCFG", *b"SRAT"]
        );
        assert_eq!(numa[..3], hotplug[..]);
        for (addr, _) in &hotplug {
            let len = read_u32(&mem_off, addr + 4) as usize;
            assert_eq!(
                read_bytes(&mem, *addr, len),
                read_bytes(&mem_off, *addr, len)
            );
        }

        // The SRAT: 2 vCPUs and the one boot memory range in domain 0, the virtio-mem region
        // (as insert_virtio_mem_device places it) in domain 1, hot-pluggable
        let srat_addr = numa[3].0;
        let len = read_u32(&mem, srat_addr + 4) as usize;
        assert_eq!(len, 48 + 2 * 16 + 2 * 40);
        let srat = read_bytes(&mem, srat_addr, len);
        assert_eq!(srat[8], 3);
        assert_eq!(srat.iter().fold(0u8, |acc, x| acc.wrapping_add(*x)), 0);
        for cpu in 0..2 {
            let s = &srat[48 + 16 * cpu..48 + 16 * (cpu + 1)];
            assert_eq!(&s[0..4], &[0, 16, 0, u8::try_from(cpu).unwrap()]);
            assert_eq!(s[4] & 1, 1);
        }
        let boot = &srat[80..120];
        assert_eq!(&boot[0..6], &[1, 40, 0, 0, 0, 0]);
        assert_eq!(u64::from_le_bytes(boot[8..16].try_into().unwrap()), 0);
        assert_eq!(
            u64::from_le_bytes(boot[16..24].try_into().unwrap()),
            mib_to_bytes(128) as u64
        );
        assert_eq!(boot[28], 0b01);
        let hotplug = &srat[120..160];
        assert_eq!(&hotplug[0..6], &[1, 40, 1, 0, 0, 0]);
        assert_eq!(
            u64::from_le_bytes(hotplug[8..16].try_into().unwrap()),
            512 << 30
        );
        assert_eq!(
            u64::from_le_bytes(hotplug[16..24].try_into().unwrap()),
            mib_to_bytes(1024) as u64
        );
        assert_eq!(hotplug[28], 0b11);
    }

    #[test]
    fn test_srat_structures_two_boot_ranges() {
        // Boot memory below and above the 32-bit gap
        let mem = multi_region_mem(&[
            (GuestAddress(0), mib_to_bytes(2)),
            (GuestAddress(4 << 30), mib_to_bytes(2)),
        ]);
        let s = setup_srat_structures(&mem, 1, GuestAddress(512 << 30), 1 << 30, 1);
        assert_eq!(s.len(), 16 + 3 * 40);
        let ranges: Vec<(u64, u64, u32, u32)> = s[16..]
            .chunks(40)
            .map(|m| {
                assert_eq!(&m[0..2], &[1, 40]);
                (
                    u64::from_le_bytes(m[8..16].try_into().unwrap()),
                    u64::from_le_bytes(m[16..24].try_into().unwrap()),
                    u32::from_le_bytes(m[2..6].try_into().unwrap()),
                    u32::from_le_bytes(m[28..32].try_into().unwrap()),
                )
            })
            .collect();
        assert_eq!(
            ranges,
            vec![
                (0, mib_to_bytes(2) as u64, 0, 1),
                (4 << 30, mib_to_bytes(2) as u64, 0, 1),
                (512 << 30, 1 << 30, 1, 3),
            ]
        );
    }
}
