//! Integration tests for `@cfg(arch="...")` — feature #054 / #055.
//!
//! A `.lake` program with two same-named `@cfg` consts must compile under
//! both `--target x86_64` and `--target aarch64` (the filter pass drops the
//! non-matching const before the registry can see a name collision), and
//! the surviving const must carry the arch-correct value.

use indicatif::ProgressBar;
use lakec::compiler::{compile_for_target, ctx::OptLevel, link, target::TargetArch};
use std::{fs, process::Command};
use tempfile::tempdir;

/// Two `@cfg` consts named `SYS_SOCKET` — x86_64=41, aarch64=198 — plus a
/// `main` that exits with that value.  Reused across the per-target cases.
const SRC: &str = r#"
@rt(rt_exit)

@cfg(arch="x86_64")
pub const SYS_SOCKET = 41

@cfg(arch="aarch64")
pub const SYS_SOCKET = 198

main is { _ -> {
  rt_exit(SYS_SOCKET)
} }
"#;

fn compile_target(src: &str, arch: TargetArch) -> Vec<u8> {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("prog.lake");
    fs::write(&src_path, src).unwrap();
    compile_for_target(ProgressBar::new(0), &src_path, OptLevel::None, arch)
        .expect("compile_for_target should succeed (no name collision)")
}

#[test]
fn same_name_cfg_const_compiles_under_both_targets() {
    // The key property: NO duplicate-`SYS_SOCKET` collision under either
    // target.  Both must emit a non-empty object.
    let x86 = compile_target(SRC, TargetArch::X86_64);
    assert!(!x86.is_empty(), "x86_64 object empty");
    let arm = compile_target(SRC, TargetArch::Aarch64);
    assert!(!arm.is_empty(), "aarch64 object empty");
}

#[test]
fn host_target_picks_matching_const_value() {
    // Compile + link + run for the host arch and assert the exit code is
    // the arch-correct `SYS_SOCKET` value.  We only execute the host
    // binary (cross-arch binaries won't run here).
    let host = TargetArch::host();
    let expected = match host {
        TargetArch::X86_64 => 41,
        TargetArch::Aarch64 => 198,
    };
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("prog.lake");
    fs::write(&src_path, SRC).unwrap();
    let bytes =
        compile_for_target(ProgressBar::new(0), &src_path, OptLevel::None, host).unwrap();
    link(dir.path(), "prog", &bytes, false, "mold").unwrap();
    let out = Command::new(dir.path().join("prog")).output().unwrap();
    assert_eq!(
        out.status.code().unwrap_or(-1),
        expected,
        "host {host:?} should pick SYS_SOCKET={expected}"
    );
}

#[test]
fn stdlib_sys_lake_cfg_consts_compile_for_both_targets() {
    // #055 — the migrated `std/experimental/sys.lake` declares each
    // syscall number twice behind `@cfg`.  Importing it and using a
    // per-arch const must compile cleanly under BOTH targets (no
    // duplicate-name collision from the cfg-filter pass).
    let stdlib = "/home/morphe/compiler/lake-stdlib";
    if !std::path::Path::new(stdlib).join("std/experimental/sys.lake").exists() {
        eprintln!("skip: stdlib not present at {stdlib}");
        return;
    }
    unsafe {
        std::env::set_var("LAKE_PATH", stdlib);
    }
    let src = r#"
        +std.experimental.sys.{ SYS_SOCKET }
        @rt(rt_exit)
        main is { _ -> { rt_exit(SYS_SOCKET) } }
    "#;
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("prog.lake");
    fs::write(&src_path, src).unwrap();

    let x86 =
        compile_for_target(ProgressBar::new(0), &src_path, OptLevel::None, TargetArch::X86_64)
            .expect("x86_64 compile of sys.lake import");
    assert!(!x86.is_empty());
    let arm =
        compile_for_target(ProgressBar::new(0), &src_path, OptLevel::None, TargetArch::Aarch64)
            .expect("aarch64 compile of sys.lake import");
    assert!(!arm.is_empty());

    // For the host arch, link + run and confirm SYS_SOCKET resolved to
    // the arch-correct value (41 on x86_64, 198 on aarch64).
    let host = TargetArch::host();
    let expected = match host {
        TargetArch::X86_64 => 41,
        TargetArch::Aarch64 => 198,
    };
    let bytes =
        compile_for_target(ProgressBar::new(0), &src_path, OptLevel::None, host).unwrap();
    link(dir.path(), "prog", &bytes, false, "mold").unwrap();
    let out = Command::new(dir.path().join("prog")).output().unwrap();
    assert_eq!(out.status.code().unwrap_or(-1), expected);
}

#[test]
fn unconditional_const_survives_all_targets() {
    let src = r#"
        @rt(rt_exit)
        pub const ANSWER = 7
        main is { _ -> { rt_exit(ANSWER) } }
    "#;
    assert!(!compile_target(src, TargetArch::X86_64).is_empty());
    assert!(!compile_target(src, TargetArch::Aarch64).is_empty());
}
