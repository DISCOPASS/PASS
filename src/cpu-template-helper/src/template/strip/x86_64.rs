// Copyright 2023 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use vmm::cpu_config::templates::CustomCpuTemplate;
use vmm::cpu_config::x86_64::custom_cpu_template::{CpuidLeafModifier, RegisterModifier};

use crate::template::strip::{strip_common, Error};
use crate::utils::x86_64::{CpuidModifierMap, MsrModifierMap};

#[allow(dead_code)]
pub fn strip(templates: Vec<CustomCpuTemplate>) -> Result<Vec<CustomCpuTemplate>, Error> {
    // Convert `Vec<CustomCpuTemplate>` to two `Vec<HashMap<_>>` of modifiers.
    let (mut cpuid_modifiers_maps, mut msr_modifiers_maps): (Vec<_>, Vec<_>) = templates
        .into_iter()
        .map(|template| {
            (
                CpuidModifierMap::from(template.cpuid_modifiers).0,
                MsrModifierMap::from(template.msr_modifiers).0,
            )
        })
        .unzip();

    // Remove common items.
    strip_common(&mut cpuid_modifiers_maps)?;
    strip_common(&mut msr_modifiers_maps)?;

    // Convert back to `Vec<CustomCpuTemplate>`.
    let templates = cpuid_modifiers_maps
        .into_iter()
        .zip(msr_modifiers_maps.into_iter())
        .map(|(cpuid_modifiers_map, msr_modifiers_map)| {
            let cpuid_modifiers =
                Vec::<CpuidLeafModifier>::from(CpuidModifierMap(cpuid_modifiers_map));
            let msr_modifiers = Vec::<RegisterModifier>::from(MsrModifierMap(msr_modifiers_map));

            CustomCpuTemplate {
                cpuid_modifiers,
                msr_modifiers,
            }
        })
        .collect::<Vec<_>>();

    Ok(templates)
}

#[cfg(test)]
mod tests {
    use vmm::cpu_config::templates::RegisterValueFilter;
    use vmm::cpu_config::x86_64::cpuid::KvmCpuidFlags;
    use vmm::cpu_config::x86_64::custom_cpu_template::CpuidRegister::*;
    use vmm::cpu_config::x86_64::custom_cpu_template::{
        CpuidLeafModifier, CpuidRegisterModifier, RegisterModifier,
    };

    use super::*;
    use crate::utils::x86_64::{cpuid_leaf_modifier, cpuid_reg_modifier, msr_modifier};

    // Summary of CPUID modifiers:
    // * As CPUID leaf 0x0 / subleaf 0x0 modifier exists in all the templates and its values are
    //   different, it should be removed.
    // * As CPUID leaf 0x1 / subleaf 0x0 modifier only exists in the second template, it should be
    //   preserved.
    // * As CPUID leaf 0x2 / subleaf 0x1 modifier exists in all the templates, EAX values are same
    //   but EBX values are different, the EAX register modifier should be removed and the EBX
    //   register modifier should be preserved.
    #[rustfmt::skip]
    fn build_input_cpuid_templates() -> Vec<CustomCpuTemplate> {
        vec![
            CustomCpuTemplate {
                cpuid_modifiers: vec![
                    cpuid_leaf_modifier!(0x0, 0x0, KvmCpuidFlags::EMPTY, vec![
                        cpuid_reg_modifier!(Eax, 0x0),
                    ]),
                    cpuid_leaf_modifier!(0x2, 0x1, KvmCpuidFlags::SIGNIFICANT_INDEX, vec![
                        cpuid_reg_modifier!(Eax, 0x0),
                        cpuid_reg_modifier!(Ebx, 0x0),
                    ]),
                ],
                msr_modifiers: vec![],
            },
            CustomCpuTemplate {
                cpuid_modifiers: vec![
                    cpuid_leaf_modifier!(0x0, 0x0, KvmCpuidFlags::EMPTY, vec![
                        cpuid_reg_modifier!(Eax, 0x0),
                    ]),
                    cpuid_leaf_modifier!(0x1, 0x0, KvmCpuidFlags::EMPTY, vec![
                        cpuid_reg_modifier!(Eax, 0x0),
                    ]),
                    cpuid_leaf_modifier!(0x2, 0x1, KvmCpuidFlags::SIGNIFICANT_INDEX, vec![
                        cpuid_reg_modifier!(Eax, 0x0),
                        cpuid_reg_modifier!(Ebx, 0x1),
                    ]),
                ],
                msr_modifiers: vec![],
            },
            CustomCpuTemplate {
                cpuid_modifiers: vec![
                    cpuid_leaf_modifier!(0x0, 0x0, KvmCpuidFlags::EMPTY, vec![
                        cpuid_reg_modifier!(Eax, 0x0),
                    ]),
                    cpuid_leaf_modifier!(0x2, 0x1, KvmCpuidFlags::SIGNIFICANT_INDEX, vec![
                        cpuid_reg_modifier!(Eax, 0x0),
                        cpuid_reg_modifier!(Ebx, 0x2),
                    ]),
                ],
                msr_modifiers: vec![],
            },
        ]
    }

    #[rustfmt::skip]
    fn build_expected_cpuid_templates() -> Vec<CustomCpuTemplate> {
        vec![
            CustomCpuTemplate {
                cpuid_modifiers: vec![
                    cpuid_leaf_modifier!(0x2, 0x1, KvmCpuidFlags::SIGNIFICANT_INDEX, vec![
                        cpuid_reg_modifier!(Ebx, 0x0),
                    ]),
                ],
                msr_modifiers: vec![],
            },
            CustomCpuTemplate {
                cpuid_modifiers: vec![
                    cpuid_leaf_modifier!(0x1, 0x0, KvmCpuidFlags::EMPTY, vec![
                        cpuid_reg_modifier!(Eax, 0x0),
                    ]),
                    cpuid_leaf_modifier!(0x2, 0x1, KvmCpuidFlags::SIGNIFICANT_INDEX, vec![
                        cpuid_reg_modifier!(Ebx, 0x1),
                    ]),
                ],
                msr_modifiers: vec![],
            },
            CustomCpuTemplate {
                cpuid_modifiers: vec![
                    cpuid_leaf_modifier!(0x2, 0x1, KvmCpuidFlags::SIGNIFICANT_INDEX, vec![
                        cpuid_reg_modifier!(Ebx, 0x2),
                    ]),
                ],
                msr_modifiers: vec![],
            },
        ]
    }

    // Summary of MSR modifiers:
    // * As addr 0x0 modifier exists in all the templates but its values are different, it should be
    //   preserved.
    // * As addr 0x1 modifier exists in all the templates and its values are same, it should be
    //   removed.
    // * As addr 0x2 modifier only exist in the third template, it should be preserved.
    #[rustfmt::skip]
    fn build_input_msr_templates() -> Vec<CustomCpuTemplate> {
        vec![
            CustomCpuTemplate {
                cpuid_modifiers: vec![],
                msr_modifiers: vec![
                    msr_modifier!(0x0, 0x0),
                    msr_modifier!(0x1, 0x1),
                ],
            },
            CustomCpuTemplate {
                cpuid_modifiers: vec![],
                msr_modifiers: vec![
                    msr_modifier!(0x0, 0x1),
                    msr_modifier!(0x1, 0x1),
                ],
            },
            CustomCpuTemplate {
                cpuid_modifiers: vec![],
                msr_modifiers: vec![
                    msr_modifier!(0x0, 0x2),
                    msr_modifier!(0x1, 0x1),
                    msr_modifier!(0x2, 0x1),
                ],
            },
        ]
    }

    #[rustfmt::skip]
    fn build_expected_msr_templates() -> Vec<CustomCpuTemplate> {
        vec![
            CustomCpuTemplate {
                cpuid_modifiers: vec![],
                msr_modifiers: vec![
                    msr_modifier!(0x0, 0x0),
                ],
            },
            CustomCpuTemplate {
                cpuid_modifiers: vec![],
                msr_modifiers: vec![
                    msr_modifier!(0x0, 0x1),
                ],
            },
            CustomCpuTemplate {
                cpuid_modifiers: vec![],
                msr_modifiers: vec![
                    msr_modifier!(0x0, 0x2),
                    msr_modifier!(0x2, 0x1),
                ],
            },
        ]
    }

    #[test]
    fn test_strip_cpuid_modifiers() {
        let input = build_input_cpuid_templates();
        let result = strip(input).unwrap();
        let expected = build_expected_cpuid_templates();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_strip_msr_modifiers() {
        let input = build_input_msr_templates();
        let result = strip(input).unwrap();
        let expected = build_expected_msr_templates();
        assert_eq!(result, expected);
    }
}
