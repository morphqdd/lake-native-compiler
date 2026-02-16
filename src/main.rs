use std::{
    fs,
    path::Path,
    sync::{Arc, atomic::{AtomicBool, Ordering}},
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use clap::{Parser, ValueEnum};
use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use lakec::compiler::{compile, ctx::OptLevel, link};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Opt {
    /// No optimisations (default)
    None,
    /// Optimise for speed
    Speed,
    /// Optimise for speed and size
    SpeedAndSize,
}

impl From<Opt> for OptLevel {
    fn from(o: Opt) -> Self {
        match o {
            Opt::None => OptLevel::None,
            Opt::Speed => OptLevel::Speed,
            Opt::SpeedAndSize => OptLevel::SpeedAndSize,
        }
    }
}

/// Lake native compiler
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Source file to compile
    source: String,

    /// Optimisation level
    #[arg(short = 'O', long, value_enum, default_value = "none")]
    opt: Opt,

    /// Strip debug symbols (linker --strip-all)
    #[arg(short, long)]
    strip: bool,

    /// Release mode: shorthand for -O speed-and-size --strip
    #[arg(short, long)]
    release: bool,

    /// Linker to use
    #[arg(long, default_value = "mold")]
    linker: String,
}

/// Run `work` while showing an animated progress bar.
/// The bar fills gradually over `fill_ms` milliseconds; when work finishes
/// it snaps to 100% and prints a done line.
fn with_progress<T>(
    label: &str,
    fill_ms: u64,
    work: impl FnOnce() -> T + Send + 'static,
) -> T
where
    T: Send + 'static,
{
    const STEPS: u64 = 200;

    let pb = ProgressBar::new(STEPS);
    pb.set_style(
        ProgressStyle::with_template(&format!(
            "  {{bar:42.cyan/dim}} {}",
            style(label).dim()
        ))
        .unwrap()
        .progress_chars("█░"),
    );

    let done = Arc::new(AtomicBool::new(false));
    let done2 = done.clone();
    let pb2 = pb.clone();
    let step_ms = fill_ms / STEPS;

    // Fill the bar gradually while work runs
    thread::spawn(move || {
        for _ in 0..STEPS {
            if done2.load(Ordering::Relaxed) { break; }
            pb2.inc(1);
            thread::sleep(Duration::from_millis(step_ms));
        }
        // Stay near full until work is done
        while !done2.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(10));
        }
    });

    let result = work();

    done.store(true, Ordering::Relaxed);
    pb.finish_and_clear();
    result
}

fn main() -> Result<()> {
    env_logger::init();

    let cli = Cli::parse();

    let (opt, strip) = if cli.release {
        (OptLevel::SpeedAndSize, true)
    } else {
        (OptLevel::from(cli.opt), cli.strip)
    };

    let mode = if cli.release {
        style("release").yellow().to_string()
    } else {
        style(opt.as_str()).dim().to_string()
    };

    let src_path = Path::new(&cli.source);
    let name = src_path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("Invalid source path: {}", src_path.display()))?
        .to_string();
    let build_dir = src_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("build");

    println!(
        "\n  {} {} [{}]",
        style("Compiling").cyan().bold(),
        style(src_path.display()).bold(),
        mode,
    );

    // ── Build ──────────────────────────────────────────────────────────────────
    let src_path_owned = src_path.to_path_buf();
    let t0 = Instant::now();
    let obj_bytes = with_progress("building", 300, move || compile(&src_path_owned, opt));
    let obj_bytes = obj_bytes?;
    let build_ms = t0.elapsed().as_millis();
    println!("  {} built   {}", style("✓").green().bold(), style(fmt_ms(build_ms)).dim());

    // ── Link ───────────────────────────────────────────────────────────────────
    let build_dir2 = build_dir.clone();
    let name2 = name.clone();
    let linker = cli.linker.clone();
    let obj = obj_bytes.clone();
    let t1 = Instant::now();
    with_progress(
        &format!("linking  {}", style(&cli.linker).dim()),
        100,
        move || link(&build_dir2, &name2, &obj, strip, &linker),
    )?;
    let link_ms = t1.elapsed().as_millis();
    println!("  {} linked  {}", style("✓").green().bold(), style(fmt_ms(link_ms)).dim());

    // ── Result ─────────────────────────────────────────────────────────────────
    let bin_path = build_dir.join(&name);
    let bin_size = fs::metadata(&bin_path)?.len();

    println!(
        "\n  {} {}\n  {} {}\n",
        style("→").green().bold(),
        style(bin_path.display()).bold(),
        style("size:").dim(),
        style(fmt_size(bin_size)).bold(),
    );

    Ok(())
}

fn fmt_ms(ms: u128) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.2}s", ms as f64 / 1000.0)
    }
}

fn fmt_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}
