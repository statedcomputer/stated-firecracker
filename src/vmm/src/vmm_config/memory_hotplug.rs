// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use crate::devices::virtio::mem::{
    VIRTIO_MEM_DEFAULT_BLOCK_SIZE_MIB, VIRTIO_MEM_DEFAULT_SLOT_SIZE_MIB, VirtioMem,
};

/// Errors associated with memory hotplug configuration.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum MemoryHotplugConfigError {
    /// Block size must not be lower than {0} MiB
    BlockSizeTooSmall(u32),
    /// Block size must be a power of 2
    BlockSizeNotPowerOfTwo,
    /// Slot size must not be lower than {0} MiB
    SlotSizeTooSmall(u32),
    /// Slot size must be a multiple of block size ({0} MiB)
    SlotSizeNotMultipleOfBlockSize(u32),
    /// Total size must not be lower than slot size ({0} MiB)
    TotalSizeTooSmall(u32),
    /// Total size must be a multiple of slot size ({0} MiB)
    TotalSizeNotMultipleOfSlotSize(u32),
    /// A NUMA node of its own for the hotpluggable memory is only supported on x86_64
    NumaNodeNotSupported,
}

fn default_block_size_mib() -> u32 {
    VIRTIO_MEM_DEFAULT_BLOCK_SIZE_MIB
}

fn default_slot_size_mib() -> u32 {
    VIRTIO_MEM_DEFAULT_SLOT_SIZE_MIB
}

/// Configuration for memory hotplug device.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryHotplugConfig {
    /// Total memory size in MiB that can be hotplugged.
    pub total_size_mib: u32,
    /// Block size in MiB. A block is the smallest unit the guest can hot(un)plug
    #[serde(default = "default_block_size_mib")]
    pub block_size_mib: u32,
    /// Slot size in MiB. A slot is the smallest unit the host can (de)attach memory
    #[serde(default = "default_slot_size_mib")]
    pub slot_size_mib: u32,
    /// Whether the hotpluggable memory is a NUMA node of its own in the guest (node 1, x86_64
    /// only). When true, an ACPI SRAT puts the boot memory and the vCPUs in proximity domain 0
    /// and the hotpluggable region in proximity domain 1, and the device offers
    /// `VIRTIO_MEM_F_ACPI_PXM` with `node_id` 1. Left out of the serialized configuration when
    /// false, which is then the configuration of a device without this option.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub numa_node: bool,
}

impl MemoryHotplugConfig {
    /// Validates the configuration.
    pub fn validate(&self) -> Result<(), MemoryHotplugConfigError> {
        let min_block_size_mib = VIRTIO_MEM_DEFAULT_BLOCK_SIZE_MIB;
        if self.block_size_mib < min_block_size_mib {
            return Err(MemoryHotplugConfigError::BlockSizeTooSmall(
                min_block_size_mib,
            ));
        }
        if !self.block_size_mib.is_power_of_two() {
            return Err(MemoryHotplugConfigError::BlockSizeNotPowerOfTwo);
        }

        let min_slot_size_mib = VIRTIO_MEM_DEFAULT_SLOT_SIZE_MIB;
        if self.slot_size_mib < min_slot_size_mib {
            return Err(MemoryHotplugConfigError::SlotSizeTooSmall(
                min_slot_size_mib,
            ));
        }
        if !self.slot_size_mib.is_multiple_of(self.block_size_mib) {
            return Err(MemoryHotplugConfigError::SlotSizeNotMultipleOfBlockSize(
                self.block_size_mib,
            ));
        }

        if self.total_size_mib < self.slot_size_mib {
            return Err(MemoryHotplugConfigError::TotalSizeTooSmall(
                self.slot_size_mib,
            ));
        }
        if !self.total_size_mib.is_multiple_of(self.slot_size_mib) {
            return Err(MemoryHotplugConfigError::TotalSizeNotMultipleOfSlotSize(
                self.slot_size_mib,
            ));
        }

        // Only x86_64 has the ACPI SRAT that describes the node to the guest
        if cfg!(not(target_arch = "x86_64")) && self.numa_node {
            return Err(MemoryHotplugConfigError::NumaNodeNotSupported);
        }

        Ok(())
    }
}

impl From<&VirtioMem> for MemoryHotplugConfig {
    fn from(mem: &VirtioMem) -> Self {
        MemoryHotplugConfig {
            total_size_mib: mem.total_size_mib(),
            block_size_mib: mem.block_size_mib(),
            slot_size_mib: mem.slot_size_mib(),
            numa_node: mem.numa_node(),
        }
    }
}

/// Configuration for memory hotplug device.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryHotplugSizeUpdate {
    /// Requested size in MiB to resize the hotpluggable memory to.
    pub requested_size_mib: u32,
}

#[cfg(test)]
mod tests {
    use serde_json;

    use super::*;

    #[test]
    fn test_valid_config() {
        let config = MemoryHotplugConfig {
            total_size_mib: 1024,
            block_size_mib: 2,
            slot_size_mib: 128,
            numa_node: false,
        };
        config.validate().unwrap();
    }

    #[test]
    fn test_block_size_too_small() {
        let config = MemoryHotplugConfig {
            total_size_mib: 1024,
            block_size_mib: 1,
            slot_size_mib: 128,
            numa_node: false,
        };
        match config.validate() {
            Err(MemoryHotplugConfigError::BlockSizeTooSmall(min)) => assert_eq!(min, 2),
            _ => panic!("Expected InvalidBlockSizeTooSmall error"),
        }
    }

    #[test]
    fn test_block_size_not_power_of_two() {
        let config = MemoryHotplugConfig {
            total_size_mib: 1024,
            block_size_mib: 3,
            slot_size_mib: 128,
            numa_node: false,
        };
        match config.validate() {
            Err(MemoryHotplugConfigError::BlockSizeNotPowerOfTwo) => {}
            _ => panic!("Expected InvalidBlockSizePowerOfTwo error"),
        }
    }

    #[test]
    fn test_slot_size_too_small() {
        let config = MemoryHotplugConfig {
            total_size_mib: 1024,
            block_size_mib: 2,
            slot_size_mib: 1,
            numa_node: false,
        };
        match config.validate() {
            Err(MemoryHotplugConfigError::SlotSizeTooSmall(min)) => assert_eq!(min, 128),
            _ => panic!("Expected InvalidSlotSizeTooSmall error"),
        }
    }

    #[test]
    fn test_slot_size_not_multiple_of_block_size() {
        let config = MemoryHotplugConfig {
            total_size_mib: 1024,
            block_size_mib: 4,
            slot_size_mib: 130,
            numa_node: false,
        };
        match config.validate() {
            Err(MemoryHotplugConfigError::SlotSizeNotMultipleOfBlockSize(block_size)) => {
                assert_eq!(block_size, 4)
            }
            _ => panic!("Expected InvalidSlotSizeMultiple error"),
        }
    }

    #[test]
    fn test_total_size_too_small() {
        let config = MemoryHotplugConfig {
            total_size_mib: 64,
            block_size_mib: 2,
            slot_size_mib: 128,
            numa_node: false,
        };
        match config.validate() {
            Err(MemoryHotplugConfigError::TotalSizeTooSmall(slot_size)) => {
                assert_eq!(slot_size, 128)
            }
            _ => panic!("Expected InvalidTotalSizeTooSmall error"),
        }
    }

    #[test]
    fn test_total_size_not_multiple_of_slot_size() {
        let config = MemoryHotplugConfig {
            total_size_mib: 1000,
            block_size_mib: 2,
            slot_size_mib: 128,
            numa_node: false,
        };
        match config.validate() {
            Err(MemoryHotplugConfigError::TotalSizeNotMultipleOfSlotSize(slot_size)) => {
                assert_eq!(slot_size, 128)
            }
            _ => panic!("Expected InvalidTotalSizeMultiple error"),
        }
    }

    #[test]
    fn test_defaults() {
        assert_eq!(default_block_size_mib(), 2);
        assert_eq!(default_slot_size_mib(), 128);

        let json = r#"{
            "total_size_mib": 1024
        }"#;
        let deserialized: MemoryHotplugConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            deserialized,
            MemoryHotplugConfig {
                total_size_mib: 1024,
                block_size_mib: 2,
                slot_size_mib: 128,
                numa_node: false,
            }
        );
    }

    #[test]
    fn test_serde() {
        let config = MemoryHotplugConfig {
            total_size_mib: 1024,
            block_size_mib: 4,
            slot_size_mib: 256,
            numa_node: false,
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: MemoryHotplugConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_numa_node() {
        // Absent: false
        let json = r#"{
            "total_size_mib": 1024
        }"#;
        let config: MemoryHotplugConfig = serde_json::from_str(json).unwrap();
        assert!(!config.numa_node);

        // Given
        let json = r#"{
            "total_size_mib": 1024,
            "numa_node": true
        }"#;
        let config: MemoryHotplugConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            config,
            MemoryHotplugConfig {
                total_size_mib: 1024,
                block_size_mib: 2,
                slot_size_mib: 128,
                numa_node: true,
            }
        );

        // Not a bool
        let json = r#"{
            "total_size_mib": 1024,
            "numa_node": 1
        }"#;
        serde_json::from_str::<MemoryHotplugConfig>(json).unwrap_err();

        // Serialized with the field, and read back
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains(r#""numa_node":true"#), "{json}");
        assert_eq!(
            serde_json::from_str::<MemoryHotplugConfig>(&json).unwrap(),
            config
        );

        #[cfg(target_arch = "x86_64")]
        config.validate().unwrap();
        #[cfg(target_arch = "aarch64")]
        assert!(matches!(
            config.validate(),
            Err(MemoryHotplugConfigError::NumaNodeNotSupported)
        ));

        // Off: left out of the serialized configuration, as before the field existed
        let config = MemoryHotplugConfig {
            numa_node: false,
            ..config
        };
        config.validate().unwrap();
        assert_eq!(
            serde_json::to_string(&config).unwrap(),
            r#"{"total_size_mib":1024,"block_size_mib":2,"slot_size_mib":128}"#
        );
    }
}
