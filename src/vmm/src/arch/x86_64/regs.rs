// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::{fmt, mem};

use kvm_bindings::{kvm_fpu, kvm_regs, kvm_sregs};
use kvm_ioctls::VcpuFd;
use utils::vm_memory::{Address, Bytes, GuestAddress, GuestMemory, GuestMemoryMmap};

use super::gdt::{gdt_entry, kvm_segment_from_gdt};

// Initial pagetables.
const PML4_START: u64 = 0x9000;
const PDPTE_START: u64 = 0xa000;
const PDE_START: u64 = 0xb000;

/// Errors thrown while setting up x86_64 registers.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    /// Failed to get SREGs for this CPU.
    #[error("Failed to get SREGs for this CPU: {0}")]
    GetStatusRegisters(kvm_ioctls::Error),
    /// Failed to set base registers for this CPU.
    #[error("Failed to set base registers for this CPU: {0}")]
    SetBaseRegisters(kvm_ioctls::Error),
    /// Failed to configure the FPU.
    #[error("Failed to configure the FPU: {0}")]
    SetFPURegisters(kvm_ioctls::Error),
    /// Failed to set SREGs for this CPU.
    #[error("Failed to set SREGs for this CPU: {0}")]
    SetStatusRegisters(kvm_ioctls::Error),
    /// Writing the GDT to RAM failed.
    #[error("Writing the GDT to RAM failed.")]
    WriteGDT,
    /// Writing the IDT to RAM failed.
    #[error("Writing the IDT to RAM failed")]
    WriteIDT,
    /// Writing PDPTE to RAM failed.
    #[error("WritePDPTEAddress")]
    WritePDPTEAddress,
    /// Writing PDE to RAM failed.
    #[error("WritePDEAddress")]
    WritePDEAddress,
    /// Writing PML4 to RAM failed.
    #[error("WritePML4Address")]
    WritePML4Address,
}
type Result<T> = std::result::Result<T, Error>;

/// Error type for [`setup_fpu`].
#[derive(Debug, derive_more::From, PartialEq, Eq)]
pub struct SetupFpuError(utils::errno::Error);
impl std::error::Error for SetupFpuError {}
impl fmt::Display for SetupFpuError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Failed to setup FPU: {}", self.0)
    }
}

/// Configure Floating-Point Unit (FPU) registers for a given CPU.
///
/// # Arguments
///
/// * `vcpu` - Structure for the VCPU that holds the VCPU's fd.
///
/// # Errors
///
/// When [`kvm_ioctls::ioctls::vcpu::VcpuFd::set_fpu`] errors.
pub fn setup_fpu(vcpu: &VcpuFd) -> std::result::Result<(), SetupFpuError> {
    let fpu: kvm_fpu = kvm_fpu {
        fcw: 0x37f,
        mxcsr: 0x1f80,
        ..Default::default()
    };

    vcpu.set_fpu(&fpu).map_err(SetupFpuError)
}

/// Error type of [`setup_regs`].
#[derive(Debug, derive_more::From, PartialEq, Eq)]
pub struct SetupRegistersError(utils::errno::Error);
impl std::error::Error for SetupRegistersError {}
impl fmt::Display for SetupRegistersError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Failed to setup registers:{}", self.0)
    }
}

/// Configure base registers for a given CPU.
///
/// # Arguments
///
/// * `vcpu` - Structure for the VCPU that holds the VCPU's fd.
/// * `boot_ip` - Starting instruction pointer.
///
/// # Errors
///
/// When [`kvm_ioctls::ioctls::vcpu::VcpuFd::set_regs`] errors.
pub fn setup_regs(vcpu: &VcpuFd, boot_ip: u64) -> std::result::Result<(), SetupRegistersError> {
    let regs: kvm_regs = kvm_regs {
        rflags: 0x0000_0000_0000_0002u64,
        rip: boot_ip,
        // Frame pointer. It gets a snapshot of the stack pointer (rsp) so that when adjustments are
        // made to rsp (i.e. reserving space for local variables or pushing values on to the stack),
        // local variables and function parameters are still accessible from a constant offset from
        // rbp.
        rsp: super::layout::BOOT_STACK_POINTER,
        // Starting stack pointer.
        rbp: super::layout::BOOT_STACK_POINTER,
        // Must point to zero page address per Linux ABI. This is x86_64 specific.
        rsi: super::layout::ZERO_PAGE_START,
        ..Default::default()
    };

    vcpu.set_regs(&regs).map_err(SetupRegistersError)
}

/// Error type for [`setup_sregs`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SetupSpecialRegistersError {
    /// Failed to get special registers
    #[error("Failed to get special registers: {0}")]
    GetSpecialRegisters(utils::errno::Error),
    /// Failed to configure segments and special registers
    #[error("Failed to configure segments and special registers: {0}")]
    ConfigureSegmentsAndSpecialRegisters(Error),
    /// Failed to setup page tables
    #[error("Failed to setup page tables: {0}")]
    SetupPageTables(Error),
    /// Failed to set special registers
    #[error("Failed to set special registers: {0}")]
    SetSpecialRegisters(utils::errno::Error),
}

/// Configures the special registers and system page tables for a given CPU.
///
/// # Arguments
///
/// * `mem` - The memory that will be passed to the guest.
/// * `vcpu` - Structure for the VCPU that holds the VCPU's fd.
///
/// # Errors
///
/// When:
/// - [`kvm_ioctls::ioctls::vcpu::VcpuFd::get_sregs`] errors.
/// - [`configure_segments_and_sregs`] errors.
/// - [`setup_page_tables`] errors
/// - [`kvm_ioctls::ioctls::vcpu::VcpuFd::set_sregs`] errors.
pub fn setup_sregs(
    mem: &GuestMemoryMmap,
    vcpu: &VcpuFd,
) -> std::result::Result<(), SetupSpecialRegistersError> {
    let mut sregs: kvm_sregs = vcpu
        .get_sregs()
        .map_err(SetupSpecialRegistersError::GetSpecialRegisters)?;

    configure_segments_and_sregs(mem, &mut sregs)
        .map_err(SetupSpecialRegistersError::ConfigureSegmentsAndSpecialRegisters)?;
    setup_page_tables(mem, &mut sregs).map_err(SetupSpecialRegistersError::SetupPageTables)?; // TODO(dgreid) - Can this be done once per system instead?

    vcpu.set_sregs(&sregs)
        .map_err(SetupSpecialRegistersError::SetSpecialRegisters)
}

const BOOT_GDT_OFFSET: u64 = 0x500;
const BOOT_IDT_OFFSET: u64 = 0x520;

const BOOT_GDT_MAX: usize = 4;

const EFER_LMA: u64 = 0x400;
const EFER_LME: u64 = 0x100;

const X86_CR0_PE: u64 = 0x1;
const X86_CR0_PG: u64 = 0x8000_0000;
const X86_CR4_PAE: u64 = 0x20;

fn write_gdt_table(table: &[u64], guest_mem: &GuestMemoryMmap) -> Result<()> {
    let boot_gdt_addr = GuestAddress(BOOT_GDT_OFFSET);
    for (index, entry) in table.iter().enumerate() {
        let addr = guest_mem
            .checked_offset(boot_gdt_addr, index * mem::size_of::<u64>())
            .ok_or(Error::WriteGDT)?;
        guest_mem
            .write_obj(*entry, addr)
            .map_err(|_| Error::WriteGDT)?;
    }
    Ok(())
}

fn write_idt_value(val: u64, guest_mem: &GuestMemoryMmap) -> Result<()> {
    let boot_idt_addr = GuestAddress(BOOT_IDT_OFFSET);
    guest_mem
        .write_obj(val, boot_idt_addr)
        .map_err(|_| Error::WriteIDT)
}

fn configure_segments_and_sregs(mem: &GuestMemoryMmap, sregs: &mut kvm_sregs) -> Result<()> {
    let gdt_table: [u64; BOOT_GDT_MAX] = [
        gdt_entry(0, 0, 0),            // NULL
        gdt_entry(0xa09b, 0, 0xfffff), // CODE
        gdt_entry(0xc093, 0, 0xfffff), // DATA
        gdt_entry(0x808b, 0, 0xfffff), // TSS
    ];

    let code_seg = kvm_segment_from_gdt(gdt_table[1], 1);
    let data_seg = kvm_segment_from_gdt(gdt_table[2], 2);
    let tss_seg = kvm_segment_from_gdt(gdt_table[3], 3);

    // Write segments
    write_gdt_table(&gdt_table[..], mem)?;
    sregs.gdt.base = BOOT_GDT_OFFSET;
    sregs.gdt.limit = mem::size_of_val(&gdt_table) as u16 - 1;

    write_idt_value(0, mem)?;
    sregs.idt.base = BOOT_IDT_OFFSET;
    sregs.idt.limit = mem::size_of::<u64>() as u16 - 1;

    sregs.cs = code_seg;
    sregs.ds = data_seg;
    sregs.es = data_seg;
    sregs.fs = data_seg;
    sregs.gs = data_seg;
    sregs.ss = data_seg;
    sregs.tr = tss_seg;

    // 64-bit protected mode
    sregs.cr0 |= X86_CR0_PE;
    sregs.efer |= EFER_LME | EFER_LMA;

    Ok(())
}

fn setup_page_tables(mem: &GuestMemoryMmap, sregs: &mut kvm_sregs) -> Result<()> {
    // Puts PML4 right after zero page but aligned to 4k.
    let boot_pml4_addr = GuestAddress(PML4_START);
    let boot_pdpte_addr = GuestAddress(PDPTE_START);
    let boot_pde_addr = GuestAddress(PDE_START);

    // Entry covering VA [0..512GB)
    mem.write_obj(boot_pdpte_addr.raw_value() | 0x03, boot_pml4_addr)
        .map_err(|_| Error::WritePML4Address)?;

    // Entry covering VA [0..1GB)
    mem.write_obj(boot_pde_addr.raw_value() | 0x03, boot_pdpte_addr)
        .map_err(|_| Error::WritePDPTEAddress)?;
    // 512 2MB entries together covering VA [0..1GB). Note we are assuming
    // CPU supports 2MB pages (/proc/cpuinfo has 'pse'). All modern CPUs do.
    for i in 0..512 {
        mem.write_obj((i << 21) + 0x83u64, boot_pde_addr.unchecked_add(i * 8))
            .map_err(|_| Error::WritePDEAddress)?;
    }

    sregs.cr3 = boot_pml4_addr.raw_value();
    sregs.cr4 |= X86_CR4_PAE;
    sregs.cr0 |= X86_CR0_PG;
    Ok(())
}

#[cfg(test)]
mod tests {
    use kvm_ioctls::Kvm;
    use utils::vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

    use super::*;

    fn create_guest_mem(mem_size: Option<u64>) -> GuestMemoryMmap {
        let page_size = 0x10000usize;
        let mem_size = mem_size.unwrap_or(page_size as u64) as usize;
        if mem_size % page_size == 0 {
            utils::vm_memory::test_utils::create_anon_guest_memory(
                &[(GuestAddress(0), mem_size)],
                false,
            )
            .unwrap()
        } else {
            utils::vm_memory::test_utils::create_guest_memory_unguarded(
                &[(GuestAddress(0), mem_size)],
                false,
            )
            .unwrap()
        }
    }

    fn read_u64(gm: &GuestMemoryMmap, offset: u64) -> u64 {
        let read_addr = GuestAddress(offset);
        gm.read_obj(read_addr).unwrap()
    }

    fn validate_segments_and_sregs(gm: &GuestMemoryMmap, sregs: &kvm_sregs) {
        assert_eq!(0x0, read_u64(gm, BOOT_GDT_OFFSET));
        assert_eq!(0xaf_9b00_0000_ffff, read_u64(gm, BOOT_GDT_OFFSET + 8));
        assert_eq!(0xcf_9300_0000_ffff, read_u64(gm, BOOT_GDT_OFFSET + 16));
        assert_eq!(0x8f_8b00_0000_ffff, read_u64(gm, BOOT_GDT_OFFSET + 24));
        assert_eq!(0x0, read_u64(gm, BOOT_IDT_OFFSET));

        assert_eq!(0, sregs.cs.base);
        assert_eq!(0xfffff, sregs.ds.limit);
        assert_eq!(0x10, sregs.es.selector);
        assert_eq!(1, sregs.fs.present);
        assert_eq!(1, sregs.gs.g);
        assert_eq!(0, sregs.ss.avl);
        assert_eq!(0, sregs.tr.base);
        assert_eq!(0xfffff, sregs.tr.limit);
        assert_eq!(0, sregs.tr.avl);
        assert!(sregs.cr0 & X86_CR0_PE != 0);
        assert!(sregs.efer & EFER_LME != 0 && sregs.efer & EFER_LMA != 0);
    }

    fn validate_page_tables(gm: &GuestMemoryMmap, sregs: &kvm_sregs) {
        assert_eq!(0xa003, read_u64(gm, PML4_START));
        assert_eq!(0xb003, read_u64(gm, PDPTE_START));
        for i in 0..512 {
            assert_eq!((i << 21) + 0x83u64, read_u64(gm, PDE_START + (i * 8)));
        }

        assert_eq!(PML4_START, sregs.cr3);
        assert!(sregs.cr4 & X86_CR4_PAE != 0);
        assert!(sregs.cr0 & X86_CR0_PG != 0);
    }

    #[test]
    fn test_setup_fpu() {
        let kvm = Kvm::new().unwrap();
        let vm = kvm.create_vm().unwrap();
        let vcpu = vm.create_vcpu(0).unwrap();
        setup_fpu(&vcpu).unwrap();

        let expected_fpu: kvm_fpu = kvm_fpu {
            fcw: 0x37f,
            mxcsr: 0x1f80,
            ..Default::default()
        };
        let actual_fpu: kvm_fpu = vcpu.get_fpu().unwrap();
        // TODO: auto-generate kvm related structures with PartialEq on.
        assert_eq!(expected_fpu.fcw, actual_fpu.fcw);
        // Setting the mxcsr register from kvm_fpu inside setup_fpu does not influence anything.
        // See 'kvm_arch_vcpu_ioctl_set_fpu' from arch/x86/kvm/x86.c.
        // The mxcsr will stay 0 and the assert below fails. Decide whether or not we should
        // remove it at all.
        // assert!(expected_fpu.mxcsr == actual_fpu.mxcsr);
    }

    #[test]
    fn test_setup_regs() {
        let kvm = Kvm::new().unwrap();
        let vm = kvm.create_vm().unwrap();
        let vcpu = vm.create_vcpu(0).unwrap();

        let expected_regs: kvm_regs = kvm_regs {
            rflags: 0x0000_0000_0000_0002u64,
            rip: 1,
            rsp: super::super::layout::BOOT_STACK_POINTER,
            rbp: super::super::layout::BOOT_STACK_POINTER,
            rsi: super::super::layout::ZERO_PAGE_START,
            ..Default::default()
        };

        setup_regs(&vcpu, expected_regs.rip).unwrap();

        let actual_regs: kvm_regs = vcpu.get_regs().unwrap();
        assert_eq!(actual_regs, expected_regs);
    }

    #[test]
    fn test_setup_sregs() {
        let kvm = Kvm::new().unwrap();
        let vm = kvm.create_vm().unwrap();
        let vcpu = vm.create_vcpu(0).unwrap();
        let gm = create_guest_mem(None);

        assert!(vcpu.set_sregs(&Default::default()).is_ok());
        setup_sregs(&gm, &vcpu).unwrap();

        let mut sregs: kvm_sregs = vcpu.get_sregs().unwrap();
        // for AMD KVM_GET_SREGS returns g = 0 for each kvm_segment.
        // We set it to 1, otherwise the test will fail.
        sregs.gs.g = 1;

        validate_segments_and_sregs(&gm, &sregs);
        validate_page_tables(&gm, &sregs);
    }

    #[test]
    fn test_write_gdt_table() {
        // Not enough memory for the gdt table to be written.
        let gm = create_guest_mem(Some(BOOT_GDT_OFFSET));
        let gdt_table: [u64; BOOT_GDT_MAX] = [
            gdt_entry(0, 0, 0),            // NULL
            gdt_entry(0xa09b, 0, 0xfffff), // CODE
            gdt_entry(0xc093, 0, 0xfffff), // DATA
            gdt_entry(0x808b, 0, 0xfffff), // TSS
        ];
        assert!(write_gdt_table(&gdt_table, &gm).is_err());

        // We allocate exactly the amount needed to write four u64 to `BOOT_GDT_OFFSET`.
        let gm = create_guest_mem(Some(
            BOOT_GDT_OFFSET + (mem::size_of::<u64>() * BOOT_GDT_MAX) as u64,
        ));

        let gdt_table: [u64; BOOT_GDT_MAX] = [
            gdt_entry(0, 0, 0),            // NULL
            gdt_entry(0xa09b, 0, 0xfffff), // CODE
            gdt_entry(0xc093, 0, 0xfffff), // DATA
            gdt_entry(0x808b, 0, 0xfffff), // TSS
        ];
        assert!(write_gdt_table(&gdt_table, &gm).is_ok());
    }

    #[test]
    fn test_write_idt_table() {
        // Not enough memory for the a u64 value to fit.
        let gm = create_guest_mem(Some(BOOT_IDT_OFFSET));
        let val = 0x100;
        assert!(write_idt_value(val, &gm).is_err());

        let gm = create_guest_mem(Some(BOOT_IDT_OFFSET + mem::size_of::<u64>() as u64));
        // We have allocated exactly the amount neded to write an u64 to `BOOT_IDT_OFFSET`.
        assert!(write_idt_value(val, &gm).is_ok());
    }

    #[test]
    fn test_configure_segments_and_sregs() {
        let mut sregs: kvm_sregs = Default::default();
        let gm = create_guest_mem(None);
        configure_segments_and_sregs(&gm, &mut sregs).unwrap();

        validate_segments_and_sregs(&gm, &sregs);
    }

    #[test]
    fn test_setup_page_tables() {
        let mut sregs: kvm_sregs = Default::default();
        let gm = create_guest_mem(Some(PML4_START));
        assert!(setup_page_tables(&gm, &mut sregs).is_err());

        let gm = create_guest_mem(Some(PDPTE_START));
        assert!(setup_page_tables(&gm, &mut sregs).is_err());

        let gm = create_guest_mem(Some(PDE_START));
        assert!(setup_page_tables(&gm, &mut sregs).is_err());

        let gm = create_guest_mem(None);
        setup_page_tables(&gm, &mut sregs).unwrap();

        validate_page_tables(&gm, &sregs);
    }
}
