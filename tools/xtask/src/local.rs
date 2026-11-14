use std::path::Path;

use rampire_ops::process;

const PACKAGES: &[&str] = &[
    "-p",
    "rampire-core",
    "-p",
    "rampire-guest",
    "-p",
    "rampire-host",
    "-p",
    "rampire-ops",
    "-p",
    "rampire",
    "-p",
    "xtask",
    "-p",
    "rampire-guest-linux",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Format,
    Build,
    Test,
    Lint,
    /// Format, build, test and lint: everything that needs no Git checkout,
    /// so a copied source tree on a build host can run it.
    Checks,
    Local,
}

pub fn run(step: Step, repo: &Path) -> Result<(), String> {
    match step {
        Step::Format => format(repo),
        Step::Build => build(repo),
        Step::Test => test(repo),
        Step::Lint => lint(repo),
        Step::Checks => checks(repo),
        Step::Local => {
            checks(repo)?;
            println!("==> whitespace validation");
            process::run("git", &["diff", "--check"], repo)
        }
    }
}

fn checks(repo: &Path) -> Result<(), String> {
    format(repo)?;
    build(repo)?;
    test(repo)?;
    lint(repo)?;
    shell_syntax(repo)
}

/// Every tracked shell script must at least parse.
fn shell_syntax(repo: &Path) -> Result<(), String> {
    println!("==> shell syntax");
    let mut scripts = Vec::new();
    for directory in ["deploy", "tools", "guest-linux"] {
        collect_scripts(&repo.join(directory), &mut scripts)?;
    }
    scripts.sort();
    for script in &scripts {
        let path = script.to_string_lossy();
        process::run("bash", &["-n", path.as_ref()], repo)?;
    }
    Ok(())
}

fn collect_scripts(directory: &Path, scripts: &mut Vec<std::path::PathBuf>) -> Result<(), String> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("read {}: {error}", directory.display())),
    };
    for entry in entries {
        let path = entry
            .map_err(|error| format!("read {}: {error}", directory.display()))?
            .path();
        if path.is_dir() {
            collect_scripts(&path, scripts)?;
        } else if path.extension().is_some_and(|extension| extension == "sh") {
            scripts.push(path);
        }
    }
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
