// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use libc::{
    c_int, c_void, siginfo_t, SIGBUS, SIGHUP, SIGILL, SIGPIPE, SIGSEGV, SIGSYS, SIGXCPU, SIGXFSZ,
};
use logger::{error, IncMetric, StoreMetric, METRICS};
use utils::signal::register_signal_handler;

use crate::FcExitCode;

// The offset of `si_syscall` (offending syscall identifier) within the siginfo structure
// expressed as an `(u)int*`.
// Offset `6` for an `i32` field means that the needed information is located at `6 * sizeof(i32)`.
// See /usr/include/linux/signal.h for the C struct definition.
// See https://github.com/rust-lang/libc/issues/716 for why the offset is different in Rust.
const SI_OFF_SYSCALL: isize = 6;

const SYS_SECCOMP_CODE: i32 = 1;

#[inline]
fn exit_with_code(exit_code: FcExitCode) {
    // Write the metrics before exiting.
    if let Err(err) = METRICS.write() {
        error!("Failed to write metrics while stopping: {}", err);
    }
    // SAFETY: Safe because we're terminating the process anyway.
    unsafe { libc::_exit(exit_code as i32) };
}

macro_rules! generate_handler {
    ($fn_name:ident ,$signal_name:ident, $exit_code:ident, $signal_metric:expr, $body:ident) => {
        #[inline(always)]
        extern "C" fn $fn_name(num: c_int, info: *mut siginfo_t, _unused: *mut c_void) {
            // SAFETY: Safe because we're just reading some fields from a supposedly valid argument.
            let si_signo = unsafe { (*info).si_signo };
            // SAFETY: Safe because we're just reading some fields from a supposedly valid argument.
            let si_code = unsafe { (*info).si_code };

            if num != si_signo || num != $signal_name {
                exit_with_code(FcExitCode::UnexpectedError);
            }
            $signal_metric.store(1);

            error!(
                "Shutting down VM after intercepting signal {}, code {}.",
                si_signo, si_code
            );

            $body(si_code, info);

            #[cfg(not(test))]
            match si_signo {
                $signal_name => exit_with_code(crate::FcExitCode::$exit_code),
                _ => exit_with_code(FcExitCode::UnexpectedError),
            };
        }
    };
}

fn log_sigsys_err(si_code: c_int, info: *mut siginfo_t) {
    if si_code != SYS_SECCOMP_CODE {
        // We received a SIGSYS for a reason other than `bad syscall`.
        exit_with_code(FcExitCode::UnexpectedError);
    }

    // SAFETY: Other signals which might do async unsafe things incompatible with the rest of this
    // function are blocked due to the sa_mask used when registering the signal handler.
    let syscall = unsafe { *(info as *const i32).offset(SI_OFF_SYSCALL) };
    let syscall = usize::try_from(syscall).unwrap();
    error!(
        "Shutting down VM after intercepting a bad syscall ({}).",
        syscall
    );
}

fn empty_fn(_si_code: c_int, _info: *mut siginfo_t) {}

generate_handler!(
    sigxfsz_handler,
    SIGXFSZ,
    SIGXFSZ,
    METRICS.signals.sigxfsz,
    empty_fn
);

generate_handler!(
    sigxcpu_handler,
    SIGXCPU,
    SIGXCPU,
    METRICS.signals.sigxcpu,
    empty_fn
);

generate_handler!(
    sigbus_handler,
    SIGBUS,
    SIGBUS,
    METRICS.signals.sigbus,
    empty_fn
);

generate_handler!(
    sigsegv_handler,
    SIGSEGV,
    SIGSEGV,
    METRICS.signals.sigsegv,
    empty_fn
);

generate_handler!(
    sigsys_handler,
    SIGSYS,
    BadSyscall,
    METRICS.seccomp.num_faults,
    log_sigsys_err
);

generate_handler!(
    sighup_handler,
    SIGHUP,
    SIGHUP,
    METRICS.signals.sighup,
    empty_fn
);
generate_handler!(
    sigill_handler,
    SIGILL,
    SIGILL,
    METRICS.signals.sigill,
    empty_fn
);

#[inline(always)]
extern "C" fn sigpipe_handler(num: c_int, info: *mut siginfo_t, _unused: *mut c_void) {
    // Just record the metric and allow the process to continue, the EPIPE error needs
    // to be handled at caller level.

    // SAFETY: Safe because we're just reading some fields from a supposedly valid argument.
    let si_signo = unsafe { (*info).si_signo };
    // SAFETY: Safe because we're just reading some fields from a supposedly valid argument.
    let si_code = unsafe { (*info).si_code };

    if num != si_signo || num != SIGPIPE {
        error!("Received invalid signal {}, code {}.", si_signo, si_code);
        return;
    }

    METRICS.signals.sigpipe.inc();

    error!("Received signal {}, code {}.", si_signo, si_code);
}

/// Registers all the required signal handlers.
///
/// Custom handlers are installed for: `SIGBUS`, `SIGSEGV`, `SIGSYS`
/// `SIGXFSZ` `SIGXCPU` `SIGPIPE` `SIGHUP` and `SIGILL`.
pub fn register_signal_handlers() -> utils::errno::Result<()> {
    // Call to unsafe register_signal_handler which is considered unsafe because it will
    // register a signal handler which will be called in the current thread and will interrupt
    // whatever work is done on the current thread, so we have to keep in mind that the registered
    // signal handler must only do async-signal-safe operations.
    register_signal_handler(SIGSYS, sigsys_handler)?;
    register_signal_handler(SIGBUS, sigbus_handler)?;
    register_signal_handler(SIGSEGV, sigsegv_handler)?;
    register_signal_handler(SIGXFSZ, sigxfsz_handler)?;
    register_signal_handler(SIGXCPU, sigxcpu_handler)?;
    register_signal_handler(SIGPIPE, sigpipe_handler)?;
    register_signal_handler(SIGHUP, sighup_handler)?;
    register_signal_handler(SIGILL, sigill_handler)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::undocumented_unsafe_blocks)]
    use std::{process, thread};

    use libc::syscall;
    use seccompiler::sock_filter;

    use super::*;

    #[test]
    fn test_signal_handler() {
        let child = thread::spawn(move || {
            assert!(register_signal_handlers().is_ok());

            let filter = make_test_seccomp_bpf_filter();

            assert!(seccompiler::apply_filter(&filter).is_ok());
            assert_eq!(METRICS.seccomp.num_faults.fetch(), 0);

            // Call the forbidden `SYS_mkdirat`.
            unsafe { libc::syscall(libc::SYS_mkdirat, "/foo/bar\0") };

            // Call SIGBUS signal handler.
            assert_eq!(METRICS.signals.sigbus.fetch(), 0);
            unsafe {
                syscall(libc::SYS_kill, process::id(), SIGBUS);
            }

            // Call SIGSEGV signal handler.
            assert_eq!(METRICS.signals.sigsegv.fetch(), 0);
            unsafe {
                syscall(libc::SYS_kill, process::id(), SIGSEGV);
            }

            // Call SIGXFSZ signal handler.
            assert_eq!(METRICS.signals.sigxfsz.fetch(), 0);
            unsafe {
                syscall(libc::SYS_kill, process::id(), SIGXFSZ);
            }

            // Call SIGXCPU signal handler.
            assert_eq!(METRICS.signals.sigxcpu.fetch(), 0);
            unsafe {
                syscall(libc::SYS_kill, process::id(), SIGXCPU);
            }

            // Call SIGPIPE signal handler.
            assert_eq!(METRICS.signals.sigpipe.count(), 0);
            unsafe {
                syscall(libc::SYS_kill, process::id(), SIGPIPE);
            }

            // Call SIGHUP signal handler.
            assert_eq!(METRICS.signals.sighup.fetch(), 0);
            unsafe {
                syscall(libc::SYS_kill, process::id(), SIGHUP);
            }

            // Call SIGILL signal handler.
            assert_eq!(METRICS.signals.sigill.fetch(), 0);
            unsafe {
                syscall(libc::SYS_kill, process::id(), SIGILL);
            }
        });
        assert!(child.join().is_ok());

        assert!(METRICS.seccomp.num_faults.fetch() >= 1);
        assert!(METRICS.signals.sigbus.fetch() >= 1);
        assert!(METRICS.signals.sigsegv.fetch() >= 1);
        assert!(METRICS.signals.sigxfsz.fetch() >= 1);
        assert!(METRICS.signals.sigxcpu.fetch() >= 1);
        assert!(METRICS.signals.sigpipe.count() >= 1);
        assert!(METRICS.signals.sighup.fetch() >= 1);
        assert!(METRICS.signals.sigill.fetch() >= 1);
    }

    fn make_test_seccomp_bpf_filter() -> Vec<sock_filter> {
        // Create seccomp filter that allows all syscalls, except for `SYS_mkdirat`.
        // For some reason, directly calling `SYS_kill` with SIGSYS, like we do with the
        // other signals, results in an error. Probably because of the way `cargo test` is
        // handling signals.
        #[cfg(target_arch = "aarch64")]
        #[allow(clippy::unreadable_literal)]
        let bpf_filter = vec![
            sock_filter {
                code: 32,
                jt: 0,
                jf: 0,
                k: 4,
            },
            sock_filter {
                code: 21,
                jt: 1,
                jf: 0,
                k: 3221225655,
            },
            sock_filter {
                code: 6,
                jt: 0,
                jf: 0,
                k: 0,
            },
            sock_filter {
                code: 32,
                jt: 0,
                jf: 0,
                k: 0,
            },
            sock_filter {
                code: 21,
                jt: 0,
                jf: 1,
                k: 34,
            },
            sock_filter {
                code: 5,
                jt: 0,
                jf: 0,
                k: 1,
            },
            sock_filter {
                code: 5,
                jt: 0,
                jf: 0,
                k: 2,
            },
            sock_filter {
                code: 6,
                jt: 0,
                jf: 0,
                k: 196608,
            },
            sock_filter {
                code: 6,
                jt: 0,
                jf: 0,
                k: 2147418112,
            },
            sock_filter {
                code: 6,
                jt: 0,
                jf: 0,
                k: 2147418112,
            },
        ];
        #[cfg(target_arch = "x86_64")]
        #[allow(clippy::unreadable_literal)]
        let bpf_filter = vec![
            sock_filter {
                code: 32,
                jt: 0,
                jf: 0,
                k: 4,
            },
            sock_filter {
                code: 21,
                jt: 1,
                jf: 0,
                k: 3221225534,
            },
            sock_filter {
                code: 6,
                jt: 0,
                jf: 0,
                k: 0,
            },
            sock_filter {
                code: 32,
                jt: 0,
                jf: 0,
                k: 0,
            },
            sock_filter {
                code: 21,
                jt: 0,
                jf: 1,
                k: 258,
            },
            sock_filter {
                code: 5,
                jt: 0,
                jf: 0,
                k: 1,
            },
            sock_filter {
                code: 5,
                jt: 0,
                jf: 0,
                k: 2,
            },
            sock_filter {
                code: 6,
                jt: 0,
                jf: 0,
                k: 196608,
            },
            sock_filter {
                code: 6,
                jt: 0,
                jf: 0,
                k: 2147418112,
            },
            sock_filter {
                code: 6,
                jt: 0,
                jf: 0,
                k: 2147418112,
            },
        ];

        bpf_filter
    }
}
