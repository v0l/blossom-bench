use ramhorns::{Content, Ramhorns};

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
    value: fn(&WorkloadResult) -> f64,
    show: fn(f64) -> String,
}

const VIEWS: [View; 3] = [
    View {
        key: "rps",
        label: "Requests per second",
        unit: "successful requests per second, linear scale from zero",
        value: |r| r.ok_per_s,
        show: rate,
    },
    View {
        key: "cpu",
        label: "Per CPU core",
        unit: "successful requests per CPU-second, database included, linear scale from zero",
        value: |r| r.ok_per_cpu_s,
        show: rate,
    },
    View {
        key: "p99",
        label: "p99 latency",
        unit: "99th percentile latency, linear scale from zero, shorter is better",
        value: |r| r.p99_us as f64,
        show: |us| lat(us as u64),
    },
];

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

fn fail_rate(r: &WorkloadResult) -> f64 {
    if r.requests == 0 { 0.0 } else { 1.0 - r.ok as f64 / r.requests as f64 }
}

fn find<'a>(s: &'a ServerRun, name: &str) -> Option<&'a WorkloadResult> {
    s.results.iter().find(|r| r.name == name).filter(|r| r.skipped.is_none())
}

fn has_dq(s: &ServerRun) -> bool {
    s.results.iter().any(|r| r.disqualified)
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

fn linear_axis(values: &[f64]) -> (f64, Vec<f64>) {
    let max = values.iter().copied().fold(0.0, f64::max);
    if max <= 0.0 {
        return (1.0, vec![0.0, 1.0]);
    }
    let rough = max / 4.0;
    let exp = 10f64.powf(rough.log10().floor());
    let step = [1.0, 2.0, 2.5, 5.0, 10.0]
        .iter()
        .map(|m| m * exp)
        .find(|st| *st >= rough)
        .unwrap_or(10.0 * exp);
    let top = (max / step).ceil() * step;
    let ticks = (0..=((top / step).round() as usize)).map(|i| i as f64 * step).collect();
    (top, ticks)
}

fn frac(v: f64, top: f64) -> f64 {
    if v <= 0.0 || top <= 0.0 {
        return 0.0;
    }
    (v / top).clamp(0.0, 1.0)
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

fn geomean_order(rep: &Report) -> Vec<usize> {
    let score = |i: usize| -> f64 {
        let logs: Vec<f64> = rep.servers[i]
            .results
            .iter()
            .filter(|r| r.skipped.is_none() && r.ok_per_s > 0.0 && r.kind != "seed")
            .map(|r| r.ok_per_s.ln())
            .collect();
        if logs.is_empty() { f64::MIN } else { logs.iter().sum::<f64>() / logs.len() as f64 }
    };
    let mut idx: Vec<usize> = (0..rep.servers.len()).collect();
    idx.sort_by(|a, b| {
        has_dq(&rep.servers[*a])
            .cmp(&has_dq(&rep.servers[*b]))
            .then(score(*b).partial_cmp(&score(*a)).unwrap_or(std::cmp::Ordering::Equal))
    });
    idx
}

#[derive(Content)]
struct PageView {
    style: &'static str,
    script: &'static str,
    server_count: usize,
    views: Vec<ViewLink>,
    when: String,
    suite: String,
    server_cpus: String,
    load_cpus: String,
    connections: usize,
    host_cpus: usize,
    ram_data: bool,
    busy_warning: String,
    groups: Vec<GroupTitle>,
    summary: Vec<SummaryRow>,
    chart_views: Vec<ChartView>,
    memory: Vec<MemoryRow>,
    notes: Vec<NoteBlock>,
    has_seed: bool,
    store_heads: Vec<StoreHead>,
    store: Vec<StoreRow>,
}

#[derive(Content)]
struct StoreRow {
    name: String,
    cells: Vec<StoreCell>,
    complete: bool,
}

#[derive(Content)]
struct StoreCell {
    stored: String,
    missing: String,
    bad: String,
}

#[derive(Content)]
struct StoreHead {
    label: String,
}

#[derive(Content)]
struct GroupTitle {
    title: &'static str,
}

#[derive(Content)]
struct NoteLine {
    text: String,
    code: String,
    tail: String,
}

#[derive(Content)]
struct ViewLink {
    key: &'static str,
    label: &'static str,
}

#[derive(Content)]
struct SummaryRow {
    rank: usize,
    name: String,
    cells: Vec<SummaryCell>,
    p99: String,
    peak_memory: String,
    error: String,
    problems: bool,
    dq_list: String,
    bad_list: String,
    off_list: String,
}

#[derive(Content)]
struct SummaryCell {
    value: String,
    width: String,
    dq: bool,
}

#[derive(Content)]
struct ChartView {
    key: &'static str,
    unit: &'static str,
    groups: Vec<ChartGroup>,
}

#[derive(Content)]
struct ChartGroup {
    title: &'static str,
    panels: Vec<PanelView>,
}

#[derive(Content)]
struct PanelView {
    workload: String,
    rows: Vec<BarRow>,
    ticks: Vec<Tick>,
    failures: String,
    dq_note: String,
    offload_note: String,
}

#[derive(Content)]
struct BarRow {
    name: String,
    present: bool,
    dq: bool,
    hatch: bool,
    flag: bool,
    capped: bool,
    width: String,
    value: String,
    grid: Vec<Tick>,
}

#[derive(Content, Clone)]
struct Tick {
    left: String,
    label: String,
}

#[derive(Content)]
struct MemoryRow {
    name: String,
    anon_width: String,
    file_width: String,
    anon: String,
    file: String,
}

#[derive(Content)]
struct NoteBlock {
    name: String,
    lines: Vec<NoteLine>,
}

fn pct(v: f64) -> String {
    format!("{v:.2}")
}

fn panel(rep: &Report, order: &[usize], workload: &str, view: &View) -> PanelView {
    let rows: Vec<(usize, Option<&WorkloadResult>)> = order.iter().map(|&i| (i, find(&rep.servers[i], workload))).collect();
    let values: Vec<f64> = rows.iter().filter_map(|(_, r)| r.map(view.value)).collect();
    let (top, tick_values) = linear_axis(&values);
    let grid: Vec<Tick> = tick_values.iter().map(|t| Tick { left: pct(frac(*t, top) * 100.0), label: String::new() }).collect();

    let mut failures: Vec<String> = Vec::new();
    let mut offloads: Vec<String> = Vec::new();
    let mut dqs: Vec<String> = Vec::new();
    let mut bars = Vec::new();
    for (i, r) in &rows {
        let name = rep.servers[*i].name.clone();
        let Some(r) = r else {
            bars.push(BarRow { name, present: false, dq: false, hatch: false, flag: false, capped: false, width: String::new(), value: String::new(), grid: vec![] });
            continue;
        };
        if let Some(why) = &r.offloaded {
            if r.disqualified {
                dqs.push(format!("{name}: disqualified, {why}"));
            } else {
                offloads.push(format!("{name}: offloaded, {why}"));
            }
        }
        let v = (view.value)(r);
        let probs = problems(r);
        if !probs.is_empty() {
            failures.push(format!("{name}: {}", probs.join(", ")));
        }
        let width = if v > 0.0 { (frac(v, top) * 100.0).max(0.4) } else { 0.0 };
        bars.push(BarRow {
            name,
            present: true,
            dq: r.disqualified,
            hatch: !probs.is_empty(),
            flag: !probs.is_empty(),
            capped: r.capped,
            width: pct(width),
            value: (view.show)(v),
            grid: grid.clone(),
        });
    }
    PanelView {
        workload: workload.to_string(),
        rows: bars,
        ticks: tick_values.iter().map(|t| Tick { left: pct(frac(*t, top) * 100.0), label: tick_label(view, *t) }).collect(),
        failures: failures.join(". "),
        dq_note: dqs.join(". "),
        offload_note: offloads.join(". "),
    }
}

fn group_mean(s: &ServerRun, workloads: &[&str]) -> Option<f64> {
    let vals: Vec<f64> = workloads.iter().filter_map(|w| find(s, w)).map(|r| r.ok_per_s).filter(|v| *v > 0.0).collect();
    if vals.is_empty() {
        return None;
    }
    Some((vals.iter().map(|v| v.ln()).sum::<f64>() / vals.len() as f64).exp())
}

fn group_has_dq(s: &ServerRun, workloads: &[&str]) -> bool {
    workloads.iter().any(|w| find(s, w).is_some_and(|r| r.disqualified))
}

fn read_phase(r: &WorkloadResult) -> bool {
    r.skipped.is_none() && matches!(r.kind.as_str(), "get" | "head" | "miss")
}

fn peak_mem(s: &ServerRun) -> f64 {
    let reads = s.results.iter().filter(|r| read_phase(r)).map(|r| r.peak_anon_mb + r.peak_file_mb).fold(0.0, f64::max);
    if reads > 0.0 { reads } else { s.results.iter().filter(|r| r.skipped.is_none()).map(|r| r.peak_anon_mb + r.peak_file_mb).fold(0.0, f64::max) }
}

fn summary_rows(rep: &Report, order: &[usize]) -> Vec<SummaryRow> {
    let all: Vec<&str> = GROUPS.iter().flat_map(|(_, w)| w.iter().copied()).collect();
    let best: Vec<f64> = GROUPS
        .iter()
        .map(|(_, w)| rep.servers.iter().filter(|s| !group_has_dq(s, w)).filter_map(|s| group_mean(s, w)).fold(0.0, f64::max))
        .collect();
    order
        .iter()
        .enumerate()
        .map(|(rank, &i)| {
            let sv = &rep.servers[i];
            let cells = GROUPS
                .iter()
                .enumerate()
                .map(|(g, (_, w))| match group_mean(sv, w) {
                    Some(v) => SummaryCell {
                        value: rate(v),
                        width: format!("{:.1}", (v / best[g].max(1e-9) * 100.0).clamp(0.0, 100.0)),
                        dq: group_has_dq(sv, w),
                    },
                    None => SummaryCell { value: String::new(), width: String::new(), dq: false },
                })
                .collect();
            let dq: Vec<&str> = all.iter().filter(|w| find(sv, w).is_some_and(|r| r.disqualified)).copied().collect();
            let off: Vec<&str> = all
                .iter()
                .filter(|w| sv.results.iter().any(|r| r.name == **w && r.offloaded.is_some() && !r.disqualified))
                .copied()
                .collect();
            let bad: Vec<&str> = all.iter().filter(|w| find(sv, w).is_some_and(|r| !problems(r).is_empty())).copied().collect();
            SummaryRow {
                rank: rank + 1,
                name: sv.name.clone(),
                cells,
                p99: find(sv, "get-1k").map(|r| lat(r.p99_us)).unwrap_or_else(|| "n/a".into()),
                peak_memory: format!("{:.0}", peak_mem(sv)),
                error: sv.error.as_ref().map(|e| e.lines().next().unwrap_or("").to_string()).unwrap_or_default(),
                problems: !(bad.is_empty() && dq.is_empty() && off.is_empty()),
                dq_list: dq.join(", "),
                bad_list: bad.join(", "),
                off_list: off.join(", "),
            }
        })
        .collect()
}

fn memory_rows(rep: &Report, order: &[usize]) -> Vec<MemoryRow> {
    let peaks: Vec<(usize, f64, f64)> = order
        .iter()
        .map(|&i| {
            let mut best = (0.0f64, 0.0f64);
            let any_reads = rep.servers[i].results.iter().any(read_phase);
            for r in rep.servers[i].results.iter().filter(|r| if any_reads { read_phase(r) } else { r.skipped.is_none() }) {
                if r.peak_anon_mb + r.peak_file_mb > best.0 + best.1 {
                    best = (r.peak_anon_mb, r.peak_file_mb);
                }
            }
            (i, best.0, best.1)
        })
        .collect();
    let max_mb = peaks.iter().map(|(_, a, f)| a + f).fold(1.0, f64::max);
    peaks
        .into_iter()
        .map(|(i, a, f)| MemoryRow {
            name: rep.servers[i].name.clone(),
            anon_width: pct(a / max_mb * 100.0),
            file_width: pct(f / max_mb * 100.0),
            anon: format!("{a:.0}"),
            file: format!("{f:.0}"),
        })
        .collect()
}

fn seed_names(rep: &Report) -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = Vec::new();
    for sv in &rep.servers {
        for r in sv.results.iter().filter(|r| r.kind == "seed") {
            if !out.iter().any(|(n, _)| n == &r.name) {
                out.push((r.name.clone(), r.size.unwrap_or(0)));
            }
        }
    }
    out
}

fn size_label(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 20 => format!("{} MiB", b >> 20),
        b if b >= 1 << 10 => format!("{} KiB", b >> 10),
        b => format!("{b} B"),
    }
}

fn store_rows(rep: &Report, order: &[usize], seeds: &[(String, u64)]) -> Vec<StoreRow> {
    order
        .iter()
        .map(|&i| {
            let sv = &rep.servers[i];
            let cells: Vec<StoreCell> = seeds
                .iter()
                .map(|(name, _)| match sv.results.iter().find(|r| &r.name == name) {
                    Some(r) => StoreCell {
                        stored: rate_count(r.ok),
                        missing: if r.seed_missing > 0 { format!("{} missing", r.seed_missing) } else { String::new() },
                        bad: if r.verify_bad > 0 { format!("{} of {} readbacks wrong", r.verify_bad, r.verified) } else { String::new() },
                    },
                    None => StoreCell { stored: "n/a".into(), missing: String::new(), bad: String::new() },
                })
                .collect();
            let complete = sv.results.iter().filter(|r| r.kind == "seed").all(|r| r.seed_missing == 0 && r.verify_bad == 0);
            StoreRow { name: sv.name.clone(), cells, complete }
        })
        .collect()
}

fn rate_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn note_blocks(rep: &Report, order: &[usize]) -> Vec<NoteBlock> {
    let mut out = Vec::new();
    for &i in order {
        let sv = &rep.servers[i];
        let mut lines: Vec<NoteLine> = Vec::new();
        let plain = |text: String| NoteLine { text, code: String::new(), tail: String::new() };
        if let Some(e) = &sv.error {
            lines.push(plain(format!("Failed to run: {}", e.lines().next().unwrap_or(""))));
        }
        if !sv.notes.is_empty() {
            lines.push(plain(sv.notes.clone()));
        }
        for r in &sv.results {
            if let Some(e) = &r.error_sample {
                let short: String = e.chars().filter(|c| c.is_ascii_graphic() || *c == ' ').take(140).collect();
                lines.push(NoteLine { text: format!("{}: first failure was ", r.name), code: short, tail: String::new() });
            }
            if r.skipped.is_none() && r.load_gen_util > 0.85 {
                lines.push(plain(format!("{}: load generator was {:.0}% busy, so this may be its limit", r.name, r.load_gen_util * 100.0)));
            }
        }
        if !lines.is_empty() {
            out.push(NoteBlock { name: sv.name.clone(), lines });
        }
    }
    out
}

const PAGE: &str = include_str!("../templates/page.html");
const PANEL: &str = include_str!("../templates/panel.html");
const SUMMARY: &str = include_str!("../templates/summary.html");
const STYLE: &str = include_str!("../templates/style.css");
const SCRIPT: &str = include_str!("../templates/script.js");

pub fn page(rep: &Report) -> String {
    let order = geomean_order(rep);
    let seeds = seed_names(rep);
    let chart_views = VIEWS
        .iter()
        .map(|view| ChartView {
            key: view.key,
            unit: view.unit,
            groups: GROUPS
                .iter()
                .filter_map(|(title, workloads)| {
                    let present: Vec<&&str> = workloads.iter().filter(|w| rep.servers.iter().any(|s| find(s, w).is_some())).collect();
                    (!present.is_empty()).then(|| ChartGroup {
                        title,
                        panels: present.into_iter().map(|w| panel(rep, &order, w, view)).collect(),
                    })
                })
                .collect(),
        })
        .collect();

    let data = PageView {
        style: STYLE,
        script: SCRIPT,
        server_count: rep.servers.len(),
        views: VIEWS.iter().map(|v| ViewLink { key: v.key, label: v.label }).collect(),
        when: rep.when.clone(),
        suite: rep.suite.rsplit('/').next().unwrap_or(&rep.suite).to_string(),
        server_cpus: rep.server_cpus.clone(),
        load_cpus: rep.load_cpus.clone(),
        connections: rep.connections,
        host_cpus: rep.host_cpus,
        ram_data: rep.ram_data,
        busy_warning: if rep.host_busy_pct > 5.0 { format!("{:.0}", rep.host_busy_pct) } else { String::new() },
        groups: GROUPS.iter().map(|(t, _)| GroupTitle { title: t }).collect(),
        summary: summary_rows(rep, &order),
        chart_views,
        memory: memory_rows(rep, &order),
        notes: note_blocks(rep, &order),
        has_seed: !seeds.is_empty(),
        store_heads: seeds.iter().map(|(n, size)| StoreHead { label: format!("{n} ({})", size_label(*size)) }).collect(),
        store: store_rows(rep, &order, &seeds),
    };

    let mut engine: Ramhorns = Ramhorns::lazy(".").expect("template engine");
    engine.insert(SUMMARY, "summary").expect("summary template");
    engine.insert(PANEL, "panel").expect("panel template");
    engine.insert(PAGE, "page").expect("page template");
    engine.get("page").expect("page template").render(&data)
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
            ram_data: false,
            host_busy_pct: 0.0,
            servers: vec![run("a", 1000.0), run("b", 2000.0)],
        }
    }

    #[test]
    fn page_shows_results_and_escapes_notes() {
        let html = page(&report());
        assert!(html.contains("<title>Blossom benchmark</title>"));
        assert!(html.contains("2.00k"));
        assert!(!html.contains("reference"));
        assert!(html.contains("a &lt;b&gt; note"));
        assert!(!html.contains("a <b> note"));
    }

    #[test]
    fn disqualified_handler_keeps_its_value_in_its_own_colour_and_ranks_last() {
        let mut rep = report();
        rep.servers[1].results[0].offloaded = Some("serves a redirect".into());
        rep.servers[1].results[0].disqualified = true;
        let html = page(&rep);
        assert!(html.contains("bar dq"));
        assert!(html.contains("2.00k"));
        assert!(html.contains("<b class=dqtag>DQ</b>"));
        assert!(html.contains("b: disqualified, serves a redirect"));
        assert_eq!(geomean_order(&rep), vec![0, 1]);
    }

    #[test]
    fn offloaded_handler_is_named_but_still_scored() {
        let mut rep = report();
        rep.servers[0].results[0].offloaded = Some("hands work to a queue".into());
        let html = page(&rep);
        assert!(html.contains("a: offloaded, hands work to a queue"));
        assert!(html.contains("offloads: get-1k"));
    }

    #[test]
    fn a_capped_upload_is_marked_on_its_bar() {
        let mut rep = report();
        rep.servers[0].results[0].capped = true;
        let html = page(&rep);
        assert!(html.contains("<b class=captag"));
    }

    #[test]
    fn seeded_store_is_shown_and_memory_comes_from_the_reads() {
        let mut rep = report();
        for sv in &mut rep.servers {
            sv.results[0].peak_anon_mb = 100.0;
            sv.results.insert(0, WorkloadResult { name: "seed-1k".into(), kind: "seed".into(), size: Some(1024), ok: 500, requests: 520, ..Default::default() });
            sv.results.push(WorkloadResult { name: "put-1k".into(), kind: "upload".into(), ok: 10, requests: 10, ok_per_s: 5.0, peak_anon_mb: 900.0, ..Default::default() });
        }
        rep.servers[1].results[0].seed_missing = 3;
        let html = page(&rep);
        assert!(html.contains("<h2>Stored data</h2>"));
        assert!(html.contains("seed-1k (1 KiB)"));
        assert!(html.contains("3 missing"));
        assert!(html.contains(">100 MB<") || html.contains("100 + 0 MB"));
        assert!(!html.contains("900 + 0 MB"));
    }

    #[test]
    fn best_server_is_listed_first() {
        let rep = report();
        assert_eq!(geomean_order(&rep), vec![1, 0]);
    }

    #[test]
    fn failures_are_hatched_and_listed() {
        let mut rep = report();
        rep.servers[1].results[0].ok = 50;
        let html = page(&rep);
        assert!(html.contains("bar hatch"));
        assert!(html.contains("b: 50% failed"));
    }

    #[test]
    fn linear_axis_starts_at_zero_and_bar_length_is_proportional() {
        let (top, ticks) = linear_axis(&[1970.0, 5210.0, 50.0]);
        assert_eq!(ticks.first(), Some(&0.0));
        assert!(top >= 5210.0 && top <= 6000.0);
        assert!((frac(2605.0, top) * 2.0 - frac(5210.0, top)).abs() < 1e-9);
        assert!(frac(50.0, top) < 0.01);
        assert_eq!(frac(0.0, top), 0.0);
    }

    #[test]
    fn a_slow_server_gets_a_proportionally_tiny_bar() {
        let mut rep = report();
        rep.servers[0].results[0].ok_per_s = 100.0;
        rep.servers[1].results[0].ok_per_s = 100_000.0;
        let html = page(&rep);
        assert!(html.contains("width:0.40%"));
        assert!(html.contains("linear scale from zero"));
        assert!(!html.contains("log scale"));
    }

    #[test]
    fn every_metric_view_is_rendered() {
        let html = page(&report());
        for v in &VIEWS {
            assert!(html.contains(&format!("view v-{}", v.key)));
        }
    }
}
