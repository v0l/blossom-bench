use std::fmt::Write;

use serde::{Deserialize, Serialize};

use crate::workloads::WorkloadResult;

#[derive(Serialize, Deserialize, Clone)]
pub struct ServerRun {
    pub name: String,
    pub notes: String,
    pub error: Option<String>,
    pub results: Vec<WorkloadResult>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Report {
    pub when: String,
    pub host_cpus: usize,
    pub server_cpus: String,
    pub load_cpus: String,
    pub connections: usize,
    pub suite: String,
    pub servers: Vec<ServerRun>,
}

fn rate(v: f64) -> String {
    if v >= 100_000.0 {
        format!("{:.0}k", v / 1000.0)
    } else if v >= 10_000.0 {
        format!("{:.1}k", v / 1000.0)
    } else if v >= 1000.0 {
        format!("{:.2}k", v / 1000.0)
    } else {
        format!("{v:.0}")
    }
}

fn ratio(x: f64) -> String {
    if x >= 10.0 {
        format!("{x:.0}x")
    } else if x >= 0.1 {
        format!("{x:.2}x")
    } else {
        format!("{x:.3}x")
    }
}

fn bw(mb: f64) -> String {
    if mb >= 1000.0 { format!("{:.1} GB/s", mb / 1000.0) } else { format!("{mb:.0} MB/s") }
}

fn lat(us: u64) -> String {
    if us >= 1_000_000 {
        format!("{:.1} s", us as f64 / 1e6)
    } else if us >= 1000 {
        format!("{:.1} ms", us as f64 / 1e3)
    } else {
        format!("{us} us")
    }
}

fn find<'a>(s: &'a ServerRun, name: &str) -> Option<&'a WorkloadResult> {
    s.results.iter().find(|r| r.name == name)
}

fn failure_rate(r: &WorkloadResult) -> f64 {
    if r.requests == 0 { 0.0 } else { 1.0 - r.ok as f64 / r.requests as f64 }
}

fn cell_rate(r: Option<&WorkloadResult>, base: Option<&WorkloadResult>) -> String {
    let Some(r) = r else { return "n/a".into() };
    if r.disqualified {
        return "DQ".into();
    }
    if let Some(s) = &r.skipped {
        return format!("skipped ({s})");
    }
    let mut out = if r.kind == "upload" || r.kind == "get" {
        format!("{}/s ({})", rate(r.ok_per_s), bw(r.mb_per_s))
    } else {
        format!("{}/s", rate(r.ok_per_s))
    };
    let fail = failure_rate(r);
    if fail > 0.001 {
        out.push_str(&format!(", {:.0}% failed", fail * 100.0));
    }
    if r.verify_bad > 0 {
        out.push_str(&format!(", {} of {} readbacks wrong", r.verify_bad, r.verified));
    }
    if r.bad_length > 0 {
        out.push_str(&format!(", {} short bodies", r.bad_length));
    }
    if let Some(b) = base {
        if b.skipped.is_none() && b.ok_per_s > 0.0 && !std::ptr::eq(r, b) {
            out.push_str(&format!(" {}", ratio(r.ok_per_s / b.ok_per_s)));
        }
    }
    out
}

pub fn markdown(rep: &Report, baseline: Option<&str>) -> String {
    let mut s = String::new();
    let base_idx = baseline
        .and_then(|b| rep.servers.iter().position(|x| x.name == b))
        .unwrap_or(0);
    let _ = writeln!(s, "# Blossom benchmark\n");
    let _ = writeln!(
        s,
        "{}. Suite `{}`. Servers on CPUs {}, load generator on CPUs {}, {} connections, host has {} CPUs.\n",
        rep.when, rep.suite, rep.server_cpus, rep.load_cpus, rep.connections, rep.host_cpus
    );
    let _ = writeln!(s, "Ratios are against `{}`.\n", rep.servers[base_idx].name);
    let mut names: Vec<String> = Vec::new();
    for sv in &rep.servers {
        for r in &sv.results {
            if !names.contains(&r.name) {
                names.push(r.name.clone());
            }
        }
    }
    let header = |title: &str| {
        let mut h = format!("| {title} |");
        let mut sep = "|---|".to_string();
        for sv in &rep.servers {
            h.push_str(&format!(" {} |", sv.name));
            sep.push_str("---:|");
        }
        format!("{h}\n{sep}\n")
    };

    let _ = writeln!(s, "## Throughput\n");
    s.push_str(&header("workload"));
    for n in &names {
        let base = find(&rep.servers[base_idx], n);
        let _ = write!(s, "| {n} |");
        for sv in &rep.servers {
            let _ = write!(s, " {} |", cell_rate(find(sv, n), base));
        }
        s.push('\n');
    }

    let _ = writeln!(s, "\n## Latency (p50 / p99 / p99.9)\n");
    s.push_str(&header("workload"));
    for n in &names {
        let _ = write!(s, "| {n} |");
        for sv in &rep.servers {
            let cell = match find(sv, n) {
                Some(r) if r.skipped.is_none() && r.requests > 0 => {
                    format!("{} / {} / {}", lat(r.p50_us), lat(r.p99_us), lat(r.p999_us))
                }
                _ => "n/a".into(),
            };
            let _ = write!(s, " {cell} |");
        }
        s.push('\n');
    }

    let _ = writeln!(s, "\n## CPU cores used by the server and its database, and requests per CPU-second\n");
    s.push_str(&header("workload"));
    for n in &names {
        let _ = write!(s, "| {n} |");
        for sv in &rep.servers {
            let cell = match find(sv, n) {
                Some(r) if r.skipped.is_none() && r.cpu_cores > 0.0 => {
                    format!("{:.2} cores, {}/cpu-s", r.cpu_cores, rate(r.ok_per_cpu_s))
                }
                _ => "n/a".into(),
            };
            let _ = write!(s, " {cell} |");
        }
        s.push('\n');
    }

    let _ = writeln!(s, "\n## Peak memory of the server and its database (anonymous / file-backed)\n");
    s.push_str(&header("workload"));
    for n in &names {
        let _ = write!(s, "| {n} |");
        for sv in &rep.servers {
            let cell = match find(sv, n) {
                Some(r) if r.skipped.is_none() && r.peak_anon_mb > 0.0 => {
                    format!("{:.0} MB / {:.0} MB", r.peak_anon_mb, r.peak_file_mb)
                }
                _ => "n/a".into(),
            };
            let _ = write!(s, " {cell} |");
        }
        s.push('\n');
    }

    let mut notes = Vec::new();
    for sv in &rep.servers {
        if let Some(e) = &sv.error {
            notes.push(format!("- `{}` failed: {}", sv.name, e.lines().next().unwrap_or("")));
        }
        if !sv.notes.is_empty() {
            notes.push(format!("- `{}`: {}", sv.name, sv.notes));
        }
        for r in &sv.results {
            if let Some(e) = &r.error_sample {
                notes.push(format!("- `{}` / {}: first failure was {}", sv.name, r.name, e));
            }
            if let Some(why) = &r.offloaded {
                notes.push(format!("- `{}` / {}: offloads work, {}", sv.name, r.name, why));
            }
            if r.skipped.is_none() && r.load_gen_util > 0.85 {
                notes.push(format!(
                    "- `{}` / {}: load generator was {:.0}% busy, so this number may be the generator's limit",
                    sv.name,
                    r.name,
                    r.load_gen_util * 100.0
                ));
            }
        }
    }
    if !notes.is_empty() {
        let _ = writeln!(s, "\n## Notes\n");
        for n in notes {
            let _ = writeln!(s, "{n}");
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(name: &str, ok_per_s: f64) -> WorkloadResult {
        WorkloadResult {
            name: name.into(),
            kind: "get".into(),
            requests: 100,
            ok: 100,
            ok_per_s,
            ..Default::default()
        }
    }

    fn server(name: &str, rates: &[(&str, f64)]) -> ServerRun {
        ServerRun {
            name: name.into(),
            notes: String::new(),
            error: None,
            results: rates.iter().map(|(n, r)| result(n, *r)).collect(),
        }
    }

    fn report(servers: Vec<ServerRun>) -> Report {
        Report {
            when: "now".into(),
            host_cpus: 8,
            server_cpus: "0-3".into(),
            load_cpus: "4-7".into(),
            connections: 64,
            suite: "t".into(),
            servers,
        }
    }

    #[test]
    fn ratios_are_against_the_named_baseline() {
        let rep = report(vec![server("a", &[("get", 1000.0)]), server("b", &[("get", 5000.0)])]);
        let md = markdown(&rep, Some("a"));
        assert!(md.contains("5.00x"), "{md}");
        let md = markdown(&rep, Some("b"));
        assert!(md.contains("0.20x"), "{md}");
    }

    #[test]
    fn failed_requests_and_bad_readbacks_are_called_out() {
        let mut bad = server("a", &[("get", 1000.0)]);
        bad.results[0].ok = 60;
        bad.results[0].verify_bad = 2;
        bad.results[0].verified = 8;
        let md = markdown(&report(vec![bad]), None);
        assert!(md.contains("40% failed"), "{md}");
        assert!(md.contains("2 of 8 readbacks wrong"), "{md}");
    }

    #[test]
    fn server_errors_and_saturated_load_generator_are_noted() {
        let mut broken = server("a", &[]);
        broken.error = Some("start failed: boom\nmore".into());
        let mut busy = server("b", &[("get", 10.0)]);
        busy.results[0].load_gen_util = 0.95;
        let md = markdown(&report(vec![broken, busy]), None);
        assert!(md.contains("`a` failed: start failed: boom"), "{md}");
        assert!(md.contains("95% busy"), "{md}");
    }

    #[test]
    fn small_ratios_keep_precision() {
        assert_eq!(ratio(0.002), "0.002x");
        assert_eq!(ratio(77.4), "77x");
    }
}
