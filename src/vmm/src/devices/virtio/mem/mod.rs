// Copyright 2022 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod device;
mod event_handler;
pub mod metrics;
pub mod persist;
mod request;

pub use self::device::{VirtioMem, VirtioMemError, VirtioMemStatus};

pub(crate) const MEM_NUM_QUEUES: usize = 1;

pub(crate) const MEM_QUEUE: usize = 0;

pub const VIRTIO_MEM_DEFAULT_BLOCK_SIZE_MIB: u32 = 2;
pub const VIRTIO_MEM_DEFAULT_SLOT_SIZE_MIB: u32 = 128;
/// The ACPI proximity domain of the hotpluggable memory when it is a NUMA node of its own: the
/// device's `node_id` and the domain of its range in the SRAT.
pub const VIRTIO_MEM_NUMA_NODE_PXM: u16 = 1;

pub const VIRTIO_MEM_DEV_ID: &str = "mem";
