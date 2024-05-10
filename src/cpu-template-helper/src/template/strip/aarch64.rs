// Copyright 2023 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use vmm::cpu_config::aarch64::custom_cpu_template::RegisterModifier;
use vmm::cpu_config::templates::CustomCpuTemplate;

use crate::template::strip::{strip_common, Error};
use crate::utils::aarch64::RegModifierMap;

#[allow(dead_code)]
pub fn strip(templates: Vec<CustomCpuTemplate>) -> Result<Vec<CustomCpuTemplate>, Error> {
    // Convert `Vec<CustomCpuTemplate>` to `Vec<HashMap<_>>`.
    let mut reg_modifiers_maps = templates
        .into_iter()
        .map(|template| RegModifierMap::from(template.reg_modifiers).0)
        .collect::<Vec<_>>();

    // Remove common items.
    strip_common(&mut reg_modifiers_maps)?;

    // Convert back to `Vec<CustomCpuTemplate>`.
    let templates = reg_modifiers_maps
        .into_iter()
        .map(|reg_modifiers_map| {
            let reg_modifiers = Vec::<RegisterModifier>::from(RegModifierMap(reg_modifiers_map));
            CustomCpuTemplate { reg_modifiers }
        })
        .collect();

    Ok(templates)
}

#[cfg(test)]
mod tests {
    use vmm::cpu_config::aarch64::custom_cpu_template::RegisterModifier;
    use vmm::cpu_config::templates::RegisterValueFilter;

    use super::*;
    use crate::utils::aarch64::reg_modifier;

    // Summary of reg modifiers:
    // * As addr 0x0 modifier exists in all the templates but its values are different, it should be
    //   preserved.
    // * As addr 0x1 modifier exists in all the templates and its values are same, it should be
    //   removed.
    // * As addr 0x2 modifier only exist in the third template, it should be preserved.
    #[rustfmt::skip]
    fn build_input_templates() -> Vec<CustomCpuTemplate> {
        vec![
            CustomCpuTemplate {
                reg_modifiers: vec![
                    reg_modifier!(0x0, 0x0),
                    reg_modifier!(0x1, 0x1),
                ],
            },
            CustomCpuTemplate {
                reg_modifiers: vec![
                    reg_modifier!(0x0, 0x1),
                    reg_modifier!(0x1, 0x1),
                ],
            },
            CustomCpuTemplate {
                reg_modifiers: vec![
                    reg_modifier!(0x0, 0x2),
                    reg_modifier!(0x1, 0x1),
                    reg_modifier!(0x2, 0x1),
                ],
            },
        ]
    }

    #[rustfmt::skip]
    fn build_expected_templates() -> Vec<CustomCpuTemplate> {
        vec![
            CustomCpuTemplate {
                reg_modifiers: vec![
                    reg_modifier!(0x0, 0x0),
                ],
            },
            CustomCpuTemplate {
                reg_modifiers: vec![
                    reg_modifier!(0x0, 0x1),
                ],
            },
            CustomCpuTemplate {
                reg_modifiers: vec![
                    reg_modifier!(0x0, 0x2),
                    reg_modifier!(0x2, 0x1),
                ],
            },
        ]
    }

    #[test]
    fn test_strip_reg_modifiers() {
        let input = build_input_templates();
        let result = strip(input).unwrap();
        let expected = build_expected_templates();
        assert_eq!(result, expected);
    }
}
