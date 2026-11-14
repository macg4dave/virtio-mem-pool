use std::path::{Path, PathBuf};
use std::time::Duration;

use rampire::attestation::{
    read_review, CompatibilityAttestation, VirshCompatibilityEvidenceSource,
};
use rampire::virsh::Virsh;

#[derive(Debug, PartialEq, Eq)]
pub struct Command {
    vm: String,
    alias: String,
    review: PathBuf,
    output: PathBuf,
    timeout: Duration,
    connection: String,
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    if args.len() < 3 {
        return Err("attestation requires VM_NAME ALIAS REVIEW and explicit options".to_owned());
    }
    validate_identity(&args[0], "VM_NAME")?;
    validate_identity(&args[1], "ALIAS")?;
    let mut output = None;
    let mut timeout = None;
    let mut connection = "qemu:///system".to_owned();
    let mut connection_seen = false;
    let mut index = 3;
    while index < args.len() {
        let option = args[index].as_str();
        index += 1;
        let value = args
            .get(index)
            .ok_or_else(|| format!("{option} requires a value"))?;
        match option {
            "--output" if output.is_none() => output = Some(PathBuf::from(value)),
            "--command-timeout-seconds" if timeout.is_none() => {
                timeout = Some(Duration::from_secs(
                    value
                        .parse::<u64>()
                        .ok()
                        .filter(|value| *value > 0)
                        .ok_or_else(|| format!("{option} requires a positive integer"))?,
                ));
            }
            "--connect" if !connection_seen && !value.trim().is_empty() => {
                connection_seen = true;
                connection = value.clone();
            }
            _ => return Err(format!("unknown or duplicate attestation option: {option}")),
        }
        index += 1;
    }
    Ok(Command {
        vm: args[0].clone(),
        alias: args[1].clone(),
        review: PathBuf::from(&args[2]),
        output: output.ok_or_else(|| "attestation requires --output".to_owned())?,
        timeout: timeout
            .ok_or_else(|| "attestation requires --command-timeout-seconds".to_owned())?,
        connection,
    })
}

pub fn run(command: &Command, repo: &Path) -> Result<(), String> {
    let review_path = resolve(repo, &command.review);
    let review = read_review(&review_path)?;
    let virsh = Virsh::with_connection("virsh", command.timeout, &command.connection);
    let live =
        VirshCompatibilityEvidenceSource::new(virsh, &command.vm, &command.alias).collect()?;
    let document = CompatibilityAttestation::new(live, review)?;
    let output = resolve(repo, &command.output);
    let parent = output
        .parent()
        .ok_or_else(|| "attestation output has no parent".to_owned())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create attestation output directory: {error}"))?;
    let temporary = parent.join(format!(".attestation-{}.tmp", std::process::id()));
    std::fs::write(&temporary, format!("{}\n", document.to_pretty_json()?))
        .map_err(|error| format!("write attestation: {error}"))?;
    std::fs::rename(&temporary, &output).map_err(|error| format!("commit attestation: {error}"))?;
    println!("Compatibility attestation written to {}", output.display());
    Ok(())
}

fn resolve(repo: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo.join(path)
    }
}

fn validate_identity(value: &str, name: &str) -> Result<(), String> {
    if value.is_empty()
        || value.starts_with('-')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
    {
        Err(format!("{name} contains unsafe characters"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attestation_requires_an_explicit_output_and_timeout() {
        let args = [
            "guest",
            "alias",
            "review.toml",
            "--output",
            "attestation.json",
            "--command-timeout-seconds",
            "10",
        ]
        .map(str::to_owned);
        assert!(parse(&args).is_ok());
        assert!(parse(&args[..3]).is_err());
    }
}
