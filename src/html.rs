use std::fmt::Write;

use crate::report::{Report, ServerRun};
use crate::workloads::WorkloadResult;

const PALETTE: [&str; 10] = [
    "#5b8def", "#e8833a", "#4fb477", "#c261d4", "#e5c03c", "#38b6c9", "#e0577a", "#8a9aa8", "#a0c43a", "#b07a52",
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

fn bw(mb: f64) -> String {
    if mb >= 1000.0 { format!("{:.1} GB/s", mb / 1000.0) } else { format!("{mb:.0} MB/s") }
}

fn fail_rate(r: &WorkloadResult) -> f64 {
    if r.requests == 0 { 0.0 } else { 1.0 - r.ok as f64 / r.requests as f64 }
}

fn find<'a>(s: &'a ServerRun, name: &str) -> Option<&'a WorkloadResult> {
    s.results.iter().find(|r| r.name == name)
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

const STYLE: &str = r#"
:root{--bg:#0f1318;--panel:#171d25;--line:#26303c;--fg:#dde4ec;--dim:#8b98a8;--bad:#e5645a}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--fg);font:14px/1.45 system-ui,sans-serif}
main{max-width:1180px;margin:0 auto;padding:28px 20px 80px}
h1{margin:0 0 4px;font-size:24px}
h2{margin:36px 0 12px;font-size:17px}
h3{margin:0 0 10px;font-size:14px;display:flex;justify-content:space-between;align-items:baseline}
h3 small{color:var(--dim);font-weight:400}
.meta{color:var(--dim);margin:0 0 18px}
.legend{display:flex;flex-wrap:wrap;gap:6px 16px;margin:12px 0 4px}
.legend span{display:inline-flex;align-items:center;gap:6px}
.sw{width:11px;height:11px;border-radius:3px;display:inline-block}
.card{background:var(--panel);border:1px solid var(--line);border-radius:8px;padding:14px 16px;margin:0 0 14px}
.row{display:grid;grid-template-columns:150px 1fr;gap:10px;align-items:center;margin:4px 0}
.name{color:var(--dim);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.track{position:relative;height:22px}
.bar{height:100%;border-radius:3px;min-width:2px}
.bar.bad{background-image:repeating-linear-gradient(45deg,rgba(0,0,0,.35) 0 4px,transparent 4px 8px)}
.val{position:absolute;left:6px;top:0;line-height:22px;font-size:12px;font-weight:600;text-shadow:0 0 3px #000,0 0 3px #000}
.val em{font-style:normal;font-weight:400;color:#cfd8e2;margin-left:8px}
.val b{color:#ff9a90;margin-left:8px}
table{border-collapse:collapse;width:100%;font-size:13px}
th,td{padding:6px 9px;text-align:right;border-bottom:1px solid var(--line);white-space:nowrap}
th:first-child,td:first-child{text-align:left}
th{color:var(--dim);font-weight:500}
td.base{color:var(--dim)}
.scroll{overflow-x:auto}
ul.notes{padding-left:18px;color:var(--dim)}
ul.notes li{margin:4px 0}
code{font-family:ui-monospace,monospace;font-size:12px;color:var(--fg)}
.tabs{display:flex;gap:6px;margin:0 0 12px;flex-wrap:wrap}
.tabs button{background:var(--panel);color:var(--fg);border:1px solid var(--line);border-radius:6px;padding:5px 11px;cursor:pointer;font:inherit}
.tabs button.on{border-color:#5b8def;background:#1d2a40}
"#;

const SCRIPT: &str = r#"
for (const g of document.querySelectorAll('[data-group]')) {
  const buttons = g.querySelectorAll('.tabs button');
  const panes = g.querySelectorAll('[data-pane]');
  const show = k => {
    buttons.forEach(b => b.classList.toggle('on', b.dataset.k === k));
    panes.forEach(p => p.hidden = p.dataset.pane !== k);
  };
  buttons.forEach(b => b.onclick = () => show(b.dataset.k));
  show(buttons[0].dataset.k);
}
"#;

fn bars(
    out: &mut String,
    rep: &Report,
    colors: &[&str],
    workload: &str,
    value: impl Fn(&WorkloadResult) -> f64,
    label: impl Fn(&WorkloadResult) -> (String, String),
) {
    let mut rows: Vec<(usize, &WorkloadResult)> = rep
        .servers
        .iter()
        .enumerate()
        .filter_map(|(i, s)| find(s, workload).filter(|r| r.skipped.is_none()).map(|r| (i, r)))
        .collect();
    rows.sort_by(|a, b| value(b.1).partial_cmp(&value(a.1)).unwrap_or(std::cmp::Ordering::Equal));
    let max = rows.iter().map(|(_, r)| value(r)).fold(0.0, f64::max).max(1e-9);
    for (i, r) in rows {
        let v = value(r);
        let width = (v / max * 100.0).clamp(0.0, 100.0);
        let probs = problems(r);
        let class = if probs.is_empty() { "bar" } else { "bar bad" };
        let note = if probs.is_empty() { String::new() } else { format!("<b>{}</b>", esc(&probs.join(", "))) };
        let (main, extra) = label(r);
        let extra = if extra.is_empty() { String::new() } else { format!("<em>{}</em>", esc(&extra)) };
        let _ = write!(
            out,
            "<div class=row><div class=name title=\"{n}\">{n}</div><div class=track><div class=\"{class}\" style=\"width:{width:.2}%;background-color:{c}\"></div><div class=val>{m}{extra}{note}</div></div></div>",
            n = esc(&rep.servers[i].name),
            c = colors[i % colors.len()],
            m = esc(&main),
        );
    }
    let missing: Vec<&str> = rep
        .servers
        .iter()
        .filter(|s| find(s, workload).is_none_or(|r| r.skipped.is_some()))
        .map(|s| s.name.as_str())
        .collect();
    if !missing.is_empty() {
        let _ = write!(out, "<div class=row><div class=name></div><div class=name>no result: {}</div></div>", esc(&missing.join(", ")));
    }
}

pub fn page(rep: &Report, baseline: Option<&str>) -> String {
    let base_idx = baseline.and_then(|b| rep.servers.iter().position(|x| x.name == b)).unwrap_or(0);
    let colors: Vec<&str> = (0..rep.servers.len()).map(|i| PALETTE[i % PALETTE.len()]).collect();
    let mut names: Vec<String> = Vec::new();
    for sv in &rep.servers {
        for r in &sv.results {
            if !names.contains(&r.name) {
                names.push(r.name.clone());
            }
        }
    }

    let mut o = String::new();
    let _ = write!(
        o,
        "<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>Blossom benchmark</title><style>{STYLE}</style><main>"
    );
    let _ = write!(
        o,
        "<h1>Blossom benchmark</h1><p class=meta>{} &middot; suite <code>{}</code> &middot; servers on CPUs {} &middot; load generator on CPUs {} &middot; {} connections &middot; host has {} CPUs</p>",
        esc(&rep.when),
        esc(&rep.suite),
        esc(&rep.server_cpus),
        esc(&rep.load_cpus),
        rep.connections,
        rep.host_cpus
    );
    o.push_str("<div class=legend>");
    for (i, s) in rep.servers.iter().enumerate() {
        let _ = write!(o, "<span><i class=sw style=\"background:{}\"></i>{}</span>", colors[i], esc(&s.name));
    }
    o.push_str("</div>");

    let _ = write!(o, "<h2>Ratio against {}</h2><div class=\"card scroll\"><table><tr><th>workload</th>", esc(&rep.servers[base_idx].name));
    for s in &rep.servers {
        let _ = write!(o, "<th>{}</th>", esc(&s.name));
    }
    o.push_str("</tr>");
    for w in &names {
        let base = find(&rep.servers[base_idx], w).filter(|r| r.skipped.is_none() && r.ok_per_s > 0.0);
        let _ = write!(o, "<tr><td>{}</td>", esc(w));
        for (i, s) in rep.servers.iter().enumerate() {
            let cell = match (find(s, w).filter(|r| r.skipped.is_none()), base) {
                (Some(r), Some(_)) if i == base_idx => format!("<td class=base>{}/s</td>", rate(r.ok_per_s)),
                (Some(r), Some(b)) => {
                    let x = r.ok_per_s / b.ok_per_s;
                    let hue = if x >= 1.0 { 140.0 } else { 0.0 };
                    let strength = (x.max(1.0 / x.max(1e-6)).ln() / 3.0).clamp(0.0, 1.0) * 0.45;
                    let mark = if problems(r).is_empty() { "" } else { " !" };
                    format!(
                        "<td style=\"background:hsla({hue},60%,45%,{strength:.2})\" title=\"{}/s\">{}x{}</td>",
                        rate(r.ok_per_s),
                        if x >= 10.0 { format!("{x:.0}") } else if x >= 0.1 { format!("{x:.2}") } else { format!("{x:.3}") },
                        mark
                    )
                }
                (Some(r), None) => format!("<td>{}/s</td>", rate(r.ok_per_s)),
                _ => "<td class=base>n/a</td>".into(),
            };
            o.push_str(&cell);
        }
        o.push_str("</tr>");
    }
    o.push_str("</table><p class=meta style=\"margin:8px 0 0\">A trailing ! means some requests failed or a readback was wrong, so the rate is not a clean comparison.</p></div>");

    o.push_str("<h2>Throughput</h2><div data-group><div class=tabs>");
    for (i, w) in names.iter().enumerate() {
        let _ = write!(o, "<button data-k=\"{i}\">{}</button>", esc(w));
    }
    o.push_str("</div>");
    for (i, w) in names.iter().enumerate() {
        let _ = write!(o, "<div data-pane=\"{i}\" class=card><h3>{} <small>successful requests per second</small></h3>", esc(w));
        let is_data = rep.servers.iter().filter_map(|s| find(s, w)).any(|r| r.kind == "upload" || r.kind == "get");
        bars(&mut o, rep, &colors, w, |r| r.ok_per_s, |r| {
            let main = format!("{}/s", rate(r.ok_per_s));
            if is_data { (main, bw(r.mb_per_s)) } else { (main, String::new()) }
        });
        o.push_str("</div>");
    }
    o.push_str("</div>");

    o.push_str("<h2>Efficiency</h2><div data-group><div class=tabs>");
    for (i, w) in names.iter().enumerate() {
        let _ = write!(o, "<button data-k=\"{i}\">{}</button>", esc(w));
    }
    o.push_str("</div>");
    for (i, w) in names.iter().enumerate() {
        let _ = write!(o, "<div data-pane=\"{i}\" class=card><h3>{} <small>successful requests per CPU-second, including any database</small></h3>", esc(w));
        bars(&mut o, rep, &colors, w, |r| r.ok_per_cpu_s, |r| (format!("{}/cpu-s", rate(r.ok_per_cpu_s)), format!("{:.2} cores", r.cpu_cores)));
        o.push_str("</div>");
    }
    o.push_str("</div>");

    o.push_str("<h2>Latency</h2><div class=\"card scroll\"><table><tr><th>workload</th><th>server</th><th>p50</th><th>p99</th><th>p99.9</th><th>max</th></tr>");
    for w in &names {
        for (i, s) in rep.servers.iter().enumerate() {
            let Some(r) = find(s, w).filter(|r| r.skipped.is_none()) else { continue };
            let _ = write!(
                o,
                "<tr><td>{}</td><td><i class=sw style=\"background:{}\"></i> {}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(w),
                colors[i],
                esc(&s.name),
                lat(r.p50_us),
                lat(r.p99_us),
                lat(r.p999_us),
                lat(r.max_us)
            );
        }
    }
    o.push_str("</table></div>");

    o.push_str("<h2>Memory</h2><div class=\"card scroll\"><table><tr><th>workload</th>");
    for s in &rep.servers {
        let _ = write!(o, "<th>{}</th>", esc(&s.name));
    }
    o.push_str("</tr>");
    for w in &names {
        let _ = write!(o, "<tr><td>{}</td>", esc(w));
        for s in &rep.servers {
            match find(s, w).filter(|r| r.skipped.is_none()) {
                Some(r) => {
                    let _ = write!(o, "<td title=\"anonymous + file-backed\">{:.0} + {:.0} MB</td>", r.peak_anon_mb, r.peak_file_mb);
                }
                None => o.push_str("<td class=base>n/a</td>"),
            }
        }
        o.push_str("</tr>");
    }
    o.push_str("</table></div>");

    o.push_str("<h2>Notes</h2><ul class=notes>");
    for sv in &rep.servers {
        if let Some(e) = &sv.error {
            let _ = write!(o, "<li><code>{}</code> failed: {}</li>", esc(&sv.name), esc(e.lines().next().unwrap_or("")));
        }
        if !sv.notes.is_empty() {
            let _ = write!(o, "<li><code>{}</code>: {}</li>", esc(&sv.name), esc(&sv.notes));
        }
        for r in &sv.results {
            if let Some(e) = &r.error_sample {
                let short: String = e.chars().filter(|c| c.is_ascii_graphic() || *c == ' ').take(160).collect();
                let _ = write!(o, "<li><code>{}</code> / {}: first failure was <code>{}</code></li>", esc(&sv.name), esc(&r.name), esc(&short));
            }
            if r.skipped.is_none() && r.load_gen_util > 0.85 {
                let _ = write!(
                    o,
                    "<li><code>{}</code> / {}: load generator was {:.0}% busy, so this may be the generator's limit</li>",
                    esc(&sv.name),
                    esc(&r.name),
                    r.load_gen_util * 100.0
                );
            }
        }
    }
    o.push_str("</ul>");
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
    fn page_has_every_server_and_escapes_notes() {
        let html = page(&report(), Some("a"));
        assert!(html.contains("<title>Blossom benchmark</title>"));
        assert!(html.contains("2.00x"));
        assert!(html.contains("a &lt;b&gt; note"));
        assert!(!html.contains("a <b> note"));
    }

    #[test]
    fn bar_labels_are_markup_not_text() {
        let html = page(&report(), Some("a"));
        assert!(html.contains("<em>"));
        assert!(!html.contains("&lt;em&gt;"));
    }

    #[test]
    fn failures_are_marked_in_bars_and_ratios() {
        let mut rep = report();
        rep.servers[1].results[0].ok = 50;
        let html = page(&rep, Some("a"));
        assert!(html.contains("bar bad"));
        assert!(html.contains("50% failed"));
        assert!(html.contains("2.00x !"));
    }
}
