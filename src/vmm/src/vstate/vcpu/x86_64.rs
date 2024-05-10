// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::collections::{HashMap, HashSet};

use kvm_bindings::{
    kvm_debugregs, kvm_lapic_state, kvm_mp_state, kvm_regs, kvm_sregs, kvm_vcpu_events, kvm_xcrs,
    kvm_xsave, CpuId, Msrs, KVM_MAX_CPUID_ENTRIES, KVM_MAX_MSR_ENTRIES,
};
use kvm_ioctls::{VcpuExit, VcpuFd};
use logger::{error, warn, IncMetric, METRICS};
use utils::vm_memory::{Address, GuestAddress, GuestMemoryMmap};
use versionize::{VersionMap, Versionize, VersionizeError, VersionizeResult};
use versionize_derive::Versionize;

use crate::arch::x86_64::interrupts;
use crate::arch::x86_64::msr::{create_boot_msr_entries, Error as MsrError};
use crate::arch::x86_64::regs::{SetupFpuError, SetupRegistersError, SetupSpecialRegistersError};
use crate::cpu_config::x86_64::{cpuid, CpuConfiguration};
use crate::vstate::vcpu::{VcpuConfig, VcpuEmulation};
use crate::vstate::vm::Vm;

// Tolerance for TSC frequency expected variation.
// The value of 250 parts per million is based on
// the QEMU approach, more details here:
// https://bugzilla.redhat.com/show_bug.cgi?id=1839095
const TSC_KHZ_TOL: f64 = 250.0 / 1_000_000.0;

/// Errors associated with the wrappers over KVM ioctls.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Failed to convert CPUID type.
    #[error("Failed to convert `kvm_bindings::CpuId` to `Cpuid`: {0}")]
    ConvertCpuidType(#[from] cpuid::CpuidTryFromKvmCpuid),
    /// A FamStructWrapper operation has failed.
    #[error("Failed FamStructWrapper operation: {0:?}")]
    Fam(#[from] utils::fam::Error),
    /// Error configuring the floating point related registers
    #[error("Error configuring the floating point related registers: {0:?}")]
    FpuConfiguration(crate::arch::x86_64::regs::Error),
    /// Failed to get dumpable MSR index list.
    #[error("Failed to get dumpable MSR index list: {0}")]
    GetMsrsToDump(#[from] crate::arch::x86_64::msr::Error),
    /// Cannot set the local interruption due to bad configuration.
    #[error("Cannot set the local interruption due to bad configuration: {0:?}")]
    LocalIntConfiguration(crate::arch::x86_64::interrupts::Error),
    /// Error configuring the general purpose registers
    #[error("Error configuring the general purpose registers: {0:?}")]
    RegsConfiguration(crate::arch::x86_64::regs::Error),
    /// Error configuring the special registers
    #[error("Error configuring the special registers: {0:?}")]
    SregsConfiguration(crate::arch::x86_64::regs::Error),
    /// Cannot open the VCPU file descriptor.
    #[error("Cannot open the VCPU file descriptor: {0}")]
    VcpuFd(kvm_ioctls::Error),
    /// Failed to get KVM vcpu debug regs.
    #[error("Failed to get KVM vcpu debug regs: {0}")]
    VcpuGetDebugRegs(kvm_ioctls::Error),
    /// Failed to get KVM vcpu lapic.
    #[error("Failed to get KVM vcpu lapic: {0}")]
    VcpuGetLapic(kvm_ioctls::Error),
    /// Failed to get KVM vcpu mp state.
    #[error("Failed to get KVM vcpu mp state: {0}")]
    VcpuGetMpState(kvm_ioctls::Error),
    /// The number of MSRS returned by the kernel is unexpected.
    #[error("Unexpected number of MSRS reported by the kernel")]
    VcpuGetMsrsIncomplete,
    /// Failed to get KVM vcpu msrs.
    #[error("Failed to get KVM vcpu msrs: {0}")]
    VcpuGetMsrs(kvm_ioctls::Error),
    /// Failed to get KVM vcpu regs.
    #[error("Failed to get KVM vcpu regs: {0}")]
    VcpuGetRegs(kvm_ioctls::Error),
    /// Failed to get KVM vcpu sregs.
    #[error("Failed to get KVM vcpu sregs: {0}")]
    VcpuGetSregs(kvm_ioctls::Error),
    /// Failed to get KVM vcpu event.
    #[error("Failed to get KVM vcpu event: {0}")]
    VcpuGetVcpuEvents(kvm_ioctls::Error),
    /// Failed to get KVM vcpu xcrs.
    #[error("Failed to get KVM vcpu xcrs: {0}")]
    VcpuGetXcrs(kvm_ioctls::Error),
    /// Failed to get KVM vcpu xsave.
    #[error("Failed to get KVM vcpu xsave: {0}")]
    VcpuGetXsave(kvm_ioctls::Error),
    /// Failed to get KVM vcpu cpuid.
    #[error("Failed to get KVM vcpu cpuid: {0}")]
    VcpuGetCpuid(kvm_ioctls::Error),
    /// Failed to get KVM TSC freq.
    #[error("Failed to get KVM TSC frequency: {0}")]
    VcpuGetTsc(kvm_ioctls::Error),
    /// Failed to set KVM vcpu cpuid.
    #[error("Failed to set KVM vcpu cpuid: {0}")]
    VcpuSetCpuid(kvm_ioctls::Error),
    /// Failed to set KVM vcpu debug regs.
    #[error("Failed to set KVM vcpu debug regs: {0}")]
    VcpuSetDebugRegs(kvm_ioctls::Error),
    /// Failed to set KVM vcpu lapic.
    #[error("Failed to set KVM vcpu lapic: {0}")]
    VcpuSetLapic(kvm_ioctls::Error),
    /// Failed to set KVM vcpu mp state.
    #[error("Failed to set KVM vcpu mp state: {0}")]
    VcpuSetMpState(kvm_ioctls::Error),
    /// Failed to set KVM vcpu msrs.
    #[error("Failed to set KVM vcpu msrs: {0}")]
    VcpuSetMsrs(kvm_ioctls::Error),
    /// Failed to set all KVM vcpu MSRs. Only a partial set was done.
    #[error("Failed to set all KVM MSRs for this vCPU. Only a partial write was done.")]
    VcpuSetMsrsIncomplete,
    /// Failed to set KVM vcpu regs.
    #[error("Failed to set KVM vcpu regs: {0}")]
    VcpuSetRegs(kvm_ioctls::Error),
    /// Failed to set KVM vcpu sregs.
    #[error("Failed to set KVM vcpu sregs: {0}")]
    VcpuSetSregs(kvm_ioctls::Error),
    /// Failed to set KVM vcpu event.
    #[error("Failed to set KVM vcpu event: {0}")]
    VcpuSetVcpuEvents(kvm_ioctls::Error),
    /// Failed to set KVM vcpu xcrs.
    #[error("Failed to set KVM vcpu xcrs: {0}")]
    VcpuSetXcrs(kvm_ioctls::Error),
    /// Failed to set KVM vcpu xsave.
    #[error("Failed to set KVM vcpu xsave: {0}")]
    VcpuSetXsave(kvm_ioctls::Error),
    /// Failed to set KVM TSC freq.
    #[error("Failed to set KVM TSC frequency: {0}")]
    VcpuSetTsc(kvm_ioctls::Error),
    /// Failed to apply CPU template.
    #[error("Failed to apply CPU template")]
    VcpuTemplateError,
}

type Result<T> = std::result::Result<T, Error>;

/// Error type for [`KvmVcpu::get_tsc_khz`] and [`KvmVcpu::is_tsc_scaling_required`].
#[derive(Debug, thiserror::Error, derive_more::From, Eq, PartialEq)]
#[error("{0}")]
pub struct GetTscError(utils::errno::Error);

/// Error type for [`KvmVcpu::set_tsc_khz`].
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
#[error("{0}")]
pub struct SetTscError(#[from] kvm_ioctls::Error);

/// Error type for [`KvmVcpu::configure`].
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum KvmVcpuConfigureError {
    #[error("Failed to convert `Cpuid` to `kvm_bindings::CpuId`: {0}")]
    ConvertCpuidType(#[from] utils::fam::Error),
    /// Failed to apply modifications to CPUID.
    #[error("Failed to apply modifications to CPUID: {0}")]
    NormalizeCpuidError(#[from] cpuid::NormalizeCpuidError),
    #[error("Failed to set CPUID: {0}")]
    SetCpuid(#[from] utils::errno::Error),
    #[error("Failed to set MSRs: {0}")]
    SetMsrs(#[from] MsrError),
    #[error("Failed to setup registers: {0}")]
    SetupRegisters(#[from] SetupRegistersError),
    #[error("Failed to setup FPU: {0}")]
    SetupFpu(#[from] SetupFpuError),
    #[error("Failed to setup special registers: {0}")]
    SetupSpecialRegisters(#[from] SetupSpecialRegistersError),
    #[error("Failed to configure LAPICs: {0}")]
    SetLint(#[from] interrupts::Error),
}

/// A wrapper around creating and using a kvm x86_64 vcpu.
pub struct KvmVcpu {
    pub index: u8,
    pub fd: VcpuFd,

    pub pio_bus: Option<crate::devices::Bus>,
    pub mmio_bus: Option<crate::devices::Bus>,

    msrs_to_save: HashSet<u32>,
}

impl KvmVcpu {
    /// Constructs a new kvm vcpu with arch specific functionality.
    ///
    /// # Arguments
    ///
    /// * `index` - Represents the 0-based CPU index between [0, max vcpus).
    /// * `vm` - The vm to which this vcpu will get attached.
    pub fn new(index: u8, vm: &Vm) -> Result<Self> {
        let kvm_vcpu = vm.fd().create_vcpu(index.into()).map_err(Error::VcpuFd)?;

        Ok(KvmVcpu {
            index,
            fd: kvm_vcpu,
            pio_bus: None,
            mmio_bus: None,
            msrs_to_save: vm.msrs_to_save().as_slice().iter().copied().collect(),
        })
    }

    /// Configures a x86_64 specific vcpu for booting Linux and should be called once per vcpu.
    ///
    /// # Arguments
    ///
    /// * `guest_mem` - The guest memory used by this microvm.
    /// * `kernel_start_addr` - Offset from `guest_mem` at which the kernel starts.
    /// * `vcpu_config` - The vCPU configuration.
    /// * `cpuid` - The capabilities exposed by this vCPU.
    pub fn configure(
        &mut self,
        guest_mem: &GuestMemoryMmap,
        kernel_start_addr: GuestAddress,
        vcpu_config: &VcpuConfig,
    ) -> std::result::Result<(), KvmVcpuConfigureError> {
        let mut cpuid = vcpu_config.cpu_config.cpuid.clone();

        // Apply machine specific changes to CPUID.
        cpuid.normalize(
            // The index of the current logical CPU in the range [0..cpu_count].
            self.index,
            // The total number of logical CPUs.
            vcpu_config.vcpu_count,
            // The number of bits needed to enumerate logical CPUs per core.
            u8::from(vcpu_config.vcpu_count > 1 && vcpu_config.smt),
        )?;

        // Set CPUID.
        let kvm_cpuid = kvm_bindings::CpuId::try_from(cpuid)?;

        // Set CPUID in the KVM
        self.fd
            .set_cpuid2(&kvm_cpuid)
            .map_err(KvmVcpuConfigureError::SetCpuid)?;

        // Clone MSR entries that are modified by CPU template from `VcpuConfig`.
        let mut msrs = vcpu_config.cpu_config.msrs.clone();
        self.msrs_to_save.extend(msrs.keys());

        // Apply MSR modification to comply the linux boot protocol.
        create_boot_msr_entries().into_iter().for_each(|entry| {
            msrs.insert(entry.index, entry.data);
        });

        // TODO - Add/amend MSRs for vCPUs based on cpu_config
        // By this point the Guest CPUID is established. Some CPU features require MSRs
        // to configure and interact with those features. If a MSR is writable from
        // inside the Guest, or is changed by KVM or Firecracker on behalf of the Guest,
        // then we will need to save it every time we take a snapshot, and restore its
        // value when we restore the microVM since the Guest may need that value.
        // Since CPUID tells us what features are enabled for the Guest, we can infer
        // the extra MSRs that we need to save based on a dependency map.
        let extra_msrs = cpuid::common::msrs_to_save_by_cpuid(&kvm_cpuid);
        self.msrs_to_save.extend(extra_msrs);

        // TODO: Some MSRs depend on values of other MSRs. This dependency will need to
        // be implemented.

        // By this point we know that at snapshot, the list of MSRs we need to
        // save is `architectural MSRs` + `MSRs inferred through CPUID` + `other
        // MSRs defined by the template`

        let kvm_msrs = msrs
            .into_iter()
            .map(|entry| kvm_bindings::kvm_msr_entry {
                index: entry.0,
                data: entry.1,
                ..Default::default()
            })
            .collect::<Vec<_>>();

        crate::arch::x86_64::msr::set_msrs(&self.fd, &kvm_msrs)?;
        crate::arch::x86_64::regs::setup_regs(&self.fd, kernel_start_addr.raw_value())?;
        crate::arch::x86_64::regs::setup_fpu(&self.fd)?;
        crate::arch::x86_64::regs::setup_sregs(guest_mem, &self.fd)?;
        crate::arch::x86_64::interrupts::set_lint(&self.fd)?;

        Ok(())
    }

    /// Sets a Port Mapped IO bus for this vcpu.
    pub fn set_pio_bus(&mut self, pio_bus: crate::devices::Bus) {
        self.pio_bus = Some(pio_bus);
    }

    /// Get the current TSC frequency for this vCPU.
    ///
    /// # Errors
    ///
    /// When [`kvm_ioctls::VcpuFd::get_tsc_khz`] errrors.
    pub fn get_tsc_khz(&self) -> std::result::Result<u32, GetTscError> {
        let res = self.fd.get_tsc_khz()?;
        Ok(res)
    }

    /// Get CPUID for this vCPU.
    ///
    /// Opposed to KVM_GET_SUPPORTED_CPUID, KVM_GET_CPUID2 does not update "nent" with valid number
    /// of entries on success. Thus, when it passes "num_entries" greater than required, zeroed
    /// entries follow after valid entries. This function removes such zeroed empty entries.
    ///
    /// # Errors
    ///
    /// * When [`kvm_ioctls::VcpuFd::get_cpuid2`] returns errors.
    fn get_cpuid(&self) -> Result<kvm_bindings::CpuId> {
        let mut cpuid = self
            .fd
            .get_cpuid2(KVM_MAX_CPUID_ENTRIES)
            .map_err(Error::VcpuGetCpuid)?;

        // As CPUID.0h:EAX should have the largest CPUID standard function, we don't need to check
        // EBX, ECX and EDX to confirm whether it is a valid entry.
        cpuid.retain(|entry| {
            !(entry.function == 0 && entry.index == 0 && entry.flags == 0 && entry.eax == 0)
        });

        Ok(cpuid)
    }

    /// Get MSR chunks for the given MSR index list.
    ///
    /// KVM only supports getting `KVM_MAX_MSR_ENTRIES` at a time, so we divide
    /// the list of MSR indices into chunks, call `KVM_GET_MSRS` for each
    /// chunk, and collect into a Vec<Msrs>.
    ///
    /// # Arguments
    ///
    /// * `msr_index_list`: List of MSR indices.
    ///
    /// # Errors
    ///
    /// * When [`kvm_bindings::Msrs::new`] returns errors.
    /// * When [`kvm_ioctls::VcpuFd::get_msrs`] returns errors.
    /// * When the return value of [`kvm_ioctls::VcpuFd::get_msrs`] (the number of entries that
    ///   could be gotten) is less than expected.
    fn get_msr_chunks(&self, msr_index_list: &[u32]) -> Result<Vec<Msrs>> {
        let mut msr_chunks: Vec<Msrs> = Vec::new();

        for msr_index_chunk in msr_index_list.chunks(KVM_MAX_MSR_ENTRIES) {
            let mut msrs = Msrs::new(msr_index_chunk.len())?;
            let msr_entries = msrs.as_mut_slice();
            assert_eq!(msr_index_chunk.len(), msr_entries.len());
            for (pos, index) in msr_index_chunk.iter().enumerate() {
                msr_entries[pos].index = *index;
            }

            let expected_nmsrs = msrs.as_slice().len();
            let nmsrs = self.fd.get_msrs(&mut msrs).map_err(Error::VcpuGetMsrs)?;
            if nmsrs != expected_nmsrs {
                return Err(Error::VcpuGetMsrsIncomplete);
            }

            msr_chunks.push(msrs);
        }

        Ok(msr_chunks)
    }

    /// Get MSRs for the given MSR index list.
    ///
    /// # Arguments
    ///
    /// * `msr_index_list`: List of MSR indices
    ///
    /// # Errors
    ///
    /// * When `KvmVcpu::get_msr_chunks()` returns errors.
    pub fn get_msrs(&self, msr_index_list: &[u32]) -> Result<HashMap<u32, u64>> {
        let mut msrs: HashMap<u32, u64> = HashMap::new();
        self.get_msr_chunks(msr_index_list)?
            .iter()
            .for_each(|msr_chunk| {
                msr_chunk.as_slice().iter().for_each(|msr| {
                    msrs.insert(msr.index, msr.data);
                });
            });
        Ok(msrs)
    }

    /// Save the KVM internal state.
    pub fn save_state(&self) -> Result<VcpuState> {
        // Ordering requirements:
        //
        // KVM_GET_MP_STATE calls kvm_apic_accept_events(), which might modify
        // vCPU/LAPIC state. As such, it must be done before most everything
        // else, otherwise we cannot restore everything and expect it to work.
        //
        // KVM_GET_VCPU_EVENTS/KVM_SET_VCPU_EVENTS is unsafe if other vCPUs are
        // still running.
        //
        // KVM_GET_LAPIC may change state of LAPIC before returning it.
        //
        // GET_VCPU_EVENTS should probably be last to save. The code looks as
        // it might as well be affected by internal state modifications of the
        // GET ioctls.
        //
        // SREGS saves/restores a pending interrupt, similar to what
        // VCPU_EVENTS also does.

        let mp_state = self.fd.get_mp_state().map_err(Error::VcpuGetMpState)?;
        let regs = self.fd.get_regs().map_err(Error::VcpuGetRegs)?;
        let sregs = self.fd.get_sregs().map_err(Error::VcpuGetSregs)?;
        let xsave = self.fd.get_xsave().map_err(Error::VcpuGetXsave)?;
        let xcrs = self.fd.get_xcrs().map_err(Error::VcpuGetXcrs)?;
        let debug_regs = self.fd.get_debug_regs().map_err(Error::VcpuGetDebugRegs)?;
        let lapic = self.fd.get_lapic().map_err(Error::VcpuGetLapic)?;
        let tsc_khz = self.get_tsc_khz().ok().or_else(|| {
            // v0.25 and newer snapshots without TSC will only work on
            // the same CPU model as the host on which they were taken.
            // TODO: Add negative test for this warning failure.
            warn!("TSC freq not available. Snapshot cannot be loaded on a different CPU model.");
            None
        });
        let cpuid = self.get_cpuid()?;
        let saved_msrs =
            self.get_msr_chunks(&self.msrs_to_save.iter().copied().collect::<Vec<_>>())?;
        let vcpu_events = self
            .fd
            .get_vcpu_events()
            .map_err(Error::VcpuGetVcpuEvents)?;

        Ok(VcpuState {
            cpuid,
            saved_msrs,
            msrs: Msrs::new(0)?,
            debug_regs,
            lapic,
            mp_state,
            regs,
            sregs,
            vcpu_events,
            xcrs,
            xsave,
            tsc_khz,
        })
    }

    /// Dumps CPU configuration (CPUID and MSRs).
    ///
    /// Opposed to `save_state()`, this dumps all the supported and dumpable MSRs not limited to
    /// serializable ones.
    pub fn dump_cpu_config(&self) -> Result<CpuConfiguration> {
        let cpuid = cpuid::Cpuid::try_from(self.get_cpuid()?)?;
        let kvm = kvm_ioctls::Kvm::new().unwrap();
        let msr_index_list = crate::arch::x86_64::msr::get_msrs_to_dump(&kvm)?;
        let msrs = self.get_msrs(msr_index_list.as_slice())?;
        Ok(CpuConfiguration { cpuid, msrs })
    }

    /// Checks whether the TSC needs scaling when restoring a snapshot.
    ///
    /// # Errors
    ///
    /// When
    pub fn is_tsc_scaling_required(
        &self,
        state_tsc_freq: u32,
    ) -> std::result::Result<bool, GetTscError> {
        // Compare the current TSC freq to the one found
        // in the state. If they are different, we need to
        // scale the TSC to the freq found in the state.
        // We accept values within a tolerance of 250 parts
        // per million beacuse it is common for TSC frequency
        // to differ due to calibration at boot time.
        let diff = (i64::from(self.get_tsc_khz()?) - i64::from(state_tsc_freq)).abs();
        Ok(diff > (f64::from(state_tsc_freq) * TSC_KHZ_TOL).round() as i64)
    }

    // Scale the TSC frequency of this vCPU to the one provided as a parameter.
    pub fn set_tsc_khz(&self, tsc_freq: u32) -> std::result::Result<(), SetTscError> {
        self.fd.set_tsc_khz(tsc_freq).map_err(SetTscError)
    }

    /// Use provided state to populate KVM internal state.
    pub fn restore_state(&self, state: &VcpuState) -> Result<()> {
        // Ordering requirements:
        //
        // KVM_GET_VCPU_EVENTS/KVM_SET_VCPU_EVENTS is unsafe if other vCPUs are
        // still running.
        //
        // Some SET ioctls (like set_mp_state) depend on kvm_vcpu_is_bsp(), so
        // if we ever change the BSP, we have to do that before restoring anything.
        // The same seems to be true for CPUID stuff.
        //
        // SREGS saves/restores a pending interrupt, similar to what
        // VCPU_EVENTS also does.
        //
        // SET_REGS clears pending exceptions unconditionally, thus, it must be
        // done before SET_VCPU_EVENTS, which restores it.
        //
        // SET_LAPIC must come after SET_SREGS, because the latter restores
        // the apic base msr.
        //
        // SET_LAPIC must come before SET_MSRS, because the TSC deadline MSR
        // only restores successfully, when the LAPIC is correctly configured.

        self.fd
            .set_cpuid2(&state.cpuid)
            .map_err(Error::VcpuSetCpuid)?;
        self.fd
            .set_mp_state(state.mp_state)
            .map_err(Error::VcpuSetMpState)?;
        self.fd.set_regs(&state.regs).map_err(Error::VcpuSetRegs)?;
        self.fd
            .set_sregs(&state.sregs)
            .map_err(Error::VcpuSetSregs)?;
        self.fd
            .set_xsave(&state.xsave)
            .map_err(Error::VcpuSetXsave)?;
        self.fd.set_xcrs(&state.xcrs).map_err(Error::VcpuSetXcrs)?;
        self.fd
            .set_debug_regs(&state.debug_regs)
            .map_err(Error::VcpuSetDebugRegs)?;
        self.fd
            .set_lapic(&state.lapic)
            .map_err(Error::VcpuSetLapic)?;
        for msrs in &state.saved_msrs {
            let nmsrs = self.fd.set_msrs(msrs).map_err(Error::VcpuSetMsrs)?;
            if nmsrs < msrs.as_fam_struct_ref().nmsrs as usize {
                return Err(Error::VcpuSetMsrsIncomplete);
            }
        }
        self.fd
            .set_vcpu_events(&state.vcpu_events)
            .map_err(Error::VcpuSetVcpuEvents)?;
        Ok(())
    }

    /// Runs the vCPU in KVM context and handles the kvm exit reason.
    ///
    /// Returns error or enum specifying whether emulation was handled or interrupted.
    pub fn run_arch_emulation(&self, exit: VcpuExit) -> super::Result<VcpuEmulation> {
        match exit {
            VcpuExit::IoIn(addr, data) => {
                if let Some(pio_bus) = &self.pio_bus {
                    pio_bus.read(u64::from(addr), data);
                    METRICS.vcpu.exit_io_in.inc();
                }
                Ok(VcpuEmulation::Handled)
            }
            VcpuExit::IoOut(addr, data) => {
                if let Some(pio_bus) = &self.pio_bus {
                    pio_bus.write(u64::from(addr), data);
                    METRICS.vcpu.exit_io_out.inc();
                }
                Ok(VcpuEmulation::Handled)
            }
            unexpected_exit => {
                METRICS.vcpu.failures.inc();
                // TODO: Are we sure we want to finish running a vcpu upon
                // receiving a vm exit that is not necessarily an error?
                error!("Unexpected exit reason on vcpu run: {:?}", unexpected_exit);
                Err(super::Error::UnhandledKvmExit(format!(
                    "{:?}",
                    unexpected_exit
                )))
            }
        }
    }
}

#[derive(Clone, Versionize)]
/// Structure holding VCPU kvm state.
// NOTICE: Any changes to this structure require a snapshot version bump.
pub struct VcpuState {
    pub cpuid: CpuId,
    #[version(end = 3, default_fn = "default_msrs")]
    msrs: Msrs,
    #[version(start = 3, de_fn = "de_saved_msrs", ser_fn = "ser_saved_msrs")]
    saved_msrs: Vec<Msrs>,
    debug_regs: kvm_debugregs,
    lapic: kvm_lapic_state,
    mp_state: kvm_mp_state,
    regs: kvm_regs,
    sregs: kvm_sregs,
    vcpu_events: kvm_vcpu_events,
    xcrs: kvm_xcrs,
    xsave: kvm_xsave,
    #[version(start = 2, default_fn = "default_tsc_khz", ser_fn = "ser_tsc")]
    pub tsc_khz: Option<u32>,
}

impl VcpuState {
    fn default_tsc_khz(_: u16) -> Option<u32> {
        warn!("CPU TSC freq not found in snapshot");
        None
    }

    fn ser_tsc(&mut self, _target_version: u16) -> VersionizeResult<()> {
        // v0.24 and older versions do not support TSC scaling.
        warn!(
            "Saving to older snapshot version, TSC freq {}",
            self.tsc_khz
                .map(|freq| freq.to_string() + "KHz not included in snapshot.")
                .unwrap_or_else(|| "not available.".to_string())
        );

        Ok(())
    }

    fn default_msrs(_source_version: u16) -> Msrs {
        // Safe to unwrap since Msrs::new() only returns an error if the number
        // of elements exceeds KVM_MAX_MSR_ENTRIES
        Msrs::new(0).unwrap()
    }

    fn de_saved_msrs(&mut self, source_version: u16) -> VersionizeResult<()> {
        if source_version < 3 {
            self.saved_msrs.push(self.msrs.clone());
        }
        Ok(())
    }

    fn ser_saved_msrs(&mut self, target_version: u16) -> VersionizeResult<()> {
        match self.saved_msrs.len() {
            0 => Err(VersionizeError::Serialize(
                "Cannot serialize MSRs because the MSR list is empty".to_string(),
            )),
            1 => {
                if target_version < 3 {
                    self.msrs = self.saved_msrs[0].clone();
                    Ok(())
                } else {
                    Err(VersionizeError::Serialize(format!(
                        "Cannot serialize MSRs to target version {}",
                        target_version
                    )))
                }
            }
            _ => Err(VersionizeError::Serialize(
                "Cannot serialize MSRs. The uVM state needs to save
                 more MSRs than the target snapshot version supports."
                    .to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::undocumented_unsafe_blocks)]

    use std::os::unix::io::AsRawFd;

    use kvm_ioctls::Cap;

    use super::*;
    use crate::arch::x86_64::cpu_model::CpuModel;
    use crate::cpu_config::templates::{
        CpuConfiguration, CpuTemplateType, CustomCpuTemplate, GetCpuTemplate, GuestConfigError,
        StaticCpuTemplate,
    };
    use crate::cpu_config::x86_64::cpuid::{Cpuid, CpuidEntry, CpuidKey};
    use crate::vstate::vm::tests::setup_vm;
    use crate::vstate::vm::Vm;

    impl Default for VcpuState {
        fn default() -> Self {
            VcpuState {
                cpuid: CpuId::new(1).unwrap(),
                msrs: Msrs::new(1).unwrap(),
                saved_msrs: vec![Msrs::new(1).unwrap()],
                debug_regs: Default::default(),
                lapic: Default::default(),
                mp_state: Default::default(),
                regs: Default::default(),
                sregs: Default::default(),
                vcpu_events: Default::default(),
                xcrs: Default::default(),
                xsave: Default::default(),
                tsc_khz: Some(0),
            }
        }
    }

    fn setup_vcpu(mem_size: usize) -> (Vm, KvmVcpu, GuestMemoryMmap) {
        let (vm, vm_mem) = setup_vm(mem_size);
        vm.setup_irqchip().unwrap();
        let vcpu = KvmVcpu::new(0, &vm).unwrap();
        (vm, vcpu, vm_mem)
    }

    fn is_at_least_cascade_lake() -> bool {
        CpuModel::get_cpu_model()
            >= (CpuModel {
                extended_family: 0,
                extended_model: 5,
                family: 6,
                model: 5,
                stepping: 7,
            })
    }

    fn create_vcpu_config(
        vm: &Vm,
        vcpu: &KvmVcpu,
        template: &CustomCpuTemplate,
    ) -> std::result::Result<VcpuConfig, GuestConfigError> {
        let cpuid = Cpuid::try_from(vm.supported_cpuid().clone())
            .map_err(GuestConfigError::CpuidFromKvmCpuid)?;
        let msrs = vcpu
            .get_msrs(&template.get_msr_index_list())
            .map_err(GuestConfigError::VcpuIoctl)?;
        let base_cpu_config = CpuConfiguration { cpuid, msrs };
        let cpu_config = CpuConfiguration::apply_template(base_cpu_config, template)?;
        Ok(VcpuConfig {
            vcpu_count: 1,
            smt: false,
            cpu_config,
        })
    }

    #[test]
    fn test_configure_vcpu() {
        let (vm, mut vcpu, vm_mem) = setup_vcpu(0x10000);

        let vcpu_config = create_vcpu_config(&vm, &vcpu, &CustomCpuTemplate::default()).unwrap();
        assert_eq!(
            vcpu.configure(&vm_mem, GuestAddress(0), &vcpu_config,),
            Ok(())
        );

        let try_configure = |vm: &Vm, vcpu: &mut KvmVcpu, template| -> bool {
            let cpu_template = Some(CpuTemplateType::Static(template));
            let template = cpu_template.get_cpu_template();
            match template {
                Ok(template) => match create_vcpu_config(vm, vcpu, &template) {
                    Ok(config) => vcpu
                        .configure(
                            &vm_mem,
                            GuestAddress(crate::arch::get_kernel_start()),
                            &config,
                        )
                        .is_ok(),
                    Err(_) => false,
                },
                Err(_) => false,
            }
        };

        // Test configure while using the T2 template.
        let t2_res = try_configure(&vm, &mut vcpu, StaticCpuTemplate::T2);

        // Test configure while using the C3 template.
        let c3_res = try_configure(&vm, &mut vcpu, StaticCpuTemplate::C3);

        // Test configure while using the T2S template.
        let t2s_res = try_configure(&vm, &mut vcpu, StaticCpuTemplate::T2S);

        // Test configure while using the T2CL template.
        let t2cl_res = try_configure(&vm, &mut vcpu, StaticCpuTemplate::T2CL);

        // Test configure while using the T2S template.
        let t2a_res = try_configure(&vm, &mut vcpu, StaticCpuTemplate::T2A);

        match &cpuid::common::get_vendor_id_from_host().unwrap() {
            cpuid::VENDOR_ID_INTEL => {
                assert!(t2_res);
                assert!(c3_res);
                assert!(t2s_res);
                if is_at_least_cascade_lake() {
                    assert!(t2cl_res);
                } else {
                    assert!(!t2cl_res);
                }
                assert!(!t2a_res);
            }
            cpuid::VENDOR_ID_AMD => {
                assert!(!t2_res);
                assert!(!c3_res);
                assert!(!t2s_res);
                assert!(!t2cl_res);
                assert!(t2a_res);
            }
            _ => {
                assert!(!t2_res);
                assert!(!c3_res);
                assert!(!t2s_res);
                assert!(!t2cl_res);
                assert!(!t2a_res);
            }
        }
    }

    #[test]
    fn test_vcpu_cpuid_restore() {
        let (_vm, vcpu, _) = setup_vcpu(0x1000);
        let mut state = vcpu.save_state().unwrap();
        // Mutate the CPUID.
        //
        // The CPUID obtained with KVM_GET_CPUID2 is empty here, as vcpu configuration (including
        // KVM_SET_CPUID2 call) has not been done yet.
        state.cpuid = CpuId::from_entries(&[kvm_bindings::kvm_cpuid_entry2 {
            function: 0,
            index: 0,
            flags: 0,
            eax: 0x1234_5678,
            ..Default::default()
        }])
        .unwrap();
        assert!(vcpu.restore_state(&state).is_ok());

        unsafe { libc::close(vcpu.fd.as_raw_fd()) };
        let (_vm, vcpu, _) = setup_vcpu(0x1000);
        assert!(vcpu.restore_state(&state).is_ok());

        // Validate the mutated cpuid is saved.
        assert!(vcpu.save_state().unwrap().cpuid.as_slice()[0].eax == 0x1234_5678);
    }

    #[test]
    fn test_empty_cpuid_entries_removed() {
        // Test that `get_cpuid()` removes zeroed empty entries from the `KVM_GET_CPUID2` result.
        let (vm, mut vcpu, vm_mem) = setup_vcpu(0x10000);
        let vcpu_config = VcpuConfig {
            vcpu_count: 1,
            smt: false,
            cpu_config: CpuConfiguration {
                cpuid: Cpuid::try_from(vm.supported_cpuid().clone()).unwrap(),
                msrs: HashMap::new(),
            },
        };
        vcpu.configure(&vm_mem, GuestAddress(0), &vcpu_config)
            .unwrap();

        // Invalid entries filled with 0 should not exist.
        let cpuid = vcpu.get_cpuid().unwrap();
        cpuid.as_slice().iter().for_each(|entry| {
            assert!(
                !(entry.function == 0
                    && entry.index == 0
                    && entry.flags == 0
                    && entry.eax == 0
                    && entry.ebx == 0
                    && entry.ecx == 0
                    && entry.edx == 0)
            );
        });

        // Leaf 0 should have non-zero entry in `Cpuid`.
        let cpuid = Cpuid::try_from(cpuid).unwrap();
        assert_ne!(
            cpuid
                .inner()
                .get(&CpuidKey {
                    leaf: 0,
                    subleaf: 0,
                })
                .unwrap(),
            &CpuidEntry {
                ..Default::default()
            }
        );
    }

    #[test]
    fn test_dump_cpu_config_with_non_configured_vcpu() {
        // Test `dump_cpu_config()` before vcpu configuration.
        //
        // `KVM_GET_CPUID2` returns the result of `KVM_SET_CPUID2`. See
        // https://docs.kernel.org/virt/kvm/api.html#kvm-set-cpuid
        // Since `KVM_SET_CPUID2` has not been called before vcpu configuration, all leaves should
        // be filled with zero. Therefore, `KvmVcpu::dump_cpu_config()` should fail with CPUID type
        // conversion error due to the lack of brand string info in leaf 0x0.
        let (_, vcpu, _) = setup_vcpu(0x10000);
        match vcpu.dump_cpu_config() {
            Err(Error::ConvertCpuidType(_)) => (),
            Err(err) => panic!("Unexpected error: {err}"),
            Ok(_) => panic!("Dumping CPU configuration should fail before vcpu configuration."),
        }
    }

    #[test]
    fn test_dump_cpu_config_with_configured_vcpu() {
        // Test `dump_cpu_config()` after vcpu configuration.
        let (vm, mut vcpu, vm_mem) = setup_vcpu(0x10000);
        let vcpu_config = VcpuConfig {
            vcpu_count: 1,
            smt: false,
            cpu_config: CpuConfiguration {
                cpuid: Cpuid::try_from(vm.supported_cpuid().clone()).unwrap(),
                msrs: HashMap::new(),
            },
        };
        vcpu.configure(&vm_mem, GuestAddress(0), &vcpu_config)
            .unwrap();
        assert!(vcpu.dump_cpu_config().is_ok());
    }

    #[test]
    #[allow(clippy::cast_sign_loss, clippy::redundant_clone)] // always positive, no u32::try_from(f64)
    fn test_is_tsc_scaling_required() {
        // Test `is_tsc_scaling_required` as if it were on the same
        // CPU model as the one in the snapshot state.
        let (_vm, vcpu, _) = setup_vcpu(0x1000);
        let orig_state = vcpu.save_state().unwrap();

        {
            // The frequency difference is within tolerance.
            let mut state = orig_state.clone();
            state.tsc_khz = Some(state.tsc_khz.unwrap() + (TSC_KHZ_TOL / 2.0).round() as u32);
            assert!(!vcpu
                .is_tsc_scaling_required(state.tsc_khz.unwrap())
                .unwrap());
        }

        {
            // The frequency difference is over the tolerance.
            let mut state = orig_state;
            state.tsc_khz = Some(state.tsc_khz.unwrap() + (TSC_KHZ_TOL * 2.0).round() as u32);
            assert!(!vcpu
                .is_tsc_scaling_required(state.tsc_khz.unwrap())
                .unwrap());
        }
    }

    #[test]
    #[allow(clippy::cast_sign_loss)] // always positive, no u32::try_from(f64)
    fn test_set_tsc() {
        let (vm, vcpu, _) = setup_vcpu(0x1000);
        let mut state = vcpu.save_state().unwrap();
        state.tsc_khz = Some(state.tsc_khz.unwrap() + (TSC_KHZ_TOL * 2.0).round() as u32);

        if vm.fd().check_extension(Cap::TscControl) {
            assert!(vcpu.set_tsc_khz(state.tsc_khz.unwrap()).is_ok());
            if vm.fd().check_extension(Cap::GetTscKhz) {
                assert_eq!(vcpu.get_tsc_khz().ok(), state.tsc_khz);
            } else {
                assert!(vcpu.get_tsc_khz().is_err());
            }
        } else {
            assert!(vcpu.set_tsc_khz(state.tsc_khz.unwrap()).is_err());
        }
    }

    #[test]
    fn test_get_msrs_with_msrs_to_save() {
        // Test `get_msrs()` with the MSR indices that should be serialized into snapshots.
        // The MSR indices should be valid and this test should succeed.
        let (_, vcpu, _) = setup_vcpu(0x1000);
        assert!(vcpu
            .get_msrs(&vcpu.msrs_to_save.iter().copied().collect::<Vec<_>>())
            .is_ok());
    }

    #[test]
    fn test_get_msrs_with_msrs_to_dump() {
        // Test `get_msrs()` with the MSR indices that should be dumped.
        // All the MSR indices should be valid and the call should succeed.
        let (_, vcpu, _) = setup_vcpu(0x1000);

        let kvm = kvm_ioctls::Kvm::new().unwrap();
        let msrs_to_dump = crate::arch::x86_64::msr::get_msrs_to_dump(&kvm).unwrap();
        assert!(vcpu.get_msrs(msrs_to_dump.as_slice()).is_ok());
    }

    #[test]
    fn test_get_msrs_with_invalid_msr_index() {
        // Test `get_msrs()` with unsupported MSR indices. This should return
        // `VcpuGetMsrsIncomplete` error that happens when `KVM_GET_MSRS` fails to populdate
        // MSR value in the middle and exits. Currently, MSR indices 2..=4 are not listed as
        // supported MSRs.
        let (_, vcpu, _) = setup_vcpu(0x1000);
        let msr_index_list: Vec<u32> = vec![2, 3, 4];
        match vcpu.get_msrs(&msr_index_list) {
            Err(Error::VcpuGetMsrsIncomplete) => (),
            Err(err) => panic!("Unexpected error: {err}"),
            Ok(_) => panic!(
                "KvmVcpu::get_msrs() for unsupported MSRs should fail with VcpuGetMsrsIncomplete."
            ),
        }
    }
}
