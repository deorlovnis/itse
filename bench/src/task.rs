//! A task: a dir in Harbor's layout (`FILES`), with `environment/setup.sh WS` in place of a
//! Dockerfile. `tests/test.sh` runs from the task dir with WORKSPACE set, and its exit status is
//! the verdict, not Harbor's reward.txt.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::toml;

pub const FILES: &[&str] = &["instruction.md", "task.toml", "environment/setup.sh", "tests/test.sh", "solution/solve.sh"];

#[derive(Debug)]
pub struct Task {
    pub id: String,
    pub dir: PathBuf,
    pub sha: String,
    pub instruction: String,
    pub tags: Vec<String>,
    pub agent_timeout: Duration,
    pub verifier_timeout: Duration,
    pub bench: Bench,
}

/// `[itse_bench]`: what the `with` arm declares, and the harness's turn cap.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bench {
    #[serde(default, rename = "use")]
    pub uses: Vec<String>,
    pub max_turns: Option<u32>,
}

impl Task {
    pub fn load(dir: &Path) -> Result<Self> {
        let dir = fs::canonicalize(dir).with_context(|| format!("task {}", dir.display()))?;
        let id = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        let ok = !id.is_empty() && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        ensure!(ok, "task id '{id}' is not lowercase letters, digits and '-'");
        let missing: Vec<&str> = FILES.iter().copied().filter(|f| !dir.join(f).is_file()).collect();
        ensure!(missing.is_empty(), "{id}: missing {}", missing.join(", "));
        let doc = toml::parse(&fs::read_to_string(dir.join("task.toml"))?).with_context(|| format!("{id}/task.toml"))?;
        // Key by key, so an error names its table; Harbor's other keys (author, `[environment]`, …) are ignored.
        let key = |k: &str| doc.get(k).with_context(|| format!("{id}/task.toml: no {k}"));
        // Seconds, an integer or a float as Harbor writes them (`600.0`).
        let secs = |k: &str| key(k).map(|t| t["timeout_sec"].as_f64().filter(|s| *s > 0.0 && *s <= 1e9).map(Duration::from_secs_f64));
        let tags: Vec<String> = serde_json::from_value(key("metadata")?.get("tags").cloned().unwrap_or(json!([]))).with_context(|| format!("{id}/task.toml: [metadata] tags"))?;
        let bench: Bench = doc.get("itse_bench").map_or(Ok(Bench::default()), |v| serde_json::from_value(v.clone())).with_context(|| format!("{id}/task.toml: [itse_bench]"))?;
        let instruction = fs::read_to_string(dir.join("instruction.md"))?;
        let test_sh = fs::read_to_string(dir.join("tests/test.sh"))?;
        let (version, agent, verifier) = (key("version")?.as_str().unwrap_or_default(), secs("agent")?, secs("verifier")?);
        let checks = [
            (version.is_empty(), "task.toml: version must be a non-empty string"),
            (agent.is_none(), "task.toml: [agent] timeout_sec must be a number above 0"),
            (verifier.is_none(), "task.toml: [verifier] timeout_sec must be a number above 0"),
            (bench.max_turns == Some(0), "task.toml: max_turns must be above 0"),
            (instruction.trim().is_empty(), "instruction.md is empty"),
            (test_sh.contains("/logs/verifier"), "tests/test.sh: the verdict is its exit status, not /logs/verifier/reward.txt"),
        ];
        checks.iter().find(|(bad, _)| *bad).map_or(Ok(()), |(_, problem)| Err(anyhow!("{id}/{problem}")))?;
        Ok(Self {
            sha: sha(&dir).with_context(|| id.clone())?,
            instruction,
            tags,
            agent_timeout: agent.unwrap_or_default(),
            verifier_timeout: verifier.unwrap_or_default(),
            bench,
            id,
            dir,
        })
    }
}

/// The task dirs under `path` that hold any task file, by name (one missing its task.toml then fails to load); `path` itself if it holds a task.toml.
pub fn list(path: &Path) -> Result<Vec<PathBuf>> {
    if path.join("task.toml").exists() {
        return Ok(vec![path.into()]);
    }
    let task_like = |p: &PathBuf| FILES.iter().any(|f| p.join(f.split('/').next().unwrap_or(f)).exists());
    let mut dirs: Vec<PathBuf> = fs::read_dir(path).with_context(|| format!("tasks {}", path.display()))?.flatten().map(|e| e.path()).filter(task_like).collect();
    dirs.sort();
    ensure!(!dirs.is_empty(), "no task dirs (with a task.toml) in {}", path.display());
    Ok(dirs)
}

/// sha256 over the task's files sorted by relative path: each path, a NUL, its length (u64 BE)
/// and its bytes. Results name it, so two runs compare only on the same task text.
pub fn sha(dir: &Path) -> Result<String> {
    let mut files = vec![];
    walk(dir, dir, &mut files)?;
    files.sort();
    let mut h = Sha256::new();
    for (path, bytes) in files {
        [path.as_bytes(), &[0], &(bytes.len() as u64).to_be_bytes(), &bytes].iter().for_each(|part| h.update(part));
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            walk(root, &path, out)?;
        } else if meta.is_file() {
            let rel = path.strip_prefix(root)?.to_string_lossy().into_owned();
            out.push((rel, fs::read(&path)?));
        } else {
            bail!("{} is not a file or a dir", path.strip_prefix(root)?.display());
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/task.rs"]
mod tests;
