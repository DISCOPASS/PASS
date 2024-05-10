// Copyright 2023 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use crate::utils::{ModifierMapKey, ModifierMapValue};

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "aarch64")]
pub use aarch64::verify;
#[cfg(target_arch = "x86_64")]
mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use x86_64::verify;

#[rustfmt::skip]
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0} not found in CPU configuration.")]
    KeyNotFound(String),
    #[error("Value for {0} mismatched.\n{1}")]
    ValueMismatched(String, String),
}

/// Verify that the given CPU template is applied as intended.
///
/// This function is an arch-agnostic part of CPU template verification. As template formats differ
/// between x86_64 and aarch64, the arch-specific part converts the structure to an arch-agnostic
/// `HashMap` implementing `ModifierMapKey` and `ModifierMapValue` for its key and value
/// respectively before calling this arch-agnostic function.
pub fn verify_common<K, V>(template: HashMap<K, V>, config: HashMap<K, V>) -> Result<(), Error>
where
    K: ModifierMapKey,
    V: ModifierMapValue,
{
    for (key, template_value_filter) in template {
        let config_value_filter = config
            .get(&key)
            .ok_or(Error::KeyNotFound(key.to_string()))?;

        let template_value = template_value_filter.value() & template_value_filter.filter();
        let config_value = config_value_filter.value() & template_value_filter.filter();

        if template_value != config_value {
            return Err(Error::ValueMismatched(
                key.to_string(),
                V::to_diff_string(template_value, config_value),
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::tests::{mock_modifier, MockModifierMapKey, MockModifierMapValue};

    #[test]
    fn test_verify_modifier_map_with_non_existing_key() {
        // Test with a sample whose key exists in CPU template but not in CPU config.
        let cpu_template_map =
            HashMap::from([mock_modifier!(0b0000_0000, (0b0000_0000, 0b0000_0000))]);
        let cpu_config_map = HashMap::new();

        assert_eq!(
            verify_common(cpu_template_map, cpu_config_map)
                .unwrap_err()
                .to_string(),
            "ID=0x0 not found in CPU configuration.".to_string()
        );
    }

    #[test]
    #[rustfmt::skip]
    fn test_verify_modifier_map_with_mismatched_value() {
        // Test with a sample whose filtered value mismatches between CPU config and CPU template.
        let cpu_template_map =
            HashMap::from([mock_modifier!(0b0000_0000, (0b0000_1111, 0b0000_0101))]);
        let cpu_config_map =
            HashMap::from([mock_modifier!(0b0000_0000, (u8::MAX, 0b0000_0000))]);

        assert_eq!(
            verify_common(cpu_template_map, cpu_config_map)
                .unwrap_err()
                .to_string(),
            "Value for ID=0x0 mismatched.\n\
             * CPU template     : 0b00000101\n\
             * CPU configuration: 0b00000000\n\
             * Diff             :        ^ ^"
        )
    }

    #[test]
    fn test_verify_modifier_map_with_valid_value() {
        // Test with valid CPU template and CPU config.
        let cpu_template_map =
            HashMap::from([mock_modifier!(0b0000_0000, (0b0000_1111, 0b0000_1010))]);
        let cpu_config_map = HashMap::from([mock_modifier!(0b0000_0000, (u8::MAX, 0b1010_1010))]);

        verify_common(cpu_template_map, cpu_config_map).unwrap();
    }
}
