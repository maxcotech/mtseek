//! Repository automation. Run `cargo xtask help`.
//!
//! Every task CI runs is a task here, so "works on my machine" and "works in CI" are the
//! same command.

#![allow(clippy::print_stdout, clippy::print_stderr)] // a CLI that talks to a human

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const HELP: &str = "\
cargo xtask <command>

Commands:
  ci                       Everything CI checks: fmt, clippy (warnings are errors), tests
  fmt                      Format all code
  bench [bench.py args]    Build release mtseek, then run bench/bench.py against ripgrep
                           e.g. cargo xtask bench --queries rare,absent --modes files
  corpus <dir> [--seed N] [--files N]
                           Write a synthetic edge-case corpus to <dir> for manual testing
  help                     Show this message
";

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let cmd = args.next().unwrap_or_else(|| "help".into());
    let rest: Vec<String> = args.collect();
    match cmd.as_str() {
        "ci" => ci(),
        "fmt" => cargo(&["fmt", "--all"]),
        "bench" => bench(&rest),
        "corpus" => corpus(&rest),
        "help" | "-h" | "--help" => {
            print!("{HELP}");
            Ok(())
        }
        other => bail!("unknown command `{other}`\n\n{HELP}"),
    }
}

fn ci() -> Result<()> {
    cargo(&["fmt", "--all", "--check"])?;
    cargo(&[
        "clippy",
        "--workspace",
        "--all-targets",
        "--locked",
        "--",
        "-D",
        "warnings",
    ])?;
    cargo(&["test", "--workspace", "--locked"])?;
    cargo(&["doc", "--workspace", "--no-deps", "--locked"])?;
    println!("\nci: all checks passed");
    Ok(())
}

fn bench(args: &[String]) -> Result<()> {
    cargo(&[
        "build",
        "--release",
        "--locked",
        "--package",
        "mtseek-cli",
        "--package",
        "mtseek-daemon",
    ])?;
    let bench_dir = root().join("bench");
    let status = Command::new("python")
        .arg("bench.py")
        .args(args)
        .current_dir(&bench_dir)
        // Windows Python defaults to cp1252 when stdout is redirected, which can't print ✓.
        .env("PYTHONIOENCODING", "utf-8")
        .status()
        .context("failed to run python; is Python 3.10+ on PATH?")?;
    if !status.success() {
        bail!("bench.py failed with {status}");
    }
    Ok(())
}

fn corpus(args: &[String]) -> Result<()> {
    let mut spec = mtseek_testkit::CorpusSpec::default();
    let mut dir = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--seed" => spec.seed = it.next().context("--seed needs a value")?.parse()?,
            "--files" => spec.files = it.next().context("--files needs a value")?.parse()?,
            other if dir.is_none() => dir = Some(PathBuf::from(other)),
            other => bail!("unexpected argument `{other}`"),
        }
    }
    let dir = dir.context("usage: cargo xtask corpus <dir> [--seed N] [--files N]")?;
    if dir.exists() && dir.read_dir()?.next().is_some() {
        bail!("{} is not empty; pick a new directory", dir.display());
    }
    let stats = mtseek_testkit::generate(&dir, &spec)?;
    println!(
        "wrote {} files, {} bytes to {} (seed {})",
        stats.files,
        stats.bytes,
        dir.display(),
        spec.seed
    );
    Ok(())
}

fn cargo(args: &[&str]) -> Result<()> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    eprintln!("$ cargo {}", args.join(" "));
    let status = Command::new(cargo)
        .args(args)
        .current_dir(root())
        .status()?;
    if !status.success() {
        bail!("`cargo {}` failed with {status}", args.join(" "));
    }
    Ok(())
}

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below the workspace root")
}
