use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::process::{Child, Command};

use crate::config::{ServerDef, Source};
use crate::load;

pub struct Vars {
    pub port: u16,
    pub data: PathBuf,
    pub src: PathBuf,
    pub bin: PathBuf,
}

impl Vars {
    pub fn expand(&self, text: &str) -> String {
        text.replace("{port}", &self.port.to_string())
            .replace("{data}", &self.data.to_string_lossy())
            .replace("{src}", &self.src.to_string_lossy())
            .replace("{bin}", &self.bin.to_string_lossy())
    }
}

pub struct Running {
    pub addr: SocketAddr,
    pub roots: Vec<u32>,
    child: Option<Child>,
    containers: Vec<String>,
    pub log: PathBuf,
}

async fn sh(cmd: &str, cwd: &Path, env: &HashMap<String, String>, log: &Path) -> Result<()> {
    let out = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    let status = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .envs(env)
        .stdout(Stdio::from(out.try_clone()?))
        .stderr(Stdio::from(out))
        .status()
        .await?;
    if !status.success() {
        bail!("`{cmd}` failed ({status}); see {}", log.display());
    }
    Ok(())
}

pub async fn prepare_source(def: &ServerDef, cache: &Path, rebuild: bool) -> Result<(PathBuf, PathBuf)> {
    let Some(src) = &def.source else {
        return Ok((PathBuf::new(), PathBuf::new()));
    };
    let dir = cache.join("src").join(&def.name);
    std::fs::create_dir_all(cache.join("src"))?;
    let log = cache.join(format!("{}-build.log", def.name));
    if let Some(local) = &src.path {
        let p = PathBuf::from(shellexpand(local));
        build(src, &p, &p, &log, rebuild, &def.name).await?;
        return Ok((p.clone(), bin_path(src, &p)));
    }
    let git = src.git.as_ref().context("source needs git or path")?;
    if !dir.join(".git").exists() {
        sh(&format!("git clone -q {git} {}", dir.display()), cache, &HashMap::new(), &log).await?;
    }
    if let Some(rev) = &src.rev {
        sh(&format!("git fetch -q --tags origin && git checkout -q {rev}"), &dir, &HashMap::new(), &log).await?;
    }
    build(src, &dir, &dir, &log, rebuild, &def.name).await?;
    Ok((dir.clone(), bin_path(src, &dir)))
}

fn shellexpand(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", std::env::var("HOME").unwrap_or_default()),
        None => p.to_string(),
    }
}

fn bin_path(src: &Source, dir: &Path) -> PathBuf {
    src.bin.as_ref().map(|b| dir.join(b)).unwrap_or_default()
}

async fn build(src: &Source, dir: &Path, _src_dir: &Path, log: &Path, rebuild: bool, name: &str) -> Result<()> {
    let stamp = dir.join(".blossom-bench-built");
    let rev = {
        let out = std::process::Command::new("git").args(["rev-parse", "HEAD"]).current_dir(dir).output();
        out.ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
    };
    let key = format!("{rev}\n{}", src.build.join("\n"));
    if !rebuild && std::fs::read_to_string(&stamp).ok().as_deref() == Some(&key) {
        return Ok(());
    }
    eprintln!("  building {name}");
    for cmd in &src.build {
        sh(cmd, dir, &src.env, log).await?;
    }
    std::fs::write(&stamp, key)?;
    Ok(())
}

pub async fn start(def: &ServerDef, work: &Path, vars: &Vars, server_cpus: &[usize], service_cpus: &[usize]) -> Result<Running> {
    let log = work.join(format!("{}.log", def.name));
    let _ = std::fs::remove_file(&log);
    if let Some(url) = &def.url {
        let hostport = url.trim_start_matches("http://").trim_end_matches('/');
        let addr: SocketAddr = tokio::net::lookup_host(hostport)
            .await?
            .next()
            .context("cannot resolve url")?;
        return Ok(Running { addr, roots: vec![], child: None, containers: vec![], log });
    }
    let run = def.run.as_ref().context("server needs a run section or a url")?;

    if vars.data.exists() {
        std::fs::remove_dir_all(&vars.data)?;
    }
    std::fs::create_dir_all(&vars.data)?;
    for f in &def.files {
        let path = PathBuf::from(vars.expand(&f.path));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, vars.expand(&f.content))?;
    }

    for cmd in &def.setup {
        sh(&vars.expand(cmd), &vars.data, &HashMap::new(), &log).await?;
    }

    let mut containers = Vec::new();
    let mut service_pids: Vec<String> = Vec::new();
    for svc in &def.services {
        let cname = format!("bb-{}-{}", def.name, svc.name);
        let _ = std::process::Command::new("docker").args(["rm", "-f", &cname]).output();
        let mut args: Vec<String> = vec!["run".into(), "-d".into(), "--name".into(), cname.clone()];
        if !service_cpus.is_empty() {
            args.push("--cpuset-cpus".into());
            args.push(cpu_list(service_cpus));
        }
        for p in &svc.publish {
            args.push("-p".into());
            args.push(format!("127.0.0.1:{p}"));
        }
        for (k, v) in &svc.env {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        args.push(svc.image.clone());
        let out = std::process::Command::new("docker").args(&args).output()?;
        if !out.status.success() {
            bail!("docker run {}: {}", svc.name, String::from_utf8_lossy(&out.stderr));
        }
        containers.push(cname.clone());
        service_pids.push(cname.clone());
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        loop {
            let ok = std::process::Command::new("docker")
                .args(["exec", &cname, "sh", "-c", &svc.ready_cmd])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                break;
            }
            if std::time::Instant::now() >= deadline {
                bail!("service {} not ready", svc.name);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    let cmd: Vec<String> = run.cmd.iter().map(|a| vars.expand(a)).collect();
    let cwd = run.cwd.as_ref().map(|c| PathBuf::from(vars.expand(c))).unwrap_or_else(|| vars.data.clone());
    std::fs::create_dir_all(&cwd)?;
    let out = std::fs::File::create(&log)?;
    let cpus = server_cpus.to_vec();
    let mut command = Command::new(&cmd[0]);
    command
        .args(&cmd[1..])
        .current_dir(&cwd)
        .envs(run.env.iter().map(|(k, v)| (k.clone(), vars.expand(v))))
        .stdin(Stdio::null())
        .stdout(Stdio::from(out.try_clone()?))
        .stderr(Stdio::from(out))
        .process_group(0);
    unsafe {
        command.pre_exec(move || crate::sys::set_affinity(&cpus));
    }
    let child = command.spawn().with_context(|| format!("spawning {:?}", cmd))?;
    let pid = child.id().context("no pid")?;
    let addr: SocketAddr = ([127, 0, 0, 1], vars.port).into();
    let mut roots = vec![pid];
    for cname in &service_pids {
        let out = std::process::Command::new("docker")
            .args(["inspect", "-f", "{{.State.Pid}}", cname])
            .output()?;
        if let Ok(p) = String::from_utf8_lossy(&out.stdout).trim().parse::<u32>() {
            roots.push(p);
        }
    }
    let mut running = Running { addr, roots, child: Some(child), containers, log };
    if let Err(e) = load::wait_ready(addr, &def.ready_path, Duration::from_secs(run.startup_timeout_s)).await {
        let tail = std::fs::read_to_string(&running.log).unwrap_or_default();
        let tail: Vec<&str> = tail.lines().rev().take(15).collect();
        running.stop().await;
        bail!("{e}\n--- log tail ---\n{}", tail.into_iter().rev().collect::<Vec<_>>().join("\n"));
    }
    Ok(running)
}

fn cpu_list(cpus: &[usize]) -> String {
    cpus.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(",")
}

impl Running {
    pub async fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if let Some(pid) = child.id() {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGTERM);
                }
            }
            if tokio::time::timeout(Duration::from_secs(5), child.wait()).await.is_err() {
                if let Some(pid) = child.id() {
                    unsafe {
                        libc::kill(-(pid as i32), libc::SIGKILL);
                    }
                }
                let _ = child.wait().await;
            }
        }
        for c in self.containers.drain(..) {
            let _ = std::process::Command::new("docker").args(["rm", "-f", &c]).output();
        }
    }
}
