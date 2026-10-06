use super::{pool::control::PoolSnapshot, TestResult};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub struct Artifacts {
    pub path: PathBuf,
    finished: bool,
}

impl Artifacts {
    pub fn new(scenario: &str) -> TestResult<Self> {
        let root = std::env::var_os("MINING_E2E_ARTIFACTS")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/mining-e2e"));
        fs::create_dir_all(&root)?;
        let directory = tempfile::Builder::new()
            .prefix(&format!("{scenario}-"))
            .tempdir_in(root)?;
        let path = directory.keep();
        let artifacts = Self {
            path,
            finished: false,
        };
        artifacts.json(
            "summary.json",
            &serde_json::json!({"scenario":scenario,"status":"running"}),
        )?;
        eprintln!("{scenario} artifacts: {}", artifacts.path.display());
        Ok(artifacts)
    }

    pub fn json(&self, name: &str, value: &impl Serialize) -> TestResult {
        fs::write(self.path.join(name), serde_json::to_vec_pretty(value)?)?;
        Ok(())
    }

    pub fn finish(&mut self, result: &TestResult, snapshots: &[PoolSnapshot]) -> TestResult {
        let mut events = File::create(self.path.join("events.jsonl"))?;
        for (pool, snapshot) in snapshots.iter().enumerate() {
            for event in &snapshot.events {
                serde_json::to_writer(
                    &mut events,
                    &serde_json::json!({"pool":pool,"sequence":event.sequence,"event":event.event}),
                )?;
                writeln!(events)?;
            }
        }
        self.json(
            "summary.json",
            &serde_json::json!({
                "status": if result.is_ok() {"passed"} else {"failed"},
                "error": result.as_ref().err().map(ToString::to_string),
                "accepted": snapshots.iter().map(|p| p.accepted).sum::<usize>(),
                "rejected": snapshots.iter().map(|p| p.rejected).sum::<usize>(),
                "pools": snapshots,
            }),
        )?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for Artifacts {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.json("summary.json", &serde_json::json!({"status":"failed","error":"scenario interrupted before cleanup completed"}));
        }
    }
}

pub fn binary_identity(path: &Path) -> TestResult<serde_json::Value> {
    // Hash each unchanged binary once per test executable; debug binaries can be hundreds of MB.
    type CacheKey = (PathBuf, u64, std::time::SystemTime);
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<CacheKey, serde_json::Value>>,
    > = std::sync::OnceLock::new();
    let metadata = fs::metadata(path)?;
    let key = (path.to_path_buf(), metadata.len(), metadata.modified()?);
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| super::error("binary identity cache poisoned"))?;
    if let Some(identity) = cache.get(&key) {
        return Ok(identity.clone());
    }
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let identity = serde_json::json!({"path":path,"sha256":format!("{:x}",hasher.finalize()),"bytes":metadata.len()});
    cache.insert(key, identity.clone());
    Ok(identity)
}
