use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::ServerDef;
use crate::load;

pub struct Vars {
    pub port: u16,
    pub data: PathBuf,
    pub pubkey: String,
}

impl Vars {
    fn npub(&self) -> String {
        let mut key = [0u8; 32];
        match hex::decode(&self.pubkey) {
            Ok(bytes) if bytes.len() == 32 => key.copy_from_slice(&bytes),
            _ => return String::new(),
        }
        crate::nostr::npub(&key)
    }

    pub fn expand(&self, text: &str) -> String {
        text.replace("${BB_PORT}", &self.port.to_string())
            .replace("${BB_DATA}", &self.data.to_string_lossy())
            .replace("${BB_PUBKEY}", &self.pubkey)
            .replace("${BB_NPUB}", &self.npub())
    }
}

pub struct Running {
    pub addr: SocketAddr,
    pub roots: Vec<u32>,
    project: Option<Project>,
    pub log: PathBuf,
}

#[derive(Clone)]
struct Project {
    name: String,
    dir: PathBuf,
    env: Vec<(String, String)>,
    ram_override: Option<PathBuf>,
}

impl Project {
    fn compose(&self) -> StdCommand {
        let mut cmd = StdCommand::new("docker");
        cmd.args(["compose", "-p", &self.name, "--project-directory"]).arg(&self.dir).arg("-f").arg(self.dir.join("compose.yaml"));
        if let Some(extra) = &self.ram_override {
            cmd.arg("-f").arg(extra);
        }
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd
    }

    fn containers(&self) -> Vec<String> {
        let out = self.compose().args(["ps", "-a", "-q"]).output();
        out.map(|o| String::from_utf8_lossy(&o.stdout).lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
            .unwrap_or_default()
    }
}

fn run_cmd(cmd: &mut StdCommand, log: &Path) -> Result<()> {
    let out = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    let status = cmd.stdout(Stdio::from(out.try_clone()?)).stderr(Stdio::from(out)).status()?;
    if !status.success() {
        bail!("{:?} failed ({status}); see {}", cmd.get_program(), log.display());
    }
    Ok(())
}

pub struct Limits<'a> {
    pub ram_data: bool,
    pub server_cpus: &'a [usize],
    pub service_cpus: &'a [usize],
    pub memory: &'a str,
}

fn cpu_list(cpus: &[usize]) -> String {
    cpus.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(",")
}

fn container_pid(id: &str) -> Option<u32> {
    let out = StdCommand::new("docker").args(["inspect", "-f", "{{.State.Pid}}", id]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok().filter(|p| *p != 0)
}

fn uid_gid() -> String {
    unsafe { format!("{}:{}", libc::getuid(), libc::getgid()) }
}

fn project(def: &ServerDef, work: &Path, vars: &Vars, limits: &Limits<'_>) -> Project {
    let service_cpus = if limits.service_cpus.is_empty() {
        format!("0-{}", crate::sys::online_cpus().saturating_sub(1))
    } else {
        cpu_list(limits.service_cpus)
    };
    let _ = work;
    Project {
        name: format!("bb-{}", def.name),
        dir: def.dir.clone(),
        ram_override: Some(def.dir.join("compose.ram.yaml")).filter(|p| limits.ram_data && p.is_file()),
        env: vec![
            ("BB_PORT".into(), vars.port.to_string()),
            ("BB_DATA".into(), vars.data.to_string_lossy().into_owned()),
            ("BB_PUBKEY".into(), vars.pubkey.clone()),
            ("BB_NPUB".into(), vars.npub()),
            ("BB_UID".into(), uid_gid()),
            ("BB_SERVER_CPUS".into(), cpu_list(limits.server_cpus)),
            ("BB_SERVICE_CPUS".into(), service_cpus),
            ("BB_MEMORY".into(), limits.memory.to_string()),
        ],
    }
}

pub fn build(def: &ServerDef, work: &Path, vars: &Vars, limits: &Limits<'_>, rebuild: bool) -> Result<()> {
    if def.url.is_some() {
        return Ok(());
    }
    let log = work.join(format!("{}-build.log", def.name));
    let _ = std::fs::remove_file(&log);
    let project = project(def, work, vars, limits);
    eprintln!("  building {}", def.name);
    let mut build = project.compose();
    build.env("DOCKER_BUILDKIT", "1").arg("build");
    if rebuild {
        build.arg("--no-cache");
    }
    run_cmd(&mut build, &log)?;
    run_cmd(project.compose().args(["pull", "--ignore-buildable", "-q"]), &log)?;
    Ok(())
}

fn copy_files(from: &Path, to: &Path, vars: &Vars) -> Result<()> {
    if !from.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_files(&entry.path(), &target, vars)?;
        } else {
            let text = std::fs::read_to_string(entry.path())?;
            std::fs::write(&target, vars.expand(&text))?;
        }
    }
    Ok(())
}

pub async fn start(def: &ServerDef, work: &Path, vars: &Vars, limits: &Limits<'_>) -> Result<Running> {
    let log = work.join(format!("{}.log", def.name));
    let _ = std::fs::remove_file(&log);
    if let Some(url) = &def.url {
        let hostport = url.trim_start_matches("http://").trim_end_matches('/');
        let addr: SocketAddr = tokio::net::lookup_host(hostport).await?.next().context("cannot resolve url")?;
        return Ok(Running { addr, roots: vec![], project: None, log });
    }

    let project = project(def, work, vars, limits);
    let _ = project.compose().args(["down", "-v", "--remove-orphans", "-t", "1"]).output();
    if vars.data.exists() {
        std::fs::remove_dir_all(&vars.data)?;
    }
    std::fs::create_dir_all(&vars.data)?;
    for d in &def.data_dirs {
        std::fs::create_dir_all(vars.data.join(vars.expand(d)))?;
    }
    copy_files(&def.dir.join("files"), &vars.data, vars)?;

    let timeout = def.startup_timeout_s.to_string();
    let up = project.compose().args(["up", "-d", "--wait", "--wait-timeout", &timeout]).output()?;
    let addr: SocketAddr = ([127, 0, 0, 1], vars.port).into();
    let mut running = Running { addr, roots: vec![], project: Some(project.clone()), log };
    if !up.status.success() {
        running.save_logs();
        let msg = String::from_utf8_lossy(&up.stderr).lines().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        running.stop().await;
        bail!("docker compose up failed: {msg}");
    }
    if let Err(e) = load::wait_ready(addr, &def.ready_path, Duration::from_secs(def.startup_timeout_s)).await {
        running.save_logs();
        let tail = std::fs::read_to_string(&running.log).unwrap_or_default();
        let tail: Vec<&str> = tail.lines().rev().take(15).collect();
        running.stop().await;
        bail!("{e}\n--- log tail ---\n{}", tail.into_iter().rev().collect::<Vec<_>>().join("\n"));
    }
    for hook in &def.after_ready {
        let out = project.compose().args(["exec", "-T", &hook.service, "sh", "-c", &vars.expand(&hook.cmd)]).output()?;
        if !out.status.success() {
            running.stop().await;
            bail!("after_ready on {} failed: {}", hook.service, String::from_utf8_lossy(&out.stderr));
        }
    }
    running.roots = project.containers().iter().filter_map(|c| container_pid(c)).collect();
    Ok(running)
}

impl Running {
    fn save_logs(&self) {
        let Some(project) = &self.project else { return };
        if let Ok(out) = project.compose().args(["logs", "--no-color", "--no-log-prefix"]).output() {
            let mut text = out.stdout;
            text.extend(out.stderr);
            let _ = std::fs::write(&self.log, text);
        }
    }

    pub async fn stop(&mut self) {
        self.save_logs();
        if let Some(project) = self.project.take() {
            let _ = project.compose().args(["down", "-v", "--remove-orphans", "-t", "1"]).output();
        }
    }
}
