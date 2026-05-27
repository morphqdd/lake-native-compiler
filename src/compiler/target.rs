//! Per-target constants the rt-fn codegen relies on.
//!
//! Linux syscall numbers differ between x86_64 and aarch64 (see
//! `arch/${arch}/include/uapi/asm/unistd.h` in the kernel tree).
//! Hardcoding x86_64 numbers in rt fn emission breaks the moment we
//! cross-build to aarch64 — this module centralises them so a future
//! target only needs to fill in a new `LinuxSyscalls` instance.
//!
//! Currently `LinuxSyscalls::for_target()` picks based on the host
//! since lakec runs on the same arch as the compiled output; once
//! we support cross-compilation through a `--target` flag, this
//! becomes parameterised on the build target instead.

/// Linux syscall numbers, per architecture.  Add fields as the
/// runtime grows — `arch/x86/entry/syscalls/syscall_64.tbl` and
/// `arch/arm64/tools/syscall_64.tbl` (or the generic `unistd.h`)
/// are the source of truth.
#[derive(Debug, Clone, Copy)]
pub struct LinuxSyscalls {
    pub sys_write: i64,
    pub sys_close: i64,
    pub sys_mmap: i64,
    pub sys_munmap: i64,
    pub sys_madvise: i64,
    pub sys_socket: i64,
    pub sys_bind: i64,
    pub sys_listen: i64,
    pub sys_setsockopt: i64,
    pub sys_nanosleep: i64,
    pub sys_exit: i64,
    pub sys_io_uring_setup: i64,
    pub sys_io_uring_enter: i64,
    pub sys_clone3: i64,
    pub sys_waitid: i64,
    pub sys_execve: i64,
}

impl LinuxSyscalls {
    /// Linux x86_64 — `syscall_64.tbl`.
    pub const X86_64: Self = Self {
        sys_write: 1,
        sys_close: 3,
        sys_mmap: 9,
        sys_munmap: 11,
        sys_madvise: 28,
        sys_socket: 41,
        sys_bind: 49,
        sys_listen: 50,
        sys_setsockopt: 54,
        sys_nanosleep: 35,
        sys_exit: 60,
        sys_io_uring_setup: 425,
        sys_io_uring_enter: 426,
        sys_clone3: 435,
        sys_waitid: 247,
        sys_execve: 59,
    };

    /// Linux aarch64 — `asm-generic/unistd.h`-derived (the kernel's
    /// "generic" syscall table is what arm64 uses).  Numbers from
    /// `arch/arm64/include/uapi/asm/unistd.h` + `linux/unistd.h`.
    pub const AARCH64: Self = Self {
        sys_write: 64,
        sys_close: 57,
        sys_mmap: 222,
        sys_munmap: 215,
        sys_madvise: 233,
        sys_socket: 198,
        sys_bind: 200,
        sys_listen: 201,
        sys_setsockopt: 208,
        sys_nanosleep: 101,
        sys_exit: 93,
        sys_io_uring_setup: 425,
        sys_io_uring_enter: 426,
        sys_clone3: 435,
        sys_waitid: 95,
        sys_execve: 221,
    };

    /// Pick the table matching the current host architecture.  Lake
    /// today is JIT/AOT-on-host, so target == host.  When a `--target`
    /// flag lands, swap this for a parameterised lookup.
    pub fn for_host() -> Self {
        Self::for_target(TargetArch::host())
    }

    /// Pick the table for an explicit target.
    pub fn for_target(target: TargetArch) -> Self {
        match target {
            TargetArch::X86_64 => Self::X86_64,
            TargetArch::Aarch64 => Self::AARCH64,
        }
    }
}

/// Cross-compile target arch.  Resolved from `--target` CLI arg or
/// defaults to host.  Lake supports Linux x86_64 and Linux aarch64.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetArch {
    X86_64,
    Aarch64,
}

impl TargetArch {
    /// Pick the arch matching the running compiler's host.  Used as
    /// the default when `--target` is not supplied.
    pub fn host() -> Self {
        match std::env::consts::ARCH {
            "x86_64" => Self::X86_64,
            "aarch64" => Self::Aarch64,
            other => panic!(
                "lake-native-compiler: host arch '{other}' not supported \
                 (only linux/x86_64 and linux/aarch64)"
            ),
        }
    }

    /// Parse a target string accepted by the `--target` CLI flag.
    /// Accepts bare arch names (`x86_64`, `aarch64`) and the more
    /// rustc-like triples (`x86_64-linux`, `aarch64-linux`,
    /// `aarch64-unknown-linux-gnu`).
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let arch = s.split('-').next().unwrap_or(s);
        match arch {
            "x86_64" => Ok(Self::X86_64),
            "aarch64" | "arm64" => Ok(Self::Aarch64),
            other => anyhow::bail!(
                "unsupported target arch '{other}' (expected x86_64 or aarch64)"
            ),
        }
    }

    /// Lowercase canonical name used by CARGO_CFG_TARGET_ARCH and
    /// `@cfg(arch="...")` lookups.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }

    /// Target triple string suitable for cranelift's `isa::lookup`.
    pub fn triple(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64-unknown-linux-gnu",
            Self::Aarch64 => "aarch64-unknown-linux-gnu",
        }
    }
}
