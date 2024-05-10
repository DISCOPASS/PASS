// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::cell::Cell;
use std::sync::atomic::{fence, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
#[cfg(test)]
use std::sync::Mutex;
use std::sync::{Arc, Barrier};
use std::{fmt, io, result, thread};

use kvm_bindings::{KVM_SYSTEM_EVENT_RESET, KVM_SYSTEM_EVENT_SHUTDOWN};
use kvm_ioctls::VcpuExit;
use libc::{c_int, c_void, siginfo_t};
use logger::{error, info, IncMetric, METRICS};
use seccompiler::{BpfProgram, BpfProgramRef};
use utils::errno;
use utils::eventfd::EventFd;
use utils::signal::{register_signal_handler, sigrtmin, Killable};
use utils::sm::StateMachine;

use crate::cpu_config::templates::{CpuConfiguration, GuestConfigError};
use crate::vstate::vm::Vm;
use crate::FcExitCode;

#[cfg(target_arch = "aarch64")]
pub(crate) mod aarch64;
#[cfg(target_arch = "x86_64")]
pub(crate) mod x86_64;

#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::{Error as VcpuError, *};
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::{Error as VcpuError, *};

/// Signal number (SIGRTMIN) used to kick Vcpus.
pub(crate) const VCPU_RTSIG_OFFSET: i32 = 0;

/// Errors associated with the wrappers over KVM ioctls.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Error creating vcpu config.
    #[error("Error creating vcpu config: {0}")]
    VcpuConfig(GuestConfigError),
    /// Error triggered by the KVM subsystem.
    #[error("Received error signaling kvm exit: {0}")]
    FaultyKvmExit(String),
    /// Failed to signal Vcpu.
    #[error("Failed to signal vcpu: {0}")]
    SignalVcpu(utils::errno::Error),
    /// Kvm Exit is not handled by our implementation.
    #[error("Unexpected kvm exit received: {0}")]
    UnhandledKvmExit(String),
    /// Wrapper over error triggered by some vcpu action.
    #[error("Failed to run action on vcpu: {0}")]
    VcpuResponse(VcpuError),
    /// Cannot spawn a new vCPU thread.
    #[error("Cannot spawn a new vCPU thread: {0}")]
    VcpuSpawn(io::Error),
    /// Cannot cleanly initialize vcpu TLS.
    #[error("Cannot clean init vcpu TLS")]
    VcpuTlsInit,
    /// Vcpu not present in TLS.
    #[error("Vcpu not present in TLS")]
    VcpuTlsNotPresent,
}

pub type Result<T> = result::Result<T, Error>;

/// Encapsulates configuration parameters for the guest vCPUS.
#[derive(Debug)]
pub struct VcpuConfig {
    /// Number of guest VCPUs.
    pub vcpu_count: u8,
    /// Enable simultaneous multithreading in the CPUID configuration.
    pub smt: bool,
    /// Configuration for vCPU
    pub cpu_config: CpuConfiguration,
}

// Using this for easier explicit type-casting to help IDEs interpret the code.
type VcpuCell = Cell<Option<*const Vcpu>>;

/// Error type for [`Vcpu::start_threaded`].
#[derive(Debug, derive_more::From)]
pub struct StartThreadedError(std::io::Error);
impl std::error::Error for StartThreadedError {}
impl fmt::Display for StartThreadedError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Failed to spawn vCPU thread: {}", self.0)
    }
}

/// A wrapper around creating and using a vcpu.
pub struct Vcpu {
    /// Access to kvm-arch specific functionality.
    pub kvm_vcpu: KvmVcpu,

    /// File descriptor for vcpu to trigger exit event on vmm.
    exit_evt: EventFd,
    /// The receiving end of events channel owned by the vcpu side.
    event_receiver: Receiver<VcpuEvent>,
    /// The transmitting end of the events channel which will be given to the handler.
    event_sender: Option<Sender<VcpuEvent>>,
    /// The receiving end of the responses channel which will be given to the handler.
    response_receiver: Option<Receiver<VcpuResponse>>,
    /// The transmitting end of the responses channel owned by the vcpu side.
    response_sender: Sender<VcpuResponse>,

    /// Exit reason used to test run_emulation function.
    #[cfg(test)]
    test_vcpu_exit_reason: Mutex<Option<std::result::Result<VcpuExit<'static>, errno::Error>>>,
}

impl Vcpu {
    thread_local!(static TLS_VCPU_PTR: VcpuCell = Cell::new(None));

    /// Associates `self` with the current thread.
    ///
    /// It is a prerequisite to successfully run `init_thread_local_data()` before using
    /// `run_on_thread_local()` on the current thread.
    /// This function will return an error if there already is a `Vcpu` present in the TLS.
    fn init_thread_local_data(&mut self) -> Result<()> {
        Self::TLS_VCPU_PTR.with(|cell: &VcpuCell| {
            if cell.get().is_some() {
                return Err(Error::VcpuTlsInit);
            }
            cell.set(Some(self as *const Vcpu));
            Ok(())
        })
    }

    /// Deassociates `self` from the current thread.
    ///
    /// Should be called if the current `self` had called `init_thread_local_data()` and
    /// now needs to move to a different thread.
    ///
    /// Fails if `self` was not previously associated with the current thread.
    fn reset_thread_local_data(&mut self) -> Result<()> {
        // Best-effort to clean up TLS. If the `Vcpu` was moved to another thread
        // _before_ running this, then there is nothing we can do.
        Self::TLS_VCPU_PTR.with(|cell: &VcpuCell| {
            if let Some(vcpu_ptr) = cell.get() {
                if vcpu_ptr == self as *const Vcpu {
                    Self::TLS_VCPU_PTR.with(|cell: &VcpuCell| cell.take());
                    return Ok(());
                }
            }
            Err(Error::VcpuTlsNotPresent)
        })
    }

    /// Runs `func` for the `Vcpu` associated with the current thread.
    ///
    /// It requires that `init_thread_local_data()` was run on this thread.
    ///
    /// Fails if there is no `Vcpu` associated with the current thread.
    ///
    /// # Safety
    ///
    /// This is marked unsafe as it allows temporary aliasing through
    /// dereferencing from pointer an already borrowed `Vcpu`.
    unsafe fn run_on_thread_local<F>(func: F) -> Result<()>
    where
        F: FnOnce(&Vcpu),
    {
        Self::TLS_VCPU_PTR.with(|cell: &VcpuCell| {
            if let Some(vcpu_ptr) = cell.get() {
                // Dereferencing here is safe since `TLS_VCPU_PTR` is populated/non-empty,
                // and it is being cleared on `Vcpu::drop` so there is no dangling pointer.
                let vcpu_ref: &Vcpu = &*vcpu_ptr;
                func(vcpu_ref);
                Ok(())
            } else {
                Err(Error::VcpuTlsNotPresent)
            }
        })
    }

    /// Registers a signal handler which makes use of TLS and kvm immediate exit to
    /// kick the vcpu running on the current thread, if there is one.
    pub fn register_kick_signal_handler() {
        extern "C" fn handle_signal(_: c_int, _: *mut siginfo_t, _: *mut c_void) {
            // SAFETY: This is safe because it's temporarily aliasing the `Vcpu` object, but we are
            // only reading `vcpu.fd` which does not change for the lifetime of the `Vcpu`.
            unsafe {
                let _ = Vcpu::run_on_thread_local(|vcpu| {
                    vcpu.kvm_vcpu.fd.set_kvm_immediate_exit(1);
                    fence(Ordering::Release);
                });
            }
        }

        register_signal_handler(sigrtmin() + VCPU_RTSIG_OFFSET, handle_signal)
            .expect("Failed to register vcpu signal handler");
    }

    /// Constructs a new VCPU for `vm`.
    ///
    /// # Arguments
    ///
    /// * `index` - Represents the 0-based CPU index between [0, max vcpus).
    /// * `vm` - The vm to which this vcpu will get attached.
    /// * `exit_evt` - An `EventFd` that will be written into when this vcpu exits.
    pub fn new(index: u8, vm: &Vm, exit_evt: EventFd) -> Result<Self> {
        let (event_sender, event_receiver) = channel();
        let (response_sender, response_receiver) = channel();
        let kvm_vcpu = KvmVcpu::new(index, vm).unwrap();

        Ok(Vcpu {
            exit_evt,
            event_receiver,
            event_sender: Some(event_sender),
            response_receiver: Some(response_receiver),
            response_sender,
            kvm_vcpu,
            #[cfg(test)]
            test_vcpu_exit_reason: Mutex::new(None),
        })
    }

    /// Sets a MMIO bus for this vcpu.
    pub fn set_mmio_bus(&mut self, mmio_bus: crate::devices::Bus) {
        self.kvm_vcpu.mmio_bus = Some(mmio_bus);
    }

    /// Moves the vcpu to its own thread and constructs a VcpuHandle.
    /// The handle can be used to control the remote vcpu.
    pub fn start_threaded(
        mut self,
        seccomp_filter: Arc<BpfProgram>,
        barrier: Arc<Barrier>,
    ) -> std::result::Result<VcpuHandle, StartThreadedError> {
        let event_sender = self.event_sender.take().expect("vCPU already started");
        let response_receiver = self.response_receiver.take().unwrap();
        let vcpu_thread = thread::Builder::new()
            .name(format!("fc_vcpu {}", self.kvm_vcpu.index))
            .spawn(move || {
                let filter = &*seccomp_filter;
                self.init_thread_local_data()
                    .expect("Cannot cleanly initialize vcpu TLS.");
                // Synchronization to make sure thread local data is initialized.
                barrier.wait();
                self.run(filter);
            })?;

        Ok(VcpuHandle::new(
            event_sender,
            response_receiver,
            vcpu_thread,
        ))
    }

    /// Main loop of the vCPU thread.
    ///
    /// Runs the vCPU in KVM context in a loop. Handles KVM_EXITs then goes back in.
    /// Note that the state of the VCPU and associated VM must be setup first for this to do
    /// anything useful.
    pub fn run(&mut self, seccomp_filter: BpfProgramRef) {
        // Load seccomp filters for this vCPU thread.
        // Execution panics if filters cannot be loaded, use --no-seccomp if skipping filters
        // altogether is the desired behaviour.
        if let Err(err) = seccompiler::apply_filter(seccomp_filter) {
            panic!(
                "Failed to set the requested seccomp filters on vCPU {}: Error: {}",
                self.kvm_vcpu.index, err
            );
        }

        // Start running the machine state in the `Paused` state.
        StateMachine::run(self, Self::paused);
    }

    // This is the main loop of the `Running` state.
    fn running(&mut self) -> StateMachine<Self> {
        // This loop is here just for optimizing the emulation path.
        // No point in ticking the state machine if there are no external events.
        loop {
            match self.run_emulation() {
                // Emulation ran successfully, continue.
                Ok(VcpuEmulation::Handled) => (),
                // Emulation was interrupted, check external events.
                Ok(VcpuEmulation::Interrupted) => break,
                // If the guest was rebooted or halted:
                // - vCPU0 will always exit out of `KVM_RUN` with KVM_EXIT_SHUTDOWN or KVM_EXIT_HLT.
                // - the other vCPUs won't ever exit out of `KVM_RUN`, but they won't consume CPU.
                // So we pause vCPU0 and send a signal to the emulation thread to stop the VMM.
                Ok(VcpuEmulation::Stopped) => return self.exit(FcExitCode::Ok),
                // Emulation errors lead to vCPU exit.
                Err(_) => return self.exit(FcExitCode::GenericError),
            }
        }

        // By default don't change state.
        let mut state = StateMachine::next(Self::running);

        // Break this emulation loop on any transition request/external event.
        match self.event_receiver.try_recv() {
            // Running ---- Pause ----> Paused
            Ok(VcpuEvent::Pause) => {
                // Nothing special to do.
                self.response_sender
                    .send(VcpuResponse::Paused)
                    .expect("failed to send pause status");

                // TODO: we should call `KVM_KVMCLOCK_CTRL` here to make sure
                // TODO continued: the guest soft lockup watchdog does not panic on Resume.

                // Move to 'paused' state.
                state = StateMachine::next(Self::paused);
            }
            Ok(VcpuEvent::Resume) => {
                self.response_sender
                    .send(VcpuResponse::Resumed)
                    .expect("failed to send resume status");
            }
            // SaveState or RestoreState cannot be performed on a running Vcpu.
            Ok(VcpuEvent::SaveState) | Ok(VcpuEvent::RestoreState(_)) => {
                self.response_sender
                    .send(VcpuResponse::NotAllowed(String::from(
                        "save/restore unavailable while running",
                    )))
                    .expect("failed to send save not allowed status");
            }
            // DumpCpuConfig cannot be performed on a running Vcpu.
            Ok(VcpuEvent::DumpCpuConfig) => {
                self.response_sender
                    .send(VcpuResponse::NotAllowed(String::from(
                        "cpu config dump is unavailable while running",
                    )))
                    .expect("failed to send save not allowed status");
            }
            Ok(VcpuEvent::Finish) => return StateMachine::finish(),
            // Unhandled exit of the other end.
            Err(TryRecvError::Disconnected) => {
                // Move to 'exited' state.
                state = self.exit(FcExitCode::GenericError);
            }
            // All other events or lack thereof have no effect on current 'running' state.
            Err(TryRecvError::Empty) => (),
        }

        state
    }

    // This is the main loop of the `Paused` state.
    fn paused(&mut self) -> StateMachine<Self> {
        match self.event_receiver.recv() {
            // Paused ---- Resume ----> Running
            Ok(VcpuEvent::Resume) => {
                // Nothing special to do.
                self.response_sender
                    .send(VcpuResponse::Resumed)
                    .expect("vcpu channel unexpectedly closed");
                // Move to 'running' state.
                StateMachine::next(Self::running)
            }
            Ok(VcpuEvent::Pause) => {
                self.response_sender
                    .send(VcpuResponse::Paused)
                    .expect("vcpu channel unexpectedly closed");
                StateMachine::next(Self::paused)
            }
            Ok(VcpuEvent::SaveState) => {
                // Save vcpu state.
                self.kvm_vcpu
                    .save_state()
                    .map(|vcpu_state| {
                        self.response_sender
                            .send(VcpuResponse::SavedState(Box::new(vcpu_state)))
                            .expect("vcpu channel unexpectedly closed");
                    })
                    .unwrap_or_else(|err| {
                        self.response_sender
                            .send(VcpuResponse::Error(Error::VcpuResponse(err)))
                            .expect("vcpu channel unexpectedly closed");
                    });

                StateMachine::next(Self::paused)
            }
            Ok(VcpuEvent::RestoreState(vcpu_state)) => {
                self.kvm_vcpu
                    .restore_state(&vcpu_state)
                    .map(|()| {
                        self.response_sender
                            .send(VcpuResponse::RestoredState)
                            .expect("vcpu channel unexpectedly closed");
                    })
                    .unwrap_or_else(|err| {
                        self.response_sender
                            .send(VcpuResponse::Error(Error::VcpuResponse(err)))
                            .expect("vcpu channel unexpectedly closed")
                    });

                StateMachine::next(Self::paused)
            }
            Ok(VcpuEvent::DumpCpuConfig) => {
                self.kvm_vcpu
                    .dump_cpu_config()
                    .map(|cpu_config| {
                        self.response_sender
                            .send(VcpuResponse::DumpedCpuConfig(Box::new(cpu_config)))
                            .expect("vcpu channel unexpectedly closed");
                    })
                    .unwrap_or_else(|err| {
                        self.response_sender
                            .send(VcpuResponse::Error(Error::VcpuResponse(err)))
                            .expect("vcpu channel unnexpectedly closed");
                    });

                StateMachine::next(Self::paused)
            }
            Ok(VcpuEvent::Finish) => StateMachine::finish(),
            // Unhandled exit of the other end.
            Err(_) => {
                // Move to 'exited' state.
                self.exit(FcExitCode::GenericError)
            }
        }
    }

    // Transition to the exited state and finish on command.
    fn exit(&mut self, exit_code: FcExitCode) -> StateMachine<Self> {
        // To avoid cycles, all teardown paths take the following route:
        // +------------------------+----------------------------+------------------------+
        // |        Vmm             |           Action           |           Vcpu         |
        // +------------------------+----------------------------+------------------------+
        // 1 |                        |                            | vcpu.exit(exit_code)   |
        // 2 |                        |                            | vcpu.exit_evt.write(1) |
        // 3 |                        | <--- EventFd::exit_evt --- |                        |
        // 4 | vmm.stop()             |                            |                        |
        // 5 |                        | --- VcpuEvent::Finish ---> |                        |
        // 6 |                        |                            | StateMachine::finish() |
        // 7 | VcpuHandle::join()     |                            |                        |
        // 8 | vmm.shutdown_exit_code becomes Some(exit_code) breaking the main event loop  |
        // +------------------------+----------------------------+------------------------+
        // Vcpu initiated teardown starts from `fn Vcpu::exit()` (step 1).
        // Vmm initiated teardown starts from `pub fn Vmm::stop()` (step 4).
        // Once `vmm.shutdown_exit_code` becomes `Some(exit_code)`, it is the upper layer's
        // responsibility to break main event loop and propagate the exit code value.
        // Signal Vmm of Vcpu exit.
        if let Err(err) = self.exit_evt.write(1) {
            METRICS.vcpu.failures.inc();
            error!("Failed signaling vcpu exit event: {}", err);
        }
        // From this state we only accept going to finished.
        loop {
            self.response_sender
                .send(VcpuResponse::Exited(exit_code))
                .expect("vcpu channel unexpectedly closed");
            // Wait for and only accept 'VcpuEvent::Finish'.
            if let Ok(VcpuEvent::Finish) = self.event_receiver.recv() {
                break;
            }
        }
        StateMachine::finish()
    }

    #[cfg(not(test))]
    /// Calls `KVM_RUN` with this [`Vcpu`]'s underlying file descriptor.
    ///
    /// Blocks until a `VM_EXIT` is received, in which case this function returns a [`VcpuExit`]
    /// containing the reason.
    pub fn emulate(&self) -> std::result::Result<VcpuExit, errno::Error> {
        self.kvm_vcpu.fd.run()
    }

    /// Runs the vCPU in KVM context and handles the kvm exit reason.
    ///
    /// Returns error or enum specifying whether emulation was handled or interrupted.
    pub fn run_emulation(&self) -> Result<VcpuEmulation> {
        match self.emulate() {
            Ok(run) => match run {
                VcpuExit::MmioRead(addr, data) => {
                    if let Some(mmio_bus) = &self.kvm_vcpu.mmio_bus {
                        mmio_bus.read(addr, data);
                        METRICS.vcpu.exit_mmio_read.inc();
                    }
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::MmioWrite(addr, data) => {
                    if let Some(mmio_bus) = &self.kvm_vcpu.mmio_bus {
                        mmio_bus.write(addr, data);
                        METRICS.vcpu.exit_mmio_write.inc();
                    }
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::Hlt => {
                    info!("Received KVM_EXIT_HLT signal");
                    Ok(VcpuEmulation::Stopped)
                }
                VcpuExit::Shutdown => {
                    info!("Received KVM_EXIT_SHUTDOWN signal");
                    Ok(VcpuEmulation::Stopped)
                }
                // Documentation specifies that below kvm exits are considered
                // errors.
                VcpuExit::FailEntry(hardware_entry_failure_reason, cpu) => {
                    // Hardware entry failure.
                    METRICS.vcpu.failures.inc();
                    error!(
                        "Received KVM_EXIT_FAIL_ENTRY signal: {} on cpu {}",
                        hardware_entry_failure_reason, cpu
                    );
                    Err(Error::FaultyKvmExit(format!(
                        "{:?}",
                        VcpuExit::FailEntry(hardware_entry_failure_reason, cpu)
                    )))
                }
                VcpuExit::InternalError => {
                    // Failure from the Linux KVM subsystem rather than from the hardware.
                    METRICS.vcpu.failures.inc();
                    error!("Received KVM_EXIT_INTERNAL_ERROR signal");
                    Err(Error::FaultyKvmExit(format!(
                        "{:?}",
                        VcpuExit::InternalError
                    )))
                }
                VcpuExit::SystemEvent(event_type, event_flags) => match event_type {
                    KVM_SYSTEM_EVENT_RESET | KVM_SYSTEM_EVENT_SHUTDOWN => {
                        info!(
                            "Received KVM_SYSTEM_EVENT: type: {}, event: {}",
                            event_type, event_flags
                        );
                        Ok(VcpuEmulation::Stopped)
                    }
                    _ => {
                        METRICS.vcpu.failures.inc();
                        error!(
                            "Received KVM_SYSTEM_EVENT signal type: {}, flag: {}",
                            event_type, event_flags
                        );
                        Err(Error::FaultyKvmExit(format!(
                            "{:?}",
                            VcpuExit::SystemEvent(event_type, event_flags)
                        )))
                    }
                },
                arch_specific_reason => {
                    // run specific architecture emulation.
                    self.kvm_vcpu.run_arch_emulation(arch_specific_reason)
                }
            },
            // The unwrap on raw_os_error can only fail if we have a logic
            // error in our code in which case it is better to panic.
            Err(ref err) => {
                match err.errno() {
                    libc::EAGAIN => Ok(VcpuEmulation::Handled),
                    libc::EINTR => {
                        self.kvm_vcpu.fd.set_kvm_immediate_exit(0);
                        // Notify that this KVM_RUN was interrupted.
                        Ok(VcpuEmulation::Interrupted)
                    }
                    libc::ENOSYS => {
                        METRICS.vcpu.failures.inc();
                        error!(
                            "Received ENOSYS error because KVM failed to emulate an instruction."
                        );
                        Err(Error::FaultyKvmExit(
                            "Received ENOSYS error because KVM failed to emulate an instruction."
                                .to_string(),
                        ))
                    }
                    _ => {
                        METRICS.vcpu.failures.inc();
                        error!("Failure during vcpu run: {}", err);
                        Err(Error::FaultyKvmExit(format!("{}", err)))
                    }
                }
            }
        }
    }
}

impl Drop for Vcpu {
    fn drop(&mut self) {
        let _ = self.reset_thread_local_data();
    }
}

#[derive(Clone)]
/// List of events that the Vcpu can receive.
pub enum VcpuEvent {
    /// The vCPU thread will end when receiving this message.
    Finish,
    /// Pause the Vcpu.
    Pause,
    /// Event to resume the Vcpu.
    Resume,
    /// Event to restore the state of a paused Vcpu.
    RestoreState(Box<VcpuState>),
    /// Event to save the state of a paused Vcpu.
    SaveState,
    /// Event to dump CPU configuration of a paused Vcpu.
    DumpCpuConfig,
}

/// List of responses that the Vcpu reports.
pub enum VcpuResponse {
    /// Requested action encountered an error.
    Error(Error),
    /// Vcpu is stopped.
    Exited(FcExitCode),
    /// Requested action not allowed.
    NotAllowed(String),
    /// Vcpu is paused.
    Paused,
    /// Vcpu is resumed.
    Resumed,
    /// Vcpu state is restored.
    RestoredState,
    /// Vcpu state is saved.
    SavedState(Box<VcpuState>),
    /// Vcpu is in the state where CPU config is dumped.
    DumpedCpuConfig(Box<CpuConfiguration>),
}

impl fmt::Debug for VcpuResponse {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use crate::VcpuResponse::*;
        match self {
            Paused => write!(f, "VcpuResponse::Paused"),
            Resumed => write!(f, "VcpuResponse::Resumed"),
            Exited(code) => write!(f, "VcpuResponse::Exited({:?})", code),
            RestoredState => write!(f, "VcpuResponse::RestoredState"),
            SavedState(_) => write!(f, "VcpuResponse::SavedState"),
            Error(ref err) => write!(f, "VcpuResponse::Error({:?})", err),
            NotAllowed(ref reason) => write!(f, "VcpuResponse::NotAllowed({})", reason),
            DumpedCpuConfig(_) => write!(f, "VcpuResponse::DumpedCpuConfig"),
        }
    }
}

/// Wrapper over Vcpu that hides the underlying interactions with the Vcpu thread.
pub struct VcpuHandle {
    event_sender: Sender<VcpuEvent>,
    response_receiver: Receiver<VcpuResponse>,
    // Rust JoinHandles have to be wrapped in Option if you ever plan on 'join()'ing them.
    // We want to be able to join these threads in tests.
    vcpu_thread: Option<thread::JoinHandle<()>>,
}

/// Error type for [`VcpuHandle::send_event`].
#[derive(Debug, derive_more::From)]
pub struct VcpuSendEventError(pub utils::errno::Error);
impl std::error::Error for VcpuSendEventError {}
impl fmt::Display for VcpuSendEventError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Failed to signal vCPU: {}", self.0)
    }
}

impl VcpuHandle {
    /// Creates a new [`VcpuHandle`].
    ///
    /// # Arguments
    /// + `event_sender`: [`Sender`] to communicate [`VcpuEvent`] to control the vcpu.
    /// + `response_received`: [`Received`] from which the vcpu's responses can be read.
    /// + `vcpu_thread`: A [`JoinHandle`] for the vcpu thread.
    pub fn new(
        event_sender: Sender<VcpuEvent>,
        response_receiver: Receiver<VcpuResponse>,
        vcpu_thread: thread::JoinHandle<()>,
    ) -> Self {
        Self {
            event_sender,
            response_receiver,
            vcpu_thread: Some(vcpu_thread),
        }
    }
    /// Sends event to vCPU.
    ///
    /// # Errors
    ///
    /// When [`vmm_sys_util::linux::signal::Killable::kill`] errors.
    pub fn send_event(&self, event: VcpuEvent) -> std::result::Result<(), VcpuSendEventError> {
        // Use expect() to crash if the other thread closed this channel.
        self.event_sender
            .send(event)
            .expect("event sender channel closed on vcpu end.");
        // Kick the vcpu so it picks up the message.
        self.vcpu_thread
            .as_ref()
            // Safe to unwrap since constructor make this 'Some'.
            .unwrap()
            .kill(sigrtmin() + VCPU_RTSIG_OFFSET)?;
        Ok(())
    }

    /// Returns a reference to the [`Received`] from which the vcpu's responses can be read.
    pub fn response_receiver(&self) -> &Receiver<VcpuResponse> {
        &self.response_receiver
    }
}

// Wait for the Vcpu thread to finish execution
impl Drop for VcpuHandle {
    fn drop(&mut self) {
        // We assume that by the time a VcpuHandle is dropped, other code has run to
        // get the state machine loop to finish so the thread is ready to join.
        // The strategy of avoiding more complex messaging protocols during the Drop
        // helps avoid cycles which were preventing a truly clean shutdown.
        //
        // If the code hangs at this point, that means that a Finish event was not
        // sent by Vmm.
        self.vcpu_thread.take().unwrap().join().unwrap();
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum VcpuEmulation {
    Handled,
    Interrupted,
    Stopped,
}

#[cfg(test)]
pub mod tests {
    #![allow(clippy::undocumented_unsafe_blocks)]

    use std::sync::{Arc, Barrier, Mutex};

    use linux_loader::loader::KernelLoader;
    use utils::errno;
    use utils::signal::validate_signal_num;
    use utils::vm_memory::{GuestAddress, GuestMemoryMmap};

    use super::*;
    use crate::builder::StartMicrovmError;
    use crate::seccomp_filters::{get_filters, SeccompConfig};
    use crate::vstate::vcpu::Error as EmulationError;
    use crate::vstate::vm::tests::setup_vm;
    use crate::vstate::vm::Vm;
    use crate::RECV_TIMEOUT_SEC;

    struct DummyDevice;
    impl crate::devices::BusDevice for DummyDevice {}

    impl Vcpu {
        pub fn emulate(&self) -> std::result::Result<VcpuExit, errno::Error> {
            self.test_vcpu_exit_reason
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| Err(errno::Error::new(libc::SIGILL)))
        }
    }

    #[test]
    fn test_run_emulation() {
        let (_vm, mut vcpu, _vm_mem) = setup_vcpu(0x1000);
        vcpu.test_vcpu_exit_reason = Mutex::new(Some(Ok(VcpuExit::Hlt)));
        let res = vcpu.run_emulation();
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), VcpuEmulation::Stopped);

        *(vcpu.test_vcpu_exit_reason.lock().unwrap()) = Some(Ok(VcpuExit::Shutdown));
        let res = vcpu.run_emulation();
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), VcpuEmulation::Stopped);

        *(vcpu.test_vcpu_exit_reason.lock().unwrap()) = Some(Ok(VcpuExit::FailEntry(0, 0)));
        let res = vcpu.run_emulation();
        assert!(res.is_err());
        assert_eq!(
            format!("{:?}", res.unwrap_err()),
            format!(
                "{:?}",
                EmulationError::FaultyKvmExit("FailEntry(0, 0)".to_string())
            )
        );

        *(vcpu.test_vcpu_exit_reason.lock().unwrap()) = Some(Ok(VcpuExit::InternalError));
        let res = vcpu.run_emulation();
        assert!(res.is_err());
        assert_eq!(
            format!("{:?}", res.unwrap_err()),
            format!(
                "{:?}",
                EmulationError::FaultyKvmExit("InternalError".to_string())
            )
        );

        *(vcpu.test_vcpu_exit_reason.lock().unwrap()) = Some(Ok(VcpuExit::SystemEvent(2, 0)));
        let res = vcpu.run_emulation();
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), VcpuEmulation::Stopped);

        *(vcpu.test_vcpu_exit_reason.lock().unwrap()) = Some(Ok(VcpuExit::SystemEvent(1, 0)));
        let res = vcpu.run_emulation();
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), VcpuEmulation::Stopped);

        *(vcpu.test_vcpu_exit_reason.lock().unwrap()) = Some(Ok(VcpuExit::SystemEvent(3, 0)));
        let res = vcpu.run_emulation();
        assert!(res.is_err());
        assert_eq!(
            format!("{:?}", res.unwrap_err()),
            format!(
                "{:?}",
                EmulationError::FaultyKvmExit("SystemEvent(3, 0)".to_string())
            )
        );

        // Check what happens with an unhandled exit reason.
        *(vcpu.test_vcpu_exit_reason.lock().unwrap()) = Some(Ok(VcpuExit::Unknown));
        let res = vcpu.run_emulation();
        assert!(res.is_err());
        assert_eq!(
            res.err().unwrap().to_string(),
            "Unexpected kvm exit received: Unknown".to_string()
        );

        *(vcpu.test_vcpu_exit_reason.lock().unwrap()) = Some(Err(errno::Error::new(libc::EAGAIN)));
        let res = vcpu.run_emulation();
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), VcpuEmulation::Handled);

        *(vcpu.test_vcpu_exit_reason.lock().unwrap()) = Some(Err(errno::Error::new(libc::ENOSYS)));
        let res = vcpu.run_emulation();
        assert!(res.is_err());
        assert_eq!(
            format!("{:?}", res.unwrap_err()),
            format!(
                "{:?}",
                EmulationError::FaultyKvmExit(
                    "Received ENOSYS error because KVM failed to emulate an instruction."
                        .to_string()
                )
            )
        );

        *(vcpu.test_vcpu_exit_reason.lock().unwrap()) = Some(Err(errno::Error::new(libc::EINTR)));
        let res = vcpu.run_emulation();
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), VcpuEmulation::Interrupted);

        *(vcpu.test_vcpu_exit_reason.lock().unwrap()) = Some(Err(errno::Error::new(libc::EINVAL)));
        let res = vcpu.run_emulation();
        assert!(res.is_err());
        assert_eq!(
            format!("{:?}", res.unwrap_err()),
            format!(
                "{:?}",
                EmulationError::FaultyKvmExit("Invalid argument (os error 22)".to_string())
            )
        );

        let mut bus = crate::devices::Bus::new();
        let dummy = Arc::new(Mutex::new(DummyDevice));
        bus.insert(dummy, 0x10, 0x10).unwrap();
        vcpu.set_mmio_bus(bus);
        let addr = 0x10;
        static mut DATA: [u8; 4] = [0, 0, 0, 0];

        unsafe {
            *(vcpu.test_vcpu_exit_reason.lock().unwrap()) =
                Some(Ok(VcpuExit::MmioRead(addr, &mut DATA)));
        }
        let res = vcpu.run_emulation();
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), VcpuEmulation::Handled);

        unsafe {
            *(vcpu.test_vcpu_exit_reason.lock().unwrap()) =
                Some(Ok(VcpuExit::MmioWrite(addr, &DATA)));
        }
        let res = vcpu.run_emulation();
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), VcpuEmulation::Handled);
    }

    impl PartialEq for VcpuResponse {
        fn eq(&self, other: &Self) -> bool {
            use crate::VcpuResponse::*;
            // Guard match with no wildcard to make sure we catch new enum variants.
            match self {
                Paused | Resumed | Exited(_) => (),
                Error(_) | NotAllowed(_) | RestoredState | SavedState(_) | DumpedCpuConfig(_) => (),
            };
            match (self, other) {
                (Paused, Paused) | (Resumed, Resumed) => true,
                (Exited(code), Exited(other_code)) => code == other_code,
                (NotAllowed(_), NotAllowed(_))
                | (RestoredState, RestoredState)
                | (SavedState(_), SavedState(_))
                | (DumpedCpuConfig(_), DumpedCpuConfig(_)) => true,
                (Error(ref err), Error(ref other_err)) => {
                    format!("{:?}", err) == format!("{:?}", other_err)
                }
                _ => false,
            }
        }
    }

    // Auxiliary function being used throughout the tests.
    #[allow(unused_mut)]
    pub(crate) fn setup_vcpu(mem_size: usize) -> (Vm, Vcpu, GuestMemoryMmap) {
        let (mut vm, gm) = setup_vm(mem_size);

        let exit_evt = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let vcpu;
        #[cfg(target_arch = "aarch64")]
        {
            vcpu = Vcpu::new(1, &vm, exit_evt).unwrap();
            vcpu.kvm_vcpu.init(vm.fd()).unwrap();
            vm.setup_irqchip(1).unwrap();
        }
        #[cfg(target_arch = "x86_64")]
        {
            vm.setup_irqchip().unwrap();
            vcpu = Vcpu::new(1, &vm, exit_evt).unwrap();
        }
        (vm, vcpu, gm)
    }

    fn load_good_kernel(vm_memory: &GuestMemoryMmap) -> GuestAddress {
        use std::fs::File;
        use std::path::PathBuf;

        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

        #[cfg(target_arch = "x86_64")]
        path.push("src/utilities/mock_resources/test_elf.bin");
        #[cfg(target_arch = "aarch64")]
        path.push("src/utilities/mock_resources/test_pe.bin");

        let mut kernel_file = File::open(path).expect("Cannot open kernel file");

        #[cfg(target_arch = "x86_64")]
        let entry_addr = linux_loader::loader::elf::Elf::load(
            vm_memory,
            Some(GuestAddress(crate::arch::get_kernel_start())),
            &mut kernel_file,
            Some(GuestAddress(crate::arch::get_kernel_start())),
        )
        .map_err(StartMicrovmError::KernelLoader);
        #[cfg(target_arch = "aarch64")]
        let entry_addr =
            linux_loader::loader::pe::PE::load(vm_memory, None, &mut kernel_file, None)
                .map_err(StartMicrovmError::KernelLoader);
        entry_addr.unwrap().kernel_load
    }

    fn vcpu_configured_for_boot() -> (VcpuHandle, utils::eventfd::EventFd) {
        Vcpu::register_kick_signal_handler();
        // Need enough mem to boot linux.
        let mem_size = 64 << 20;
        let (_vm, mut vcpu, vm_mem) = setup_vcpu(mem_size);

        let vcpu_exit_evt = vcpu.exit_evt.try_clone().unwrap();

        // Needs a kernel since we'll actually run this vcpu.
        let entry_addr = load_good_kernel(&vm_mem);

        #[cfg(target_arch = "x86_64")]
        {
            use crate::cpu_config::x86_64::cpuid::Cpuid;
            vcpu.kvm_vcpu
                .configure(
                    &vm_mem,
                    entry_addr,
                    &VcpuConfig {
                        vcpu_count: 1,
                        smt: false,
                        cpu_config: CpuConfiguration {
                            cpuid: Cpuid::try_from(_vm.supported_cpuid().clone()).unwrap(),
                            msrs: std::collections::HashMap::new(),
                        },
                    },
                )
                .expect("failed to configure vcpu");
        }

        #[cfg(target_arch = "aarch64")]
        vcpu.kvm_vcpu
            .configure(
                &vm_mem,
                entry_addr,
                &VcpuConfig {
                    vcpu_count: 1,
                    smt: false,
                    cpu_config: crate::cpu_config::aarch64::CpuConfiguration::default(),
                },
            )
            .expect("failed to configure vcpu");

        let mut seccomp_filters = get_filters(SeccompConfig::None).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let vcpu_handle = vcpu
            .start_threaded(seccomp_filters.remove("vcpu").unwrap(), barrier.clone())
            .expect("failed to start vcpu");
        // Wait for vCPUs to initialize their TLS before moving forward.
        barrier.wait();

        (vcpu_handle, vcpu_exit_evt)
    }

    #[test]
    fn test_set_mmio_bus() {
        let (_, mut vcpu, _) = setup_vcpu(0x1000);
        assert!(vcpu.kvm_vcpu.mmio_bus.is_none());
        vcpu.set_mmio_bus(crate::devices::Bus::new());
        assert!(vcpu.kvm_vcpu.mmio_bus.is_some());
    }

    #[test]
    fn test_vcpu_tls() {
        let (_, mut vcpu, _) = setup_vcpu(0x1000);

        // Running on the TLS vcpu should fail before we actually initialize it.
        unsafe {
            assert!(Vcpu::run_on_thread_local(|_| ()).is_err());
        }

        // Initialize vcpu TLS.
        vcpu.init_thread_local_data().unwrap();

        // Validate TLS vcpu is the local vcpu by changing the `id` then validating against
        // the one in TLS.
        vcpu.kvm_vcpu.index = 12;
        unsafe {
            assert!(Vcpu::run_on_thread_local(|v| assert_eq!(v.kvm_vcpu.index, 12)).is_ok());
        }

        // Reset vcpu TLS.
        assert!(vcpu.reset_thread_local_data().is_ok());

        // Running on the TLS vcpu after TLS reset should fail.
        unsafe {
            assert!(Vcpu::run_on_thread_local(|_| ()).is_err());
        }

        // Second reset should return error.
        assert!(vcpu.reset_thread_local_data().is_err());
    }

    #[test]
    fn test_invalid_tls() {
        let (_, mut vcpu, _) = setup_vcpu(0x1000);
        // Initialize vcpu TLS.
        vcpu.init_thread_local_data().unwrap();
        // Trying to initialize non-empty TLS should error.
        vcpu.init_thread_local_data().unwrap_err();
    }

    #[test]
    fn test_vcpu_kick() {
        Vcpu::register_kick_signal_handler();
        let (vm, mut vcpu, _) = setup_vcpu(0x1000);

        let kvm_run =
            kvm_ioctls::KvmRunWrapper::mmap_from_fd(&vcpu.kvm_vcpu.fd, vm.fd().run_size())
                .expect("cannot mmap kvm-run");
        let success = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let vcpu_success = success.clone();
        let barrier = Arc::new(Barrier::new(2));
        let vcpu_barrier = barrier.clone();
        // Start Vcpu thread which will be kicked with a signal.
        let handle = std::thread::Builder::new()
            .name("test_vcpu_kick".to_string())
            .spawn(move || {
                vcpu.init_thread_local_data().unwrap();
                // Notify TLS was populated.
                vcpu_barrier.wait();
                // Loop for max 1 second to check if the signal handler has run.
                for _ in 0..10 {
                    if kvm_run.as_mut_ref().immediate_exit == 1 {
                        // Signal handler has run and set immediate_exit to 1.
                        vcpu_success.store(true, Ordering::Release);
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            })
            .expect("cannot start thread");

        // Wait for the vcpu to initialize its TLS.
        barrier.wait();
        // Kick the Vcpu using the custom signal.
        handle
            .kill(sigrtmin() + VCPU_RTSIG_OFFSET)
            .expect("failed to signal thread");
        handle.join().expect("failed to join thread");
        // Verify that the Vcpu saw its kvm immediate-exit as set.
        assert!(success.load(Ordering::Acquire));
    }

    // Sends an event to a vcpu and expects a particular response.
    fn queue_event_expect_response(handle: &VcpuHandle, event: VcpuEvent, response: VcpuResponse) {
        handle
            .send_event(event)
            .expect("failed to send event to vcpu");
        assert_eq!(
            handle
                .response_receiver()
                .recv_timeout(RECV_TIMEOUT_SEC)
                .expect("did not receive event response from vcpu"),
            response
        );
    }

    #[test]
    fn test_vcpu_pause_resume() {
        let (vcpu_handle, vcpu_exit_evt) = vcpu_configured_for_boot();

        // Queue a Resume event, expect a response.
        queue_event_expect_response(&vcpu_handle, VcpuEvent::Resume, VcpuResponse::Resumed);

        // Queue a Pause event, expect a response.
        queue_event_expect_response(&vcpu_handle, VcpuEvent::Pause, VcpuResponse::Paused);

        // Validate vcpu handled the EINTR gracefully and didn't exit.
        let err = vcpu_exit_evt.read().unwrap_err();
        assert_eq!(err.raw_os_error().unwrap(), libc::EAGAIN);

        // Queue another Pause event, expect a response.
        queue_event_expect_response(&vcpu_handle, VcpuEvent::Pause, VcpuResponse::Paused);

        // Queue a Resume event, expect a response.
        queue_event_expect_response(&vcpu_handle, VcpuEvent::Resume, VcpuResponse::Resumed);

        // Queue another Resume event, expect a response.
        queue_event_expect_response(&vcpu_handle, VcpuEvent::Resume, VcpuResponse::Resumed);

        // Queue another Pause event, expect a response.
        queue_event_expect_response(&vcpu_handle, VcpuEvent::Pause, VcpuResponse::Paused);

        // Queue a Resume event, expect a response.
        queue_event_expect_response(&vcpu_handle, VcpuEvent::Resume, VcpuResponse::Resumed);

        vcpu_handle.send_event(VcpuEvent::Finish).unwrap();
    }

    #[test]
    fn test_vcpu_save_restore_state_events() {
        let (vcpu_handle, _vcpu_exit_evt) = vcpu_configured_for_boot();

        // Queue a Resume event, expect a response.
        queue_event_expect_response(&vcpu_handle, VcpuEvent::Resume, VcpuResponse::Resumed);

        // Queue a SaveState event, expect a response.
        queue_event_expect_response(
            &vcpu_handle,
            VcpuEvent::SaveState,
            VcpuResponse::NotAllowed(String::new()),
        );

        // Queue another Pause event, expect a response.
        queue_event_expect_response(&vcpu_handle, VcpuEvent::Pause, VcpuResponse::Paused);

        // Queue a SaveState event, get the response.
        vcpu_handle
            .send_event(VcpuEvent::SaveState)
            .expect("failed to send event to vcpu");
        let vcpu_state = match vcpu_handle
            .response_receiver()
            .recv_timeout(RECV_TIMEOUT_SEC)
            .expect("did not receive event response from vcpu")
        {
            VcpuResponse::SavedState(state) => state,
            _ => panic!("unexpected response"),
        };

        // Queue a RestoreState event, expect success.
        queue_event_expect_response(
            &vcpu_handle,
            VcpuEvent::RestoreState(vcpu_state),
            VcpuResponse::RestoredState,
        );

        vcpu_handle.send_event(VcpuEvent::Finish).unwrap();
    }

    #[test]
    fn test_vcpu_dump_cpu_config() {
        let (vcpu_handle, _) = vcpu_configured_for_boot();

        // Queue a DumpCpuConfig event, expect a DumpedCpuConfig response.
        vcpu_handle
            .send_event(VcpuEvent::DumpCpuConfig)
            .expect("Failed to send an event to vcpu.");
        match vcpu_handle
            .response_receiver()
            .recv_timeout(RECV_TIMEOUT_SEC)
            .expect("Could not receive a response from vcpu.")
        {
            VcpuResponse::DumpedCpuConfig(_) => (),
            VcpuResponse::Error(err) => panic!("Got an error: {err}"),
            _ => panic!("Got an unexpected response."),
        }

        // Queue a Resume event, expect a response.
        queue_event_expect_response(&vcpu_handle, VcpuEvent::Resume, VcpuResponse::Resumed);

        // Queue a DumpCpuConfig event, expect a NotAllowed respoonse.
        // The DumpCpuConfig event is only allowed while paused.
        queue_event_expect_response(
            &vcpu_handle,
            VcpuEvent::DumpCpuConfig,
            VcpuResponse::NotAllowed(String::new()),
        );

        vcpu_handle.send_event(VcpuEvent::Finish).unwrap();
    }

    #[test]
    fn test_vcpu_rtsig_offset() {
        assert!(validate_signal_num(sigrtmin() + VCPU_RTSIG_OFFSET).is_ok());
    }
}
