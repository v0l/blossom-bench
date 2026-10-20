use std::fmt::Write;

use crate::report::{Report, ServerRun};
use crate::workloads::WorkloadResult;

const GROUPS: [(&str, &[&str]); 3] = [
    ("Uploads", &["put-1k", "put-64k", "put-1m"]),
    ("Reads", &["get-1k", "get-64k", "get-1m"]),
    ("Lookups", &["head-1k", "miss", "get-hot-1k"]),
];

struct View {
    key: &'static str,
    label: &'static str,
    unit: &'static str,
    higher_is_better: bool,
    value: fn(&WorkloadResult) -> f64,
    show: fn(f64) -> String,
}

const VIEWS: [View; 3] = [
    View {
        key: "rps",
        label: "Requests per second",
        unit: "successful requests per second, log scale",
        higher_is_better: true,
        value: |r| r.ok_per_s,
        show: rate,
    },
    View {
        key: "cpu",
        label: "Per CPU core",
        unit: "successful requests per CPU-second, database included, log scale",
        higher_is_better: true,
        value: |r| r.ok_per_cpu_s,
        show: rate,
    },
    View {
        key: "p99",
        label: "p99 latency",
        unit: "99th percentile latency, log scale, shorter is better",
        higher_is_better: false,
        value: |r| r.p99_us as f64,
        show: |us| lat(us as u64),
    },
];

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
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

fn lat(us: u64) -> String {
    if us >= 1_000_000 {
        format!("{:.1} s", us as f64 / 1e6)
    } else if us >= 1000 {
        format!("{:.1} ms", us as f64 / 1e3)
    } else {
        format!("{us} us")
    }
}

fn times(x: f64) -> String {
    if x >= 10.0 {
        format!("{x:.0}x")
    } else if x >= 0.1 {
        format!("{x:.2}x")
    } else {
        format!("{x:.3}x")
    }
}

fn fail_rate(r: &WorkloadResult) -> f64 {
    if r.requests == 0 { 0.0 } else { 1.0 - r.ok as f64 / r.requests as f64 }
}

fn find<'a>(s: &'a ServerRun, name: &str) -> Option<&'a WorkloadResult> {
    s.results.iter().find(|r| r.name == name).filter(|r| r.skipped.is_none())
}

fn problems(r: &WorkloadResult) -> Vec<String> {
    let mut p = Vec::new();
    let f = fail_rate(r);
    if f > 0.001 {
        p.push(if f < 0.01 { format!("{:.1}% failed", f * 100.0) } else { format!("{:.0}% failed", f * 100.0) });
    }
    if r.verify_bad > 0 {
        p.push(format!("{} of {} readbacks wrong", r.verify_bad, r.verified));
    }
    if r.bad_length > 0 {
        p.push(format!("{} short bodies", r.bad_length));
    }
    p
}

fn log_axis(values: &[f64]) -> (f64, f64) {
    let positive: Vec<f64> = values.iter().copied().filter(|v| *v > 0.0).collect();
    let min = positive.iter().copied().fold(f64::MAX, f64::min);
    let max = positive.iter().copied().fold(0.0, f64::max);
    if positive.is_empty() {
        return (1.0, 10.0);
    }
    let lo = 10f64.powf((min * 0.7).log10().floor());
    let mut hi = 10f64.powf(max.log10().ceil());
    if hi <= lo * 1.5 {
        hi = lo * 10.0;
    }
    (lo, hi)
}

fn log_frac(v: f64, lo: f64, hi: f64) -> f64 {
    if v <= 0.0 {
        return 0.0;
    }
    ((v.log10() - lo.log10()) / (hi.log10() - lo.log10())).clamp(0.0, 1.0)
}

fn tick_label(view: &View, v: f64) -> String {
    if view.key == "p99" {
        if v >= 1e6 {
            format!("{}s", v / 1e6)
        } else if v >= 1e3 {
            format!("{}ms", v / 1e3)
        } else {
            format!("{v}us")
        }
    } else if v >= 1e6 {
        format!("{}M", v / 1e6)
    } else if v >= 1e3 {
        format!("{}k", v / 1e3)
    } else {
        format!("{v}")
    }
}

fn geomean_order(rep: &Report, base_idx: usize) -> Vec<usize> {
    let score = |i: usize| -> f64 {
        let mut logs = Vec::new();
        for r in &rep.servers[i].results {
            if r.skipped.is_some() || r.ok_per_s <= 0.0 {
                continue;
            }
            if let Some(b) = find(&rep.servers[base_idx], &r.name).filter(|b| b.ok_per_s > 0.0) {
                logs.push((r.ok_per_s / b.ok_per_s).ln());
            }
        }
        if logs.is_empty() { f64::MIN } else { logs.iter().sum::<f64>() / logs.len() as f64 }
    };
    let mut idx: Vec<usize> = (0..rep.servers.len()).collect();
    idx.sort_by(|a, b| score(*b).partial_cmp(&score(*a)).unwrap_or(std::cmp::Ordering::Equal));
    idx
}

fn panel(o: &mut String, rep: &Report, order: &[usize], base_idx: usize, workload: &str, view: &View) {
    let rows: Vec<(usize, Option<&WorkloadResult>)> = order.iter().map(|&i| (i, find(&rep.servers[i], workload))).collect();
    let values: Vec<f64> = rows.iter().filter_map(|(_, r)| r.map(view.value)).collect();
    let (lo, hi) = log_axis(&values);
    let base = find(&rep.servers[base_idx], workload).map(view.value).filter(|v| *v > 0.0);

    let mut ticks = Vec::new();
    let mut t = lo;
    while t <= hi * 1.0001 {
        ticks.push(t);
        t *= 10.0;
    }

    let _ = write!(o, "<section class=panel><h3>{}</h3>", esc(workload));
    let mut failures: Vec<String> = Vec::new();
    let base_line = base
        .map(|b| format!("<s style=\"left:{:.2}%\"></s>", log_frac(b, lo, hi) * 100.0))
        .unwrap_or_default();
    for (i, r) in &rows {
        let name = esc(&rep.servers[*i].name);
        let is_base = *i == base_idx;
        let Some(r) = r else {
            let _ = write!(o, "<div class=row><span class=name>{name}</span><span class=track></span><span class=val>n/a</span><span class=x></span></div>");
            continue;
        };
        let v = (view.value)(r);
        let probs = problems(r);
        if !probs.is_empty() {
            failures.push(format!("{}: {}", rep.servers[*i].name, probs.join(", ")));
        }
        let width = if v > 0.0 { (log_frac(v, lo, hi) * 100.0).max(1.5) } else { 0.0 };
        let mut class = String::from("bar");
        if is_base {
            class.push_str(" base");
        }
        if !probs.is_empty() {
            class.push_str(" hatch");
        }
        let ratio = match base {
            Some(b) if !is_base && v > 0.0 => {
                let x = v / b;
                let good = if view.higher_is_better { x >= 1.0 } else { x <= 1.0 };
                format!("<span class=\"x {}\">{}</span>", if good { "up" } else { "down" }, times(x))
            }
            _ => "<span class=x></span>".to_string(),
        };
        let _ = write!(
            o,
            "<div class=\"row{}\"><span class=name>{name}</span><span class=track>{grid}{base_line}<i class=\"{class}\" style=\"width:{width:.2}%\"></i></span><span class=val>{}{}</span>{ratio}</div>",
            if is_base { " is-base" } else { "" },
            (view.show)(v),
            if probs.is_empty() { "" } else { "<b class=flag>!</b>" },
            base_line = base_line,
            grid = ticks
                .iter()
                .map(|t| format!("<u style=\"left:{:.2}%\"></u>", log_frac(*t, lo, hi) * 100.0))
                .collect::<String>(),
        );
    }
    let _ = write!(
        o,
        "<div class=\"row axis\"><span></span><span class=track>{}</span><span></span><span></span></div>",
        ticks
            .iter()
            .map(|t| format!("<em style=\"left:{:.2}%\">{}</em>", log_frac(*t, lo, hi) * 100.0, esc(&tick_label(view, *t))))
            .collect::<String>()
    );
    if !failures.is_empty() {
        let _ = write!(o, "<p class=fails>{}</p>", esc(&failures.join(". ")));
    }
    o.push_str("</section>");
}

const STYLE: &str = r#"
:root{--paper:#fbfbfa;--ink:#16212c;--dim:#6a7784;--rule:#e1e5e9;--bar:#7d93aa;--base:#e0612a;--up:#1f7a4d;--down:#a14a3b;--bad:#c23b2e;
--sans:ui-sans-serif,"Helvetica Neue",Arial,sans-serif;--mono:ui-monospace,"SF Mono",Menlo,Consolas,monospace}
*{box-sizing:border-box}
html{background:var(--paper)}
body{margin:0;color:var(--ink);font:14px/1.5 var(--sans)}
main{max-width:1320px;margin:0 auto;padding:56px 32px 96px}
header{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:24px 48px;align-items:end;margin-bottom:12px}
h1{margin:0;font-size:44px;line-height:1;font-weight:700;letter-spacing:-.035em}
.lede{margin:14px 0 0;max-width:62ch;color:var(--dim);font-size:15px}
.lede b{color:var(--base);font-weight:600}
.switch{display:inline-flex;border:1px solid var(--ink);border-radius:999px;padding:3px}
.switch button{border:0;background:none;color:var(--ink);font:inherit;font-weight:500;padding:6px 16px;border-radius:999px;cursor:pointer}
.switch button[aria-pressed=true]{background:var(--ink);color:var(--paper)}
.switch button:focus-visible{outline:2px solid var(--base);outline-offset:2px}
.setup{margin:28px 0 0;padding:12px 0;border-top:1px solid var(--ink);border-bottom:1px solid var(--rule);display:flex;flex-wrap:wrap;gap:6px 36px;color:var(--dim);font-size:13px}
.setup span b{color:var(--ink);font-weight:600;font-family:var(--mono);font-size:12.5px}
.unit{margin:14px 0 0;color:var(--dim);font-size:13px}
h2{margin:44px 0 14px;font-size:22px;letter-spacing:-.02em}
.grid{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:36px 40px}
.panel{min-width:0}
h3{margin:0 0 8px;padding-bottom:6px;border-bottom:1px solid var(--ink);font:600 15px/1.2 var(--mono)}
.row{display:grid;grid-template-columns:104px minmax(0,1fr) 54px 52px;gap:0 8px;align-items:center;height:25px}
.name{overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--dim);font-size:12.5px}
.is-base .name{color:var(--base);font-weight:700}
.track{position:relative;height:100%}
.track u{position:absolute;top:0;bottom:0;width:1px;background:var(--rule)}
.bar{position:absolute;left:0;top:6px;height:13px;background:var(--bar);border-radius:0 2px 2px 0}
.bar.base{background:var(--base)}
.bar.hatch{background-image:repeating-linear-gradient(135deg,rgba(255,255,255,.75) 0 2px,transparent 2px 5px)}
.val{text-align:right;font:12.5px var(--mono);font-variant-numeric:tabular-nums;white-space:nowrap}
.is-base .val{font-weight:700}
.flag{color:var(--bad);margin-left:3px}
.x{text-align:right;font:12px var(--mono);font-variant-numeric:tabular-nums}
.x.up{color:var(--up)}
.x.down{color:var(--down)}
.axis{height:18px}
.axis .track{height:18px}
.axis em{position:absolute;top:2px;transform:translateX(-50%);font:11px var(--mono);font-style:normal;color:var(--dim);white-space:nowrap}
.track s{position:absolute;top:2px;bottom:2px;width:0;border-left:1.5px dashed var(--base);opacity:.85;z-index:1}
.fails{margin:6px 0 0;color:var(--bad);font-size:12px;line-height:1.35}
.view{display:none}
body[data-view=rps] .v-rps,body[data-view=cpu] .v-cpu,body[data-view=p99] .v-p99{display:block}
body[data-view=rps] .grid.v-rps,body[data-view=cpu] .grid.v-cpu,body[data-view=p99] .grid.v-p99{display:grid}
.mem{max-width:720px}
.mem .row{grid-template-columns:130px minmax(0,1fr) 130px}
.mem .bar{position:static;display:inline-block;vertical-align:middle;margin-right:0}
.mem .track{display:flex;align-items:center}
.mem .anon{background:var(--bar)}
.mem .file{background:#c5d0da}
.mem .is-base .anon{background:var(--base)}
.mem .is-base .file{background:#f3b79b}
.notes{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:20px 40px}
.notes article{border-top:1px solid var(--rule);padding-top:10px}
.notes h4{margin:0 0 4px;font:600 13px var(--mono)}
.notes p{margin:0 0 6px;color:var(--dim);font-size:13px}
.notes code{font:12px var(--mono);color:var(--ink);background:#eef1f4;padding:1px 4px;border-radius:3px;overflow-wrap:anywhere}
.legend{margin:16px 0 0;color:var(--dim);font-size:12.5px;display:flex;gap:22px;flex-wrap:wrap}
.legend i{display:inline-block;width:18px;height:9px;margin-right:6px;vertical-align:baseline;background:var(--bar)}
.legend i.d{width:0;height:11px;border-left:1.5px dashed var(--base);background:none;margin-right:8px;vertical-align:middle}
.legend i.b{background:var(--base)}
.legend i.h{background:var(--bar) repeating-linear-gradient(135deg,rgba(255,255,255,.75) 0 2px,transparent 2px 5px)}
@media(max-width:1100px){.grid{grid-template-columns:repeat(2,minmax(0,1fr))}.mem,.notes{grid-template-columns:1fr}header{grid-template-columns:1fr}}
@media(max-width:700px){main{padding:32px 16px 64px}.grid{grid-template-columns:1fr}h1{font-size:34px}}
"#;

const SCRIPT: &str = r#"
const buttons=document.querySelectorAll('.switch button');
const show=k=>{document.body.dataset.view=k;buttons.forEach(b=>b.setAttribute('aria-pressed',b.dataset.k===k))};
buttons.forEach(b=>b.onclick=()=>show(b.dataset.k));
show('rps');
"#;

pub fn page(rep: &Report, baseline: Option<&str>) -> String {
    let base_idx = baseline.and_then(|b| rep.servers.iter().position(|x| x.name == b)).unwrap_or(0);
    let base_name = esc(&rep.servers[base_idx].name);
    let order = geomean_order(rep, base_idx);

    let mut o = String::new();
    let _ = write!(
        o,
        "<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>Blossom benchmark</title><style>{STYLE}</style><body data-view=rps><main>"
    );
    let _ = write!(
        o,
        "<header><div><h1>Blossom server benchmark</h1><p class=lede>{} servers, same container limits, same load. <b>{base_name}</b> is the reference: green figures beat it, red figures trail it. Servers are ordered by the geometric mean of their throughput across every workload.</p></div><div class=switch role=group aria-label=\"Metric\">",
        rep.servers.len()
    );
    for v in &VIEWS {
        let _ = write!(o, "<button data-k={} aria-pressed=false>{}</button>", v.key, v.label);
    }
    o.push_str("</div></header>");
    let _ = write!(
        o,
        "<div class=setup><span>run <b>{}</b></span><span>suite <b>{}</b></span><span>servers on CPUs <b>{}</b></span><span>load generator on CPUs <b>{}</b></span><span><b>{}</b> connections</span><span>host <b>{}</b> CPUs</span></div>",
        esc(&rep.when),
        esc(rep.suite.rsplit('/').next().unwrap_or(&rep.suite)),
        esc(&rep.server_cpus),
        esc(&rep.load_cpus),
        rep.connections,
        rep.host_cpus
    );
    o.push_str("<div class=legend><span><i class=b></i>reference server</span><span><i></i>other servers</span><span><i class=h></i>some requests failed, rate is not a clean comparison</span><span><i class=d></i>dashed line marks the reference value</span></div>");

    for view in &VIEWS {
        let _ = write!(o, "<div class=\"view v-{}\"><p class=unit>{}</p>", view.key, esc(view.unit));
        for (title, workloads) in GROUPS {
            let present: Vec<&&str> = workloads.iter().filter(|w| rep.servers.iter().any(|s| find(s, w).is_some())).collect();
            if present.is_empty() {
                continue;
            }
            let _ = write!(o, "<h2>{title}</h2><div class=grid>");
            for w in present {
                panel(&mut o, rep, &order, base_idx, w, view);
            }
            o.push_str("</div>");
        }
        o.push_str("</div>");
    }

    o.push_str("<h2>Peak memory</h2><p class=unit style=\"margin:0 0 10px\">Highest resident memory seen in any workload, server plus its database. Dark is anonymous memory, light is file cache.</p><div class=mem>");
    let peaks: Vec<(usize, f64, f64)> = order
        .iter()
        .map(|&i| {
            let mut best = (0.0f64, 0.0f64);
            for r in rep.servers[i].results.iter().filter(|r| r.skipped.is_none()) {
                if r.peak_anon_mb + r.peak_file_mb > best.0 + best.1 {
                    best = (r.peak_anon_mb, r.peak_file_mb);
                }
            }
            (i, best.0, best.1)
        })
        .collect();
    let max_mb = peaks.iter().map(|(_, a, f)| a + f).fold(1.0, f64::max);
    for (i, a, f) in peaks {
        let _ = write!(
            o,
            "<div class=\"row{}\"><span class=name>{}</span><span class=track><i class=\"bar anon\" style=\"width:{:.2}%\"></i><i class=\"bar file\" style=\"width:{:.2}%\"></i></span><span class=val>{:.0} + {:.0} MB</span></div>",
            if i == base_idx { " is-base" } else { "" },
            esc(&rep.servers[i].name),
            a / max_mb * 100.0,
            f / max_mb * 100.0,
            a,
            f
        );
    }
    o.push_str("</div>");

    o.push_str("<h2>Notes</h2><div class=notes>");
    for &i in &order {
        let sv = &rep.servers[i];
        let mut lines: Vec<String> = Vec::new();
        if let Some(e) = &sv.error {
            lines.push(format!("Failed to run: {}", esc(e.lines().next().unwrap_or(""))));
        }
        if !sv.notes.is_empty() {
            lines.push(esc(&sv.notes));
        }
        for r in &sv.results {
            if let Some(e) = &r.error_sample {
                let short: String = e.chars().filter(|c| c.is_ascii_graphic() || *c == ' ').take(140).collect();
                lines.push(format!("{}: first failure was <code>{}</code>", esc(&r.name), esc(&short)));
            }
            if r.skipped.is_none() && r.load_gen_util > 0.85 {
                lines.push(format!("{}: load generator was {:.0}% busy, so this may be its limit", esc(&r.name), r.load_gen_util * 100.0));
            }
        }
        if lines.is_empty() {
            continue;
        }
        let _ = write!(o, "<article><h4>{}</h4>", esc(&sv.name));
        for l in lines {
            let _ = write!(o, "<p>{l}</p>");
        }
        o.push_str("</article>");
    }
    o.push_str("</div>");
    let _ = write!(o, "<script>{SCRIPT}</script></main></html>");
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(name: &str, rate: f64) -> ServerRun {
        ServerRun {
            name: name.into(),
            notes: "a <b> note".into(),
            error: None,
            results: vec![WorkloadResult {
                name: "get-1k".into(),
                kind: "get".into(),
                requests: 100,
                ok: 100,
                ok_per_s: rate,
                ..Default::default()
            }],
        }
    }

    fn report() -> Report {
        Report {
            when: "now".into(),
            host_cpus: 8,
            server_cpus: "0-3".into(),
            load_cpus: "4-7".into(),
            connections: 8,
            suite: "s".into(),
            servers: vec![run("a", 1000.0), run("b", 2000.0)],
        }
    }

    #[test]
    fn page_shows_ratio_and_escapes_notes() {
        let html = page(&report(), Some("a"));
        assert!(html.contains("<title>Blossom benchmark</title>"));
        assert!(html.contains("2.00x"));
        assert!(html.contains("a &lt;b&gt; note"));
        assert!(!html.contains("a <b> note"));
    }

    #[test]
    fn best_server_is_listed_first() {
        let rep = report();
        assert_eq!(geomean_order(&rep, 0), vec![1, 0]);
    }

    #[test]
    fn failures_are_hatched_and_listed() {
        let mut rep = report();
        rep.servers[1].results[0].ok = 50;
        let html = page(&rep, Some("a"));
        assert!(html.contains("bar hatch"));
        assert!(html.contains("b: 50% failed"));
    }

    #[test]
    fn log_axis_spans_the_data_in_whole_decades() {
        let (lo, hi) = log_axis(&[1970.0, 5210.0, 50.0]);
        assert_eq!((lo, hi), (10.0, 10000.0));
        assert!(log_frac(50.0, lo, hi) > 0.0 && log_frac(5210.0, lo, hi) < 1.0);
        assert_eq!(log_frac(0.0, lo, hi), 0.0);
    }

    #[test]
    fn every_metric_view_is_rendered() {
        let html = page(&report(), Some("a"));
        for v in &VIEWS {
            assert!(html.contains(&format!("view v-{}", v.key)));
        }
    }
}
