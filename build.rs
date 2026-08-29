use std::env;
use std::ffi::OsStr;
use std::fmt::Display;
use std::path::PathBuf;
use std::process::Command;
use std::str::FromStr;

use anyhow::{Context, Result, bail};

fn main() {
    println!("cargo:rerun-if-env-changed=BUILD_VERSION");
    println!("cargo:rerun-if-changed=Cargo.toml");
    if let Ok(git_dir) = git_dir() {
        println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
        println!("cargo:rerun-if-changed={}", git_dir.join("refs").display());
        println!(
            "cargo:rerun-if-changed={}",
            git_dir.join("packed-refs").display()
        );
    }

    let hash = git_short_hash();
    let date = git_committer_date();
    let version = version();

    println!("cargo:rustc-env=GIT_SHORT_HASH={}", hash);
    println!("cargo:rustc-env=VERSION={}", version);
    println!("cargo:rustc-env=VERSION_LONG={version} ({hash} {date})");
}

fn git<I, S>(args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .args(args)
        .output()
        .context("Failed to run `git`")?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        bail!(
            "`git` failed, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    }
}

fn git_dir() -> Result<PathBuf> {
    git(["rev-parse", "--git-common-dir"]).map(PathBuf::from)
}

fn git_short_hash() -> String {
    git(["rev-parse", "--short", "HEAD"]).unwrap_or("no-hash".into())
}

fn git_committer_date() -> String {
    git(["log", "-1", "--date=short", "--format=%cd"]).unwrap_or("no-date".into())
}

fn git_dirty() -> bool {
    git(["status", "--porcelain"])
        .map(|output| !output.is_empty())
        .unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Version {
    major: u32,
    minor: u32,
    patch: u32,
    revision: Option<u32>,
    suffix: Option<String>,
}

impl FromStr for Version {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        if !s.starts_with("v") {
            bail!("must starts with 'v'");
        }

        let (version_str, suffix) = s[1..]
            .split_once('-')
            .map(|(v, suffix)| (v, Some(suffix)))
            .unwrap_or((&s[1..], None));

        let mut nums = vec![];
        for s in version_str.split('.') {
            if let Ok(num) = s.parse::<u32>() {
                nums.push(num);
            } else {
                bail!("non-numeric value in version string");
            }
        }
        if ![3, 4].contains(&nums.len()) {
            bail!("version parts number not 3 or 4");
        }

        Ok(Version {
            major: nums[0],
            minor: nums[1],
            patch: nums[2],
            revision: nums.get(3).copied(),
            suffix: suffix.map(Into::into),
        })
    }
}

impl Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "v{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(r) = self.revision {
            write!(f, ".{r}")?;
        }
        if let Some(suffix) = &self.suffix {
            write!(f, "-{suffix}")?;
        }
        Ok(())
    }
}

fn version() -> String {
    // Build version can be set by the environment variable.
    if let Ok(custom) = env::var("BUILD_VERSION")
        && !custom.trim().is_empty()
    {
        return custom;
    }

    let git_dirty_suffix = if git_dirty() { "-dirty" } else { "" };

    // The released version is the one declared in Cargo.toml.
    let cargo_version = cargo_version();

    // A proper release is a commit tagged with the exact Cargo.toml version;
    // show that version verbatim.
    if head_has_version_tag(&cargo_version) {
        return cargo_version.to_string() + git_dirty_suffix;
    }

    // Otherwise it is a development build heading for the next release. Release
    // branches (`vMAJOR.MINOR`) roll the patch; `main` (and anything else)
    // rolls the minor.
    let mut next = cargo_version;
    next.revision = None;
    next.suffix = None;
    if on_release_branch() {
        next.patch += 1;
    } else {
        next.minor += 1;
        next.patch = 0;
    }

    next.to_string() + "-nightly" + git_dirty_suffix
}

fn cargo_version() -> Version {
    let raw = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION is set by cargo");
    format!("v{raw}")
        .parse()
        .expect("CARGO_PKG_VERSION should be a valid version")
}

fn head_has_version_tag(target: &Version) -> bool {
    git(["tag", "--points-at", "HEAD"])
        .map(|tags| {
            tags.lines()
                .filter_map(|line| line.trim().parse::<Version>().ok())
                .any(|v| &v == target)
        })
        .unwrap_or(false)
}

fn on_release_branch() -> bool {
    git(["branch", "--show-current"])
        .map(|branch| is_release_branch(branch.trim()))
        .unwrap_or(false)
}

fn is_release_branch(branch: &str) -> bool {
    // Release branches look like `vMAJOR.MINOR`, e.g. `v0.1` or `v10.20`.
    let Some(rest) = branch.strip_prefix('v') else {
        return false;
    };
    let mut parts = rest.split('.');
    let (Some(major), Some(minor), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !major.is_empty()
        && !minor.is_empty()
        && major.chars().all(|c| c.is_ascii_digit())
        && minor.chars().all(|c| c.is_ascii_digit())
}
