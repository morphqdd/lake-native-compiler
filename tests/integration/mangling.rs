//! Module-aware mangling smoke tests (#097, #102).
//!
//! Two scenarios that would have failed before the mangling pass landed:
//!
//!   * **#097** — user-defined `pub size` collides with `std.bytes.size`
//!     at the backend's flat symbol table.  Mangling now keys each
//!     machine by its defining module, so `lib.size` (canonical
//!     `lib__size`) and `std.bytes.size` (canonical `std_bytes__size`)
//!     coexist.
//!
//!   * **#102** — re-exporting `pub die` through `std.process` AND
//!     `std.sys` produces one `Item::Machine` (in `std.process`) but
//!     two callsites.  Mangling makes both callsites resolve to the
//!     same canonical `std_process__die`, and the backend's
//!     predeclare is now idempotent so a single defining-module emit
//!     wins.
//!
//! Each test writes a tempdir layout, points `+import` at a local
//! `std/` directory (the loader's project-root search root, no env
//! vars needed), and runs the program end-to-end.

use std::fs;

use indicatif::ProgressBar;
use lakec::compiler::{compile, ctx::OptLevel, link};
use tempfile::tempdir;

#[test]
fn issue_097_user_pub_size_shadows_stdlib_size() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("std")).unwrap();

    // User module: `pub size` returns 42.  Spawn-style (no ret-ty)
    // keeps the test off the synchronous ret-machine path so we
    // exercise the bare-mangling collision without dragging in
    // ret/wait lowering.
    fs::write(
        root.join("lib.lake"),
        r#"
            @rt(rt_exit)
            pub size is {
              _x i64 -> { rt_exit(42) }
            }
        "#,
    )
    .unwrap();

    // Local std.bytes with a single `pub size` that shares the
    // bare name with the user's `pub size`.  Spawn-style so the
    // test stays off the ret/wait lowering path; exit code 7
    // distinguishes it from the user's 42 exit.
    fs::write(
        root.join("std/bytes.lake"),
        r#"
            @rt(rt_exit)
            pub size is {
              b buf -> { rt_exit(7) }
            }
        "#,
    )
    .unwrap();

    // Main imports BOTH `size` machines.  Pre-fix this fails at
    // predeclare with "duplicate symbol size".  After mangling each
    // machine has its own canonical (`lib__size`, `std_bytes__size`)
    // and both predeclare without collision.  Main spawns the user
    // `size`, which exits 42; the stdlib `size` is never invoked so
    // its rt_exit(7) doesn't run — what matters is the predeclare
    // pass alone succeeds.
    fs::write(
        root.join("main.lake"),
        r#"
            +lib.{ size }
            +std.bytes.{ size as bsize }
            @rt(rt_exit)

            main is {
              _ -> { size(0) }
            }
        "#,
    )
    .unwrap();

    let bytes = compile(
        ProgressBar::new(0),
        root.join("main.lake"),
        OptLevel::None,
    )
    .expect("compile succeeds with module-mangled symbols");
    link(root, "prog", &bytes, false, "mold").expect("links");
    let out = std::process::Command::new(root.join("prog"))
        .output()
        .unwrap();
    let code = out.status.code().unwrap_or(-1);
    assert_eq!(
        code, 42,
        "expected user size's exit(42); got {code}; stderr={:?}",
        out.stderr
    );
}

#[test]
fn issue_102_pub_machine_reexported_through_two_paths() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("std")).unwrap();

    // Defining module: pub die exits with code 7.
    fs::write(
        root.join("std/process.lake"),
        r#"
            @rt(rt_exit)
            pub die is {
              code i64 -> { rt_exit(code) }
            }
        "#,
    )
    .unwrap();

    // Re-export wrapper module: imports die from std.process so a
    // user can write `+std.sys.{ die }` instead of going through
    // std.process directly.
    fs::write(
        root.join("std/sys.lake"),
        r#"
            +std.process.{ die }
        "#,
    )
    .unwrap();

    // Main pulls die via BOTH paths.  Pre-fix this produced two
    // predeclares of the same `die` symbol → mold "duplicate symbol".
    fs::write(
        root.join("main.lake"),
        r#"
            +std.process.{ die }
            +std.sys.{ die as die2 }

            main is {
              _ -> {
                die(11)
              }
            }
        "#,
    )
    .unwrap();

    let bytes = compile(
        ProgressBar::new(0),
        root.join("main.lake"),
        OptLevel::None,
    )
    .expect("compile succeeds — single canonical predeclare");
    link(root, "prog", &bytes, false, "mold").expect("links");
    let out = std::process::Command::new(root.join("prog"))
        .output()
        .unwrap();
    let code = out.status.code().unwrap_or(-1);
    assert_eq!(
        code, 11,
        "expected die(11); got {code}; stderr={:?}",
        out.stderr
    );
}
