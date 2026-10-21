use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Clone, Debug)]
pub struct ServerDef {
    pub name: String,
    #[serde(default)]
    pub port: u16,
    pub url: Option<String>,
    #[serde(default = "root_path")]
    pub ready_path: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default = "default_startup")]
    pub startup_timeout_s: u64,
    #[serde(default)]
    pub data_dirs: Vec<String>,
    #[serde(default)]
    pub follow_redirects: bool,
    #[serde(default)]
    pub offloaded: Vec<Offload>,
    #[serde(default)]
    pub after_ready: Vec<Hook>,
    #[serde(default)]
    pub upload: UploadSpec,
    #[serde(skip)]
    pub dir: PathBuf,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Offload {
    pub kind: Kind,
    #[serde(default)]
    pub disqualify: bool,
    pub reason: String,
}

impl ServerDef {
    pub fn offload(&self, kind: Kind) -> Option<&Offload> {
        self.offloaded.iter().find(|o| o.kind == kind)
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct Hook {
    pub service: String,
    pub cmd: String,
}

#[derive(Deserialize, Clone, Debug)]
pub struct UploadSpec {
    #[serde(default = "upload_path")]
    pub path: String,
    #[serde(default = "std_auth")]
    pub auth: String,
    #[serde(default)]
    pub sha_header: bool,
    #[serde(default)]
    pub bound_auth: bool,
    pub content_type: Option<String>,
    pub content: Option<String>,
    #[serde(default)]
    pub settle_s: u64,
    #[serde(default)]
    pub resign: bool,
}

impl Default for UploadSpec {
    fn default() -> Self {
        Self {
            path: upload_path(),
            auth: std_auth(),
            sha_header: false,
            bound_auth: false,
            content_type: None,
            content: None,
            settle_s: 0,
            resign: false,
        }
    }
}

fn root_path() -> String {
    "/".into()
}
fn upload_path() -> String {
    "/upload".into()
}
fn std_auth() -> String {
    "std".into()
}
fn default_startup() -> u64 {
    60
}

#[derive(Deserialize, Clone, Debug)]
pub struct Suite {
    pub run: SuiteRun,
    #[serde(rename = "workload")]
    pub workloads: Vec<Workload>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct SuiteRun {
    pub server_cpus: String,
    pub load_cpus: String,
    pub service_cpus: Option<String>,
    #[serde(default = "default_memory")]
    pub server_memory: String,
    #[serde(default = "default_connections")]
    pub connections: usize,
    #[serde(default = "default_duration")]
    pub duration_s: u64,
    #[serde(default = "default_warmup")]
    pub warmup_s: u64,
    #[serde(default = "default_verify")]
    pub verify: usize,
    #[serde(default = "default_seed")]
    pub seed: u64,
}

fn default_memory() -> String {
    "8g".into()
}
fn default_connections() -> usize {
    64
}
fn default_duration() -> u64 {
    10
}
fn default_warmup() -> u64 {
    2
}
fn default_verify() -> usize {
    32
}
fn default_seed() -> u64 {
    1
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Upload,
    Get,
    Head,
    Miss,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Workload {
    pub name: String,
    pub kind: Kind,
    pub size: Option<u64>,
    pub count: Option<usize>,
    pub duration_s: Option<u64>,
    pub connections: Option<usize>,
    #[serde(default)]
    pub blobs: Vec<String>,
    pub zipf: Option<f64>,
    #[serde(default = "random_content")]
    pub content: String,
}

fn random_content() -> String {
    "random".into()
}

pub fn load_servers(dir: &Path) -> Result<Vec<ServerDef>> {
    let mut defs = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        let manifest = path.join("server.toml");
        if !manifest.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&manifest)?;
        let mut def: ServerDef =
            toml::from_str(&text).with_context(|| format!("parsing {}", manifest.display()))?;
        anyhow::ensure!(path.join("compose.yaml").is_file(), "{} has no compose.yaml", path.display());
        def.dir = path;
        defs.push(def);
    }
    defs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(defs)
}

pub fn load_suite(path: &Path) -> Result<Suite> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let suite: Suite = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    for w in &suite.workloads {
        match w.kind {
            Kind::Upload => {
                anyhow::ensure!(w.size.is_some() && w.count.is_some(), "{}: upload needs size and count", w.name);
            }
            _ => anyhow::ensure!(!w.blobs.is_empty(), "{}: needs blobs", w.name),
        }
        for b in &w.blobs {
            let src = suite.workloads.iter().find(|x| &x.name == b);
            anyhow::ensure!(
                src.is_some_and(|s| s.kind == Kind::Upload),
                "{}: blobs entry {} is not an upload workload",
                w.name,
                b
            );
        }
    }
    Ok(suite)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suite(text: &str) -> Result<Suite> {
        let dir = std::env::temp_dir().join(format!("bb-cfg-{}-{}", std::process::id(), text.len()));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("s.toml");
        std::fs::write(&path, text)?;
        load_suite(&path)
    }

    const RUN: &str = "[run]\nserver_cpus=\"0\"\nload_cpus=\"1\"\n";

    #[test]
    fn get_must_name_an_upload_workload() {
        let bad = format!("{RUN}[[workload]]\nname=\"g\"\nkind=\"get\"\nblobs=[\"nope\"]\n");
        assert!(suite(&bad).is_err());
        let good = format!(
            "{RUN}[[workload]]\nname=\"p\"\nkind=\"upload\"\nsize=10\ncount=2\n[[workload]]\nname=\"g\"\nkind=\"get\"\nblobs=[\"p\"]\n"
        );
        assert_eq!(suite(&good).unwrap().workloads.len(), 2);
    }

    #[test]
    fn offload_is_looked_up_by_handler_kind() {
        let def: ServerDef = toml::from_str(
            "name=\"x\"\n[[offloaded]]\nkind=\"get\"\ndisqualify=true\nreason=\"redirects\"\n[[offloaded]]\nkind=\"upload\"\nreason=\"queues\"\n",
        )
        .unwrap();
        assert!(def.offload(Kind::Get).unwrap().disqualify);
        assert!(!def.offload(Kind::Upload).unwrap().disqualify);
        assert!(def.offload(Kind::Head).is_none());
    }

    #[test]
    fn upload_needs_size_and_count() {
        let bad = format!("{RUN}[[workload]]\nname=\"p\"\nkind=\"upload\"\n");
        assert!(suite(&bad).is_err());
    }

    #[test]
    fn shipped_suites_and_servers_parse() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for s in ["standard", "quick"] {
            load_suite(&root.join(format!("suites/{s}.toml"))).unwrap();
        }
        let servers = load_servers(&root.join("servers")).unwrap();
        assert!(!servers.is_empty());
        for d in servers {
            let compose = std::fs::read_to_string(d.dir.join("compose.yaml")).unwrap();
            assert!(compose.contains("\n  server:"), "{} needs a service named server", d.name);
            assert!(d.dir.file_name().unwrap().to_string_lossy() == d.name, "{} folder name differs", d.name);
        }
    }
}
