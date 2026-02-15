use std::{env, path::Path};

use anyhow::{Result, bail};
use lake_native_compiler::compiler::{compile, link};

fn main() -> Result<()> {
    env_logger::init();

    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        bail!("Usage: {} <source.lake>", args[0]);
    }

    let src_path = Path::new(&args[1]);
    let name = src_path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("Invalid source path: {}", src_path.display()))?;
    let build_dir = src_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("build");

    let obj_bytes = compile(src_path)?;
    link(&build_dir, name, &obj_bytes)?;

    println!("{}", build_dir.join(name).display());
    Ok(())
}
