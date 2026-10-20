use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::{ServerDef, Source};
use crate::load;

pub struct Vars {
    pub port: u16,
    pub data: PathBuf,
    pub src: PathBuf,
    pub pubkey: String,
}

impl Vars {
    pub fn expand(&self, text: &str) -> String {
        text.replace("{port}", &self.port.to_string())
            .replace("{data}", &self.data.to_string_lossy())
            .replace("{src}", &self.src.to_string_lossy())
            .replace("{pubkey}", &self.pubkey)
    }
}

const IGNORE: &str = "target\n.git\nnode_modules\n*.log\n";

pub struct Running {
    pub addr: SocketAddr,
    pub roots: Vec<u32>,
    server: Option<String>,
    containers: Vec<String>,
    pub log: PathBuf,
}


fn expand_home(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", std::env::var("HOME").unwrap_or_default()),
        None => p.to_string(),
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

pub fn image_tag(def: &ServerDef) -> String {
    format!("bb-{}:bench", def.name)
}

pub fn source_dir(def: &ServerDef, work: &Path, log: &Path) -> Result<PathBuf> {
    let Some(src) = &def.source else {
        let empty = work.join("empty-context");
        std::fs::create_dir_all(&empty)?;
        return Ok(empty);
    };
    if let Some(local) = &src.path {
        return Ok(PathBuf::from(expand_home(local)));
    }
    let git = src.git.as_ref().context("source needs git or path")?;
    let dir = work.join("src").join(&def.name);
    std::fs::create_dir_all(work.join("src"))?;
    if !dir.join(".git").exists() {
        run_cmd(StdCommand::new("git").args(["clone", "-q", git]).arg(&dir), log)?;
    }
    checkout(src, &dir, log)?;
    Ok(dir)
}

fn checkout(src: &Source, dir: &Path, log: &Path) -> Result<()> {
    if let Some(rev) = &src.rev {
        run_cmd(StdCommand::new("git").args(["fetch", "-q", "--tags", "origin"]).current_dir(dir), log)?;
        run_cmd(StdCommand::new("git").args(["checkout", "-q", rev]).current_dir(dir), log)?;
    }
    Ok(())
}

pub fn build_image(def: &ServerDef, work: &Path, rebuild: bool) -> Result<PathBuf> {
    let log = work.join(format!("{}-build.log", def.name));
    let _ = std::fs::remove_file(&log);
    let image = def.image.clone().unwrap_or_default();
    if let Some(prebuilt) = &image.prebuilt {
        run_cmd(StdCommand::new("docker").args(["pull", "-q", prebuilt]), &log)?;
        run_cmd(StdCommand::new("docker").args(["tag", prebuilt, &image_tag(def)]), &log)?;
        return Ok(PathBuf::new());
    }
    let ctx = source_dir(def, work, &log)?;
    let dockerfile = match (&image.dockerfile_inline, &image.dockerfile) {
        (Some(text), _) => {
            let dir = work.join("dockerfiles");
            std::fs::create_dir_all(&dir)?;
            let path = dir.join(format!("{}.Dockerfile", def.name));
            std::fs::write(&path, text)?;
            std::fs::write(dir.join(format!("{}.Dockerfile.dockerignore", def.name)), IGNORE)?;
            path
        }
        (None, Some(rel)) => ctx.join(rel),
        (None, None) => bail!("image needs prebuilt, dockerfile or dockerfile_inline"),
    };
    eprintln!("  building image {}", image_tag(def));
    let mut cmd = StdCommand::new("docker");
    cmd.env("DOCKER_BUILDKIT", "1").args(["build", "-q", "-t", &image_tag(def), "-f"]).arg(&dockerfile);
    if rebuild {
        cmd.arg("--no-cache");
    }
    for (k, v) in &image.build_args {
        cmd.arg("--build-arg").arg(format!("{k}={v}"));
    }
    cmd.arg(&ctx);
    run_cmd(&mut cmd, &log)?;
    Ok(ctx)
}

pub struct Limits<'a> {
    pub server_cpus: &'a [usize],
    pub service_cpus: &'a [usize],
    pub memory: &'a str,
}

fn cpu_list(cpus: &[usize]) -> String {
    cpus.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(",")
}

fn container_pid(name: &str) -> Option<u32> {
    let out = StdCommand::new("docker").args(["inspect", "-f", "{{.State.Pid}}", name]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok().filter(|p| *p != 0)
}

fn uid_gid() -> String {
    unsafe { format!("{}:{}", libc::getuid(), libc::getgid()) }
}

fn base_args(name: &str, cpus: &[usize], memory: &str) -> Vec<String> {
    let mut a: Vec<String> = ["run", "-d", "--name", name, "--network", "host"].iter().map(|s| s.to_string()).collect();
    if !cpus.is_empty() {
        a.push("--cpuset-cpus".into());
        a.push(cpu_list(cpus));
    }
    a.extend(["--memory".into(), memory.into(), "--memory-swap".into(), memory.into()]);
    a.extend(["--ulimit".into(), "nofile=1048576:1048576".into()]);
    a
}

pub async fn start(def: &ServerDef, work: &Path, vars: &Vars, limits: &Limits<'_>) -> Result<Running> {
    let log = work.join(format!("{}.log", def.name));
    let _ = std::fs::remove_file(&log);
    if let Some(url) = &def.url {
        let hostport = url.trim_start_matches("http://").trim_end_matches('/');
        let addr: SocketAddr = tokio::net::lookup_host(hostport).await?.next().context("cannot resolve url")?;
        return Ok(Running { addr, roots: vec![], server: None, containers: vec![], log });
    }

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

    let mut containers = Vec::new();
    let mut roots = Vec::new();
    for svc in &def.services {
        let cname = format!("bb-{}-{}", def.name, svc.name);
        let _ = StdCommand::new("docker").args(["rm", "-f", &cname]).output();
        let mut args = base_args(&cname, limits.service_cpus, limits.memory);
        for (k, v) in &svc.env {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        args.push(svc.image.clone());
        args.extend(svc.args.iter().map(|a| vars.expand(a)));
        let out = StdCommand::new("docker").args(&args).output()?;
        if !out.status.success() {
            bail!("docker run {}: {}", svc.name, String::from_utf8_lossy(&out.stderr));
        }
        containers.push(cname.clone());
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        loop {
            let ok = StdCommand::new("docker")
                .args(["exec", &cname, "sh", "-c", &vars.expand(&svc.ready_cmd)])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                break;
            }
            if std::time::Instant::now() >= deadline {
                for c in &containers {
                    let _ = StdCommand::new("docker").args(["rm", "-f", c]).output();
                }
                bail!("service {} not ready", svc.name);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        if let Some(p) = container_pid(&cname) {
            roots.push(p);
        }
    }

    let sname = format!("bb-{}", def.name);
    let _ = StdCommand::new("docker").args(["rm", "-f", &sname]).output();
    let mut args = base_args(&sname, limits.server_cpus, limits.memory);
    args.extend(["--user".into(), uid_gid()]);
    args.extend(["-v".into(), format!("{0}:{0}", vars.data.display()), "-w".into(), vars.data.display().to_string()]);
    for m in &def.run.mounts {
        args.push("-v".into());
        args.push(vars.expand(m));
    }
    for (k, v) in &def.run.env {
        args.push("-e".into());
        args.push(format!("{k}={}", vars.expand(v)));
    }
    if let Some(e) = &def.run.entrypoint {
        args.extend(["--entrypoint".into(), e.clone()]);
    }
    args.push(image_tag(def));
    args.extend(def.run.cmd.iter().map(|a| vars.expand(a)));
    let out = StdCommand::new("docker").args(&args).output()?;
    if !out.status.success() {
        for c in &containers {
            let _ = StdCommand::new("docker").args(["rm", "-f", c]).output();
        }
        bail!("docker run {}: {}", def.name, String::from_utf8_lossy(&out.stderr));
    }
    if let Some(p) = container_pid(&sname) {
        roots.insert(0, p);
    }
    let addr: SocketAddr = ([127, 0, 0, 1], vars.port).into();
    let mut running = Running { addr, roots, server: Some(sname), containers, log };
    if let Err(e) = load::wait_ready(addr, &def.ready_path, Duration::from_secs(def.run.startup_timeout_s)).await {
        running.save_logs();
        let tail = std::fs::read_to_string(&running.log).unwrap_or_default();
        let tail: Vec<&str> = tail.lines().rev().take(15).collect();
        running.stop().await;
        bail!("{e}\n--- log tail ---\n{}", tail.into_iter().rev().collect::<Vec<_>>().join("\n"));
    }
    for hook in &def.after_ready {
        let cname = format!("bb-{}-{}", def.name, hook.service);
        let out = StdCommand::new("docker").args(["exec", &cname, "sh", "-c", &vars.expand(&hook.cmd)]).output()?;
        if !out.status.success() {
            running.stop().await;
            bail!("after_ready on {} failed: {}", hook.service, String::from_utf8_lossy(&out.stderr));
        }
    }
    Ok(running)
}

impl Running {
    fn save_logs(&self) {
        let Some(name) = &self.server else { return };
        if let Ok(out) = StdCommand::new("docker").args(["logs", name]).output() {
            let mut text = out.stdout;
            text.extend(out.stderr);
            let _ = std::fs::write(&self.log, text);
        }
    }

    pub async fn stop(&mut self) {
        self.save_logs();
        if let Some(name) = self.server.take() {
            let _ = StdCommand::new("docker").args(["rm", "-f", &name]).output();
        }
        for c in self.containers.drain(..) {
            let _ = StdCommand::new("docker").args(["rm", "-f", &c]).output();
        }
    }
}
