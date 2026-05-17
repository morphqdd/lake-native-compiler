//! Build script: assemble the per-target runtime shims
//! (`entry.asm` + `syscall.asm`) for the target architecture.
//!
//! The output objects land in `external/build/${TARGET_ARCH}/` so that
//! the host x86_64 build and a cross-build to aarch64 don't clobber each
//! other's .o files when both sit in the same source tree.
//!
//! Tooling:
//!   x86_64  → `as` (GNU binutils) with Intel syntax (see entry.asm)
//!   aarch64 → host `as` if TARGET_ARCH == HOST_ARCH, otherwise
//!             `${target_prefix}-as` (e.g. aarch64-linux-gnu-as) from
//!             cross binutils — needs to be installed in CI.
//!
//! The compiled .o files are then `include_bytes!`-embedded by
//! `src/compiler/mod.rs` at compile time, so the lakec binary stays
//! a single fat artifact and the linker step can blast them out to
//! the user's build directory at run time.

use std::{path::PathBuf, process::Command};

use anyhow::{Context, Result, bail};

fn main() -> Result<()> {
    println!("cargo:rerun-if-changed=external/x86_64/entry.asm");
    println!("cargo:rerun-if-changed=external/x86_64/syscall.asm");
    println!("cargo:rerun-if-changed=external/x86_64/tsc.asm");
    println!("cargo:rerun-if-changed=external/aarch64/entry.asm");
    println!("cargo:rerun-if-changed=external/aarch64/syscall.asm");
    println!("cargo:rerun-if-changed=external/aarch64/tsc.asm");

    let target_arch =
        std::env::var("CARGO_CFG_TARGET_ARCH").context("CARGO_CFG_TARGET_ARCH not set")?;
    let target = std::env::var("TARGET").unwrap_or_default();

    // Cross-assembler prefix: empty when host == target.  Otherwise pick
    // a conventional GNU triple prefix so CI just needs to install
    // `binutils-aarch64-linux-gnu` (or equivalent) on x86 runners.
    let as_tool = match target_arch.as_str() {
        "x86_64" if target.contains("linux") => "as".to_string(),
        "aarch64" if target.contains("linux") => {
            // Use host `as` when building natively on aarch64; otherwise
            // expect the cross binutils package.
            if std::env::consts::ARCH == "aarch64" {
                "as".to_string()
            } else {
                std::env::var("LAKE_AARCH64_AS")
                    .unwrap_or_else(|_| "aarch64-linux-gnu-as".to_string())
            }
        }
        other => bail!(
            "lake-native-compiler: target arch '{other}' not supported (only x86_64 / aarch64 linux)"
        ),
    };

    let src_dir = PathBuf::from(format!("external/{target_arch}"));
    let out_dir: PathBuf = std::env::var("OUT_DIR").context("OUT_DIR not set")?.into();

    for stem in ["entry", "syscall", "tsc"] {
        let src = src_dir.join(format!("{stem}.asm"));
        if !src.exists() {
            bail!(
                "missing assembly source {} — add it before targeting {target_arch}",
                src.display()
            );
        }
        let out = out_dir.join(format!("{stem}.o"));
        let status = Command::new(&as_tool)
            .arg("-o")
            .arg(&out)
            .arg(&src)
            .status()
            .with_context(|| {
                format!(
                    "failed to invoke `{}` — install GNU binutils for target {target_arch}",
                    as_tool
                )
            })?;
        if !status.success() {
            bail!("{as_tool} failed to assemble {}", src.display());
        }
        println!(
            "cargo:rustc-env=LAKE_{}_OBJ={}",
            stem.to_uppercase(),
            out.display()
        );
    }

    Ok(())
}
