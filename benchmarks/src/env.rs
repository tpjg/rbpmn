//! Provenance: the machine, the checkout, and the database the numbers came
//! from.
//!
//! A benchmark number without these is folklore. Everything here ends up in
//! every result file, and the two halves are kept apart on purpose:
//!
//! - **Detected** facts (CPU, cores, RAM, OS, Postgres version and settings)
//!   are read from the machine at run time and cannot be wrong about the run.
//! - **Declared** facts come from `benchmarks/hardware.md`, which is a filled
//!   -in template rather than prose. It carries what no program can detect —
//!   above all whether the disk is NVMe, SATA or network-attached, which is
//!   the single fact most likely to explain a factor-of-ten difference
//!   between two otherwise identical machines.
//!
//! When the two disagree the run does not stop; it records both and says so
//! loudly. A stale declaration is a documentation bug, and refusing to
//! benchmark until it is fixed would be the wrong trade — but silently
//! trusting it would be worse.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hardware {
    /// Stable short id for this machine: results and micro baselines are
    /// keyed by it, because comparing absolute numbers across machines is
    /// exactly the mistake this track exists to prevent.
    pub host_id: String,
    pub detected: Detected,
    pub declared: Option<Declared>,
    /// Fields where the declaration and the machine disagree.
    pub declaration_mismatches: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Detected {
    pub os: String,
    pub arch: String,
    pub cpu_model: String,
    pub physical_cores: Option<u32>,
    pub logical_cores: Option<u32>,
    pub ram_gb: Option<f64>,
}

/// The machine-readable half of `hardware.md`. Deserialized from the TOML
/// block in that file — a template someone forgot to fill in is recorded as
/// such rather than guessed at.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declared {
    #[serde(default)]
    pub cpu_model: Option<String>,
    #[serde(default)]
    pub physical_cores: Option<u32>,
    #[serde(default)]
    pub ram_gb: Option<f64>,
    /// `nvme` | `ssd` | `network` | `unknown` — undetectable, and the field
    /// most worth knowing.
    pub disk: String,
    /// `local` | `remote`. Cross-checked against the connection URL.
    pub postgres_location: String,
    /// `local` | `compose`. Documentation for the reader; the harness records
    /// its own answer in `postgres.provisioned_by`.
    #[serde(default)]
    pub postgres_provisioning: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

impl Hardware {
    pub fn detect(root: &Path) -> Hardware {
        let detected = Detected::probe();
        let declared = Declared::load(&root.join("hardware.md"));
        let mut mismatches = Vec::new();
        if let Some(declared) = &declared {
            if let Some(cpu) = &declared.cpu_model
                && cpu != &detected.cpu_model
                && !cpu.starts_with('<')
            {
                mismatches.push(format!(
                    "hardware.md declares cpu_model = {cpu:?}, this machine reports {:?}",
                    detected.cpu_model
                ));
            }
            // Zero is the shipped template's placeholder, not a claim.
            if let (Some(a), Some(b)) = (declared.physical_cores, detected.physical_cores)
                && a != 0
                && a != b
            {
                mismatches.push(format!(
                    "hardware.md declares physical_cores = {a}, this machine reports {b}"
                ));
            }
            if let (Some(a), Some(b)) = (declared.ram_gb, detected.ram_gb)
                && a != 0.0
                && (a - b).abs() > 1.0
            {
                mismatches.push(format!(
                    "hardware.md declares ram_gb = {a}, this machine reports {b:.1}"
                ));
            }
            if declared.disk == "unknown" || declared.disk.starts_with('<') {
                mismatches.push(
                    "hardware.md does not say what the disk is — fill in `disk` \
                     (nvme | ssd | network) before publishing these numbers"
                        .to_string(),
                );
            }
        } else {
            mismatches.push(
                "benchmarks/hardware.md has no machine-readable toml block — \
                 the declared half of the hardware spec is missing"
                    .to_string(),
            );
        }
        Hardware {
            host_id: detected.host_id(),
            detected,
            declared,
            declaration_mismatches: mismatches,
        }
    }
}

impl Detected {
    pub fn probe() -> Detected {
        let os = std::env::consts::OS.to_string();
        let arch = std::env::consts::ARCH.to_string();
        let (cpu_model, physical_cores, logical_cores, ram_gb) = match os.as_str() {
            "macos" => (
                sysctl("machdep.cpu.brand_string").unwrap_or_else(|| "unknown".into()),
                sysctl("hw.physicalcpu").and_then(|v| v.parse().ok()),
                sysctl("hw.logicalcpu").and_then(|v| v.parse().ok()),
                sysctl("hw.memsize")
                    .and_then(|v| v.parse::<f64>().ok())
                    .map(|bytes| bytes / 1024.0 / 1024.0 / 1024.0),
            ),
            _ => probe_linux(),
        };
        Detected {
            os,
            arch,
            cpu_model,
            physical_cores,
            logical_cores,
            ram_gb,
        }
    }

    /// Stable across runs, meaningless across machines — which is the point.
    fn host_id(&self) -> String {
        let hostname = Command::new("hostname")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(hostname.as_bytes());
        hasher.update(self.cpu_model.as_bytes());
        hasher.update(self.arch.as_bytes());
        hasher.update(self.physical_cores.unwrap_or(0).to_le_bytes());
        format!("{:x}", hasher.finalize())[..8].to_string()
    }
}

fn probe_linux() -> (String, Option<u32>, Option<u32>, Option<f64>) {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let cpu_model = cpuinfo
        .lines()
        .find(|l| l.starts_with("model name"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    let logical = cpuinfo
        .lines()
        .filter(|l| l.starts_with("processor"))
        .count() as u32;
    // Physical cores: distinct (physical id, core id) pairs. Absent on
    // aarch64 and inside some containers, where logical is the honest answer.
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut physical_id = String::new();
    for line in cpuinfo.lines() {
        if let Some((key, value)) = line.split_once(':') {
            match key.trim() {
                "physical id" => physical_id = value.trim().to_string(),
                "core id" => pairs.push((physical_id.clone(), value.trim().to_string())),
                _ => {}
            }
        }
    }
    pairs.sort();
    pairs.dedup();
    let physical = if pairs.is_empty() {
        (logical > 0).then_some(logical)
    } else {
        Some(pairs.len() as u32)
    };
    let ram_gb = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|meminfo| {
            meminfo
                .lines()
                .find(|l| l.starts_with("MemTotal"))
                .and_then(|l| {
                    l.split_whitespace()
                        .nth(1)
                        .and_then(|v| v.parse::<f64>().ok())
                })
        })
        .map(|kb| kb / 1024.0 / 1024.0);
    (
        cpu_model,
        physical,
        (logical > 0).then_some(logical),
        ram_gb,
    )
}

fn sysctl(key: &str) -> Option<String> {
    let output = Command::new("sysctl").arg("-n").arg(key).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

impl Declared {
    /// Reads the first fenced `toml` block out of `hardware.md`. Markdown
    /// around it is for humans; the block is the contract.
    fn load(path: &Path) -> Option<Declared> {
        let text = std::fs::read_to_string(path).ok()?;
        let block = toml_block(&text)?;
        match toml::from_str(&block) {
            Ok(declared) => Some(declared),
            Err(e) => {
                eprintln!(
                    "warning: {}: toml block does not parse: {e}",
                    path.display()
                );
                None
            }
        }
    }
}

fn toml_block(text: &str) -> Option<String> {
    let mut inside = false;
    let mut block = String::new();
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            if inside {
                return Some(block);
            }
            inside = line.trim() == "```toml";
            continue;
        }
        if inside {
            block.push_str(line);
            block.push('\n');
        }
    }
    None
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkout {
    pub git_sha: Option<String>,
    /// Uncommitted changes in the working tree. A benchmark run against a
    /// dirty checkout is not reproducible and the result file has to say so.
    pub dirty: bool,
}

impl Checkout {
    pub fn detect(root: &Path) -> Checkout {
        let sha = git(root, &["rev-parse", "HEAD"]);
        let status = git(root, &["status", "--porcelain"]);
        Checkout {
            git_sha: sha,
            dirty: status.map(|s| !s.trim().is_empty()).unwrap_or(false),
        }
    }
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_hardware_template_parses() {
        // The template is data the harness reads on every run; a syntax
        // error in it must fail here rather than at 3am mid-benchmark.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(root.join("hardware.md")).expect("hardware.md exists");
        let block = toml_block(&text).expect("hardware.md carries a ```toml block");
        let declared: Declared = toml::from_str(&block).expect("the block parses");
        assert!(
            !declared.disk.is_empty(),
            "the disk field must exist even when unfilled"
        );
    }

    #[test]
    fn a_missing_block_is_none_not_a_panic() {
        assert!(toml_block("# just prose\n\nno fences here\n").is_none());
    }
}
