use std::path::Path;

use crate::process;

const PACKAGES: &[&str] = &[
    "-p",
    "virtio-mem-core",
    "-p",
    "virtio-mem-host",
    "-p",
    "virtio-mem-xtask",
    "-p",
    "virtio-mem-guest-linux",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Format,
    Build,
    Test,
    Lint,
    Local,
}

pub fn run(step: Step, repo: &Path) -> Result<(), String> {
    match step {
        Step::Format => format(repo),
        Step::Build => build(repo),
        Step::Test => test(repo),
        Step::Lint => lint(repo),
        Step::Local => gate(repo),
    }
}

fn gate(repo: &Path) -> Result<(), String> {
    format(repo)?;
    build(repo)?;
    test(repo)?;
    lint(repo)?;

    println!("==> whitespace validation");
    process::run("git", &["diff", "--check"], repo)?;
    Ok(())
}

fn format(repo: &Path) -> Result<(), String> {
    println!("==> format");
    process::run("cargo", &["fmt", "--all", "--", "--check"], repo)
}

fn build(repo: &Path) -> Result<(), String> {
    let mut build = vec!["build"];
    build.extend_from_slice(PACKAGES);
    build.extend_from_slice(&["--all-features", "--release", "--locked"]);
    println!("==> release build (RHEL-compatible crates and tooling)");
    process::run("cargo", &build, repo)
}

fn test(repo: &Path) -> Result<(), String> {
    let mut test = vec!["test"];
    test.extend_from_slice(PACKAGES);
    test.extend_from_slice(&["--all-features", "--locked"]);
    println!("==> tests (RHEL-compatible crates and tooling)");
    process::run("cargo", &test, repo)
}

fn lint(repo: &Path) -> Result<(), String> {
    let mut clippy = vec!["clippy"];
    clippy.extend_from_slice(PACKAGES);
    clippy.extend_from_slice(&[
        "--all-targets",
        "--all-features",
        "--locked",
        "--",
        "-D",
        "warnings",
    ]);
    println!("==> clippy (warnings denied)");
    process::run("cargo", &clippy, repo)
}

pub fn doctor_host() -> Result<(), String> {
    let required = ["virsh"];
    let missing: Vec<_> = required
        .iter()
        .copied()
        .filter(|command| !process::command_exists(command))
        .collect();
    if missing.is_empty() {
        println!("Host validation prerequisites are available.");
        Ok(())
    } else {
        Err(format!(
            "missing required host command(s): {}",
            missing.join(", ")
        ))
    }
}
