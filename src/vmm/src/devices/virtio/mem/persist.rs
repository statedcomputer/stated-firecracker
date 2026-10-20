// Copyright 2022 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Defines the structures needed for saving/restoring virtio-mem devices.

use std::sync::Arc;

use bitvec::vec::BitVec;
use serde::{Deserialize, Serialize};

use crate::devices::virtio::device::VirtioDeviceType;
use crate::devices::virtio::generated::virtio_mem::{VIRTIO_MEM_F_ACPI_PXM, virtio_mem_config};
use crate::devices::virtio::mem::{
    MEM_NUM_QUEUES, VIRTIO_MEM_NUMA_NODE_PXM, VirtioMem, VirtioMemError,
};
use crate::devices::virtio::persist::{PersistError as VirtioStateError, VirtioDeviceState};
use crate::devices::virtio::queue::FIRECRACKER_MAX_QUEUE_SIZE;
use crate::snapshot::Persist;
use crate::utils::usize_to_u64;
use crate::vstate::vm::KvmVm;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtioMemState {
    pub virtio_state: VirtioDeviceState,
    addr: u64,
    region_size: u64,
    block_size: u64,
    usable_region_size: u64,
    requested_size: u64,
    slot_size: usize,
    plugged_blocks: Vec<bool>,
}

#[derive(Debug)]
pub struct VirtioMemConstructorArgs {
    vm: Arc<KvmVm>,
}

impl VirtioMemConstructorArgs {
    pub fn new(vm: Arc<KvmVm>) -> Self {
        Self { vm }
    }
}

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum VirtioMemPersistError {
    /// Create virtio-mem: {0}
    CreateVirtioMem(#[from] VirtioMemError),
    /// Virtio state: {0}
    VirtioState(#[from] VirtioStateError),
}

impl Persist<'_> for VirtioMem {
    type State = VirtioMemState;
    type ConstructorArgs = VirtioMemConstructorArgs;
    type Error = VirtioMemPersistError;

    fn save(&self) -> Self::State {
        VirtioMemState {
            virtio_state: VirtioDeviceState::from_device(self, &self.queues),
            addr: self.config.addr,
            region_size: self.config.region_size,
            block_size: self.config.block_size,
            usable_region_size: self.config.usable_region_size,
            plugged_blocks: self.plugged_blocks.iter().by_vals().collect(),
            requested_size: self.config.requested_size,
            slot_size: self.slot_size,
        }
    }

    fn restore(
        constructor_args: Self::ConstructorArgs,
        state: &Self::State,
    ) -> Result<Self, Self::Error> {
        let queues = state.virtio_state.build_queues_checked(
            constructor_args.vm.guest_memory(),
            VirtioDeviceType::Mem,
            MEM_NUM_QUEUES,
            FIRECRACKER_MAX_QUEUE_SIZE,
        )?;

        let plugged_blocks = BitVec::from_iter(state.plugged_blocks.iter());

        // The node is not part of the state: a device that offers ACPI_PXM was made with
        // `numa_node`, whose node_id is always VIRTIO_MEM_NUMA_NODE_PXM.
        let node_id = if state.virtio_state.avail_features & (1 << VIRTIO_MEM_F_ACPI_PXM) != 0 {
            VIRTIO_MEM_NUMA_NODE_PXM
        } else {
            0
        };

        let config = virtio_mem_config {
            addr: state.addr,
            region_size: state.region_size,
            block_size: state.block_size,
            usable_region_size: state.usable_region_size,
            plugged_size: usize_to_u64(plugged_blocks.count_ones()) * state.block_size,
            requested_size: state.requested_size,
            node_id,
            ..Default::default()
        };

        let mut virtio_mem = VirtioMem::from_state(
            constructor_args.vm,
            queues,
            config,
            state.slot_size,
            plugged_blocks,
        )?;
        virtio_mem.set_avail_features(state.virtio_state.avail_features);
        virtio_mem.set_acked_features(state.virtio_state.acked_features);

        Ok(virtio_mem)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::virtio::device::VirtioDevice;
    use crate::devices::virtio::mem::device::test_utils::{
        default_virtio_mem, virtio_mem_with_numa_node,
    };
    use crate::vstate::vm::tests::setup_vm_with_memory;

    #[test]
    fn test_save_state() {
        let dev = default_virtio_mem();
        let state = dev.save();

        assert_eq!(state.addr, dev.config.addr);
        assert_eq!(state.region_size, dev.config.region_size);
        assert_eq!(state.block_size, dev.config.block_size);
        assert_eq!(state.usable_region_size, dev.config.usable_region_size);
        assert_eq!(
            state.plugged_blocks.iter().collect::<BitVec>(),
            dev.plugged_blocks
        );
        assert_eq!(state.requested_size, dev.config.requested_size);
        assert_eq!(state.slot_size, dev.slot_size);
    }

    #[test]
    fn test_save_restore_state() {
        let mut original_dev = default_virtio_mem();
        original_dev.set_acked_features(original_dev.avail_features());
        let state = original_dev.save();

        // Create a "new" VM for restore
        let vm = setup_vm_with_memory(0x1000);
        let vm = Arc::new(vm);
        let constructor_args = VirtioMemConstructorArgs::new(vm);
        let restored_dev = VirtioMem::restore(constructor_args, &state).unwrap();

        assert_eq!(original_dev.config, restored_dev.config);
        assert_eq!(original_dev.slot_size, restored_dev.slot_size);
        assert_eq!(original_dev.avail_features(), restored_dev.avail_features());
        assert_eq!(original_dev.acked_features(), restored_dev.acked_features());
    }

    #[test]
    fn test_save_restore_numa_node() {
        for numa_node in [false, true] {
            let original_dev = virtio_mem_with_numa_node(numa_node);
            let state = original_dev.save();

            let vm = Arc::new(setup_vm_with_memory(0x1000));
            let restored_dev =
                VirtioMem::restore(VirtioMemConstructorArgs::new(vm), &state).unwrap();

            assert_eq!(restored_dev.numa_node(), numa_node);
            assert_eq!(original_dev.config, restored_dev.config);
            assert_eq!(original_dev.avail_features(), restored_dev.avail_features());
        }
    }
}
