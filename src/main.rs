mod config;
mod html;
mod load;
mod nostr;
mod report;
mod runner;
mod sys;
mod workloads;

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::report::{Report, ServerRun};
use crate::runner::Vars;
use crate::workloads::Ctx;

#[derive(Parser)]
#[command(about = "Throughput, latency, CPU and memory comparison of Blossom servers")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Run {
        #[arg(long, default_value = "suites/standard.toml")]
        suite: PathBuf,
        #[arg(long, default_value = "servers")]
        servers: PathBuf,
        #[arg(long = "server", short = 's')]
        only: Vec<String>,
        #[arg(long, default_value = "results")]
        out: PathBuf,
        #[arg(long, default_value = "/tmp/blossom-bench")]
        work: PathBuf,
        #[arg(long)]
        baseline: Option<String>,
        #[arg(long)]
        rebuild: bool,
        #[arg(long, help = "Run even if the benchmark CPUs are busy with other work")]
        allow_busy: bool,
        #[arg(long, help = "Keep every server's data dir, and its databases' data, in RAM under this tmpfs path (for example /dev/shm/blossom-bench), so no server waits on a disk and fsync is free for all")]
        ram_data: Option<PathBuf>,
        #[arg(long)]
        server_cpus: Option<String>,
        #[arg(long)]
        load_cpus: Option<String>,
        #[arg(long)]
        duration: Option<u64>,
        #[arg(long)]
        connections: Option<usize>,
    },
    Report {
        json: PathBuf,
        #[arg(long)]
        baseline: Option<String>,
        #[arg(long)]
        html: Option<PathBuf>,
    },
    List {
        #[arg(long, default_value = "servers")]
        servers: PathBuf,
    },
}

fn now() -> String {
    let out = std::process::Command::new("date").arg("+%Y-%m-%d %H:%M").output();
    out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::List { servers } => {
            for d in config::load_servers(&servers)? {
                let how = match &d.url {
                    Some(u) => format!("existing {u}"),
                    None => format!("compose {}", d.dir.join("compose.yaml").display()),
                };
                println!("{:<18} {}", d.name, how);
            }
            Ok(())
        }
        Cmd::Report { json, baseline, html } => {
            let rep: Report = serde_json::from_str(&std::fs::read_to_string(json)?)?;
            match html {
                Some(path) => std::fs::write(path, html::page(&rep))?,
                None => print!("{}", report::markdown(&rep, baseline.as_deref())),
            }
            Ok(())
        }
        Cmd::Run { suite, servers, only, out, work, baseline, rebuild, allow_busy, ram_data, server_cpus, load_cpus, duration, connections } => {
            let mut suite_def = config::load_suite(&suite)?;
            if let Some(v) = server_cpus {
                suite_def.run.server_cpus = v;
            }
            if let Some(v) = load_cpus {
                suite_def.run.load_cpus = v;
            }
            if let Some(v) = duration {
                suite_def.run.duration_s = v;
            }
            if let Some(v) = connections {
                suite_def.run.connections = v;
            }
            let run = suite_def.run.clone();
            let server_set = sys::parse_cpus(&run.server_cpus)?;
            let load_set = sys::parse_cpus(&run.load_cpus)?;
            let service_set = run.service_cpus.as_deref().map(sys::parse_cpus).transpose()?.unwrap_or_default();
            if server_set.iter().any(|c| load_set.contains(c)) {
                bail!("server_cpus and load_cpus overlap, which would make the results meaningless");
            }
            if server_set.iter().chain(&load_set).any(|&c| c >= sys::online_cpus()) {
                bail!("cpu list names a CPU this host does not have ({} online)", sys::online_cpus());
            }
            sys::set_affinity(&load_set).context("pinning the load generator")?;

            let mut defs = config::load_servers(&servers)?;
            if !only.is_empty() {
                for o in &only {
                    if !defs.iter().any(|d| &d.name == o) {
                        bail!("no server definition named {o}");
                    }
                }
                defs.retain(|d| only.contains(&d.name));
            }
            if defs.is_empty() {
                bail!("no servers selected");
            }
            std::fs::create_dir_all(&work)?;
            std::fs::create_dir_all(&out)?;
            let all_cpus: Vec<usize> = server_set.iter().chain(&service_set).chain(&load_set).copied().collect();
            let busy = sys::cpu_busy_percent(&all_cpus, std::time::Duration::from_secs(3));
            if busy > 10.0 {
                let msg = format!("the benchmark CPUs were {busy:.0}% busy with other work before the run started");
                if !allow_busy {
                    bail!("{msg}; results would be unreliable. Free the cores or pass --allow-busy");
                }
                eprintln!("warning: {msg}");
            }
            let data_root = match &ram_data {
                Some(path) => {
                    std::fs::create_dir_all(path)?;
                    sys::require_tmpfs(path)?;
                    path.clone()
                }
                None => work.join("data"),
            };

            let mut report = Report {
                when: now(),
                host_cpus: sys::online_cpus(),
                server_cpus: run.server_cpus.clone(),
                load_cpus: run.load_cpus.clone(),
                connections: run.connections,
                suite: suite.display().to_string(),
                ram_data: ram_data.is_some(),
                host_busy_pct: busy,
                servers: Vec::new(),
            };

            for (i, def) in defs.iter().enumerate() {
                eprintln!("== {}", def.name);
                let port = if def.port != 0 { def.port } else { 18100 + i as u16 };
                let signer = nostr::Signer::fixed()?;
                let vars = Vars { port, data: data_root.join(&def.name), pubkey: signer.pubkey_hex.clone() };
                let limits = runner::Limits { ram_data: ram_data.is_some(), server_cpus: &server_set, service_cpus: &service_set, memory: &run.server_memory };
                if let Err(e) = runner::build(def, &work, &vars, &limits, rebuild) {
                    eprintln!("  build failed: {e:#}");
                    report.servers.push(ServerRun { name: def.name.clone(), notes: def.notes.clone(), error: Some(format!("build failed: {e:#}")), results: vec![] });
                    continue;
                }
                let mut running = match runner::start(def, &work, &vars, &limits).await {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("  start failed: {e:#}");
                        report.servers.push(ServerRun { name: def.name.clone(), notes: def.notes.clone(), error: Some(format!("start failed: {e:#}")), results: vec![] });
                        continue;
                    }
                };
                let mut def_local = def.clone();
                if def_local.url.is_some() {
                    def_local.port = running.addr.port();
                }
                let mut ctx = Ctx {
                    def: &def_local,
                    run: &run,
                    addr: running.addr,
                    roots: running.roots.clone(),
                    signer,
                    load_cpus: load_set.len(),
                    blobs: HashMap::new(),
                };
                let mut results = Vec::new();
                let mut error = None;
                for (wi, w) in suite_def.workloads.iter().enumerate() {
                    eprint!("  {:<22}", w.name);
                    match workloads::execute(&mut ctx, w, run.seed.wrapping_add(wi as u64)).await {
                        Ok(r) => {
                            match &r.skipped {
                                Some(why) => eprintln!(" skipped: {why}"),
                                None => eprintln!(
                                    " {:>9.0}/s ok={}/{} p99={}us cpu={:.2}{}",
                                    r.ok_per_s, r.ok, r.requests, r.p99_us, r.cpu_cores,
                                    if r.disqualified { "  [DQ]" } else { "" }
                                ),
                            }
                            results.push(r);
                        }
                        Err(e) => {
                            eprintln!(" error: {e:#}");
                            error = Some(format!("{}: {e:#}", w.name));
                            break;
                        }
                    }
                }
                running.stop().await;
                if ram_data.is_some() {
                    let _ = std::fs::remove_dir_all(&vars.data);
                }
                report.servers.push(ServerRun { name: def.name.clone(), notes: def.notes.clone(), error, results });
            }

            let latest = if ram_data.is_some() { "latest-ram" } else { "latest" };
            let stamp = report.when.replace([' ', ':'], "-");
            let json_path = out.join(format!("{stamp}.json"));
            std::fs::write(&json_path, serde_json::to_string_pretty(&report)?)?;
            let md = report::markdown(&report, baseline.as_deref());
            std::fs::write(out.join(format!("{stamp}.md")), &md)?;
            std::fs::write(out.join(format!("{latest}.md")), &md)?;
            let page = html::page(&report);
            std::fs::write(out.join(format!("{stamp}.html")), &page)?;
            std::fs::write(out.join(format!("{latest}.html")), &page)?;
            println!("{md}");
            eprintln!("wrote {}", json_path.display());
            Ok(())
        }
    }
}
