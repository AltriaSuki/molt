//! Summing up a results file: success rate, time to done and cost per task
//! for each arm, how the arms compare on the same runs, and the detail by
//! kind of task and by task.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;

use serde::Serialize;

use crate::record::{Record, Setup};
use crate::stats::{mcnemar, mean, median, wilson};

/// What a results file shows.
#[derive(Clone, Debug, Serialize)]
pub struct Report {
    /// Records counted: the last one of each run, as a retried run has several.
    pub runs: usize,
    pub tasks: usize,
    pub setup: Option<Setup>,
    pub molt_versions: Vec<String>,
    pub git_commits: Vec<String>,
    pub arms: Vec<ArmStats>,
    /// Every two arms, compared on the runs both did.
    pub pairs: Vec<Pair>,
    /// Success counts per arm by split, language, kind and difficulty.
    pub groups: Vec<Group>,
    pub by_task: Vec<TaskRow>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ArmStats {
    pub arm: String,
    pub args: Vec<String>,
    pub runs: u32,
    pub passed: u32,
    pub success_rate: f64,
    /// The 95% Wilson interval of the success rate.
    pub success_ci: (f64, f64),
    /// Runs where the agent said it was done.
    pub claimed_done: u32,
    /// Runs where it said so and the hidden tests failed.
    pub false_done: u32,
    /// Runs that ended in an error rather than a result of the agent's.
    pub errors: u32,
    pub timeouts: u32,
    /// Median time of the solved runs.
    pub median_time_to_done_s: Option<f64>,
    pub median_time_s: Option<f64>,
    pub total_cost_usd: f64,
    pub mean_cost_usd: Option<f64>,
    pub median_cost_usd: Option<f64>,
    /// Everything the arm spent over the runs it solved.
    pub cost_per_solve_usd: Option<f64>,
    pub mean_model_calls: Option<f64>,
}

/// Two arms on the same (task, trial) runs.
#[derive(Clone, Debug, Serialize)]
pub struct Pair {
    pub a: String,
    pub b: String,
    pub pairs: u32,
    pub both: u32,
    pub only_a: u32,
    pub only_b: u32,
    pub neither: u32,
    /// The exact McNemar test of `only_a` against `only_b`.
    pub p_value: f64,
    /// Over the pairs both solved.
    pub both_median_time_s: Option<(f64, f64)>,
    pub both_median_cost_usd: Option<(f64, f64)>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Group {
    /// `split`, `language`, `kind` or `difficulty`.
    pub by: String,
    pub value: String,
    /// Per arm, in the order of [`Report::arms`]: (passed, runs).
    pub counts: Vec<(u32, u32)>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TaskRow {
    pub task: String,
    pub split: String,
    pub language: String,
    /// Per arm, in the order of [`Report::arms`].
    pub cells: Vec<Cell>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Cell {
    pub passed: u32,
    pub runs: u32,
    pub median_time_s: Option<f64>,
    pub mean_cost_usd: Option<f64>,
}

/// The last record of each run, in the order the runs first appear.
pub fn latest(records: &[Record]) -> Vec<&Record> {
    let mut at: HashMap<(&str, &str, u32), usize> = HashMap::new();
    let mut out: Vec<&Record> = Vec::new();
    for r in records {
        match at.get(&(r.task.as_str(), r.arm.as_str(), r.trial)) {
            Some(&i) => out[i] = r,
            None => {
                at.insert((&r.task, &r.arm, r.trial), out.len());
                out.push(r);
            }
        }
    }
    out
}

pub fn report(records: &[Record]) -> Report {
    let records = latest(records);
    let mut arm_names: Vec<&str> =
        records.iter().map(|r| r.arm.as_str()).collect::<BTreeSet<_>>().into_iter().collect();
    // Molt first and the plain loop second read best; any others follow by name.
    arm_names.sort_by_key(|a| (*a != "molt", *a != "plain", *a));
    let of = |arm: &str| -> Vec<&Record> { records.iter().copied().filter(|r| r.arm == arm).collect() };

    let arms = arm_names.iter().map(|&a| arm_stats(a, &of(a))).collect();

    let mut pairs = Vec::new();
    for (i, &a) in arm_names.iter().enumerate() {
        for &b in &arm_names[i + 1..] {
            pairs.push(pair(a, b, &of(a), &of(b)));
        }
    }

    let mut groups = Vec::new();
    let dims: [(&str, Facet); 4] = [
        ("split", |r| r.split.to_string()),
        ("language", |r| r.language.to_string()),
        ("kind", |r| r.kind.to_string()),
        ("difficulty", |r| r.difficulty.to_string()),
    ];
    for (by, value_of) in dims {
        let values: BTreeSet<String> = records.iter().map(|r| value_of(r)).collect();
        for value in values {
            let counts = arm_names
                .iter()
                .map(|&a| {
                    let these: Vec<&&Record> = records.iter().filter(|r| r.arm == a && value_of(r) == value).collect();
                    (these.iter().filter(|r| r.passed).count() as u32, these.len() as u32)
                })
                .collect();
            groups.push(Group { by: by.into(), value, counts });
        }
    }

    let mut by_task: BTreeMap<&str, Vec<&Record>> = BTreeMap::new();
    for r in &records {
        by_task.entry(&r.task).or_default().push(r);
    }
    let by_task = by_task
        .into_iter()
        .map(|(task, rs)| TaskRow {
            task: task.into(),
            split: rs[0].split.to_string(),
            language: rs[0].language.to_string(),
            cells: arm_names
                .iter()
                .map(|&a| {
                    let these: Vec<&Record> = rs.iter().copied().filter(|r| r.arm == a).collect();
                    Cell {
                        passed: these.iter().filter(|r| r.passed).count() as u32,
                        runs: these.len() as u32,
                        median_time_s: median(&these.iter().map(|r| secs(r.wall_ms)).collect::<Vec<_>>()),
                        mean_cost_usd: mean(&these.iter().map(|r| r.cost_usd).collect::<Vec<_>>()),
                    }
                })
                .collect(),
        })
        .collect::<Vec<_>>();

    let set = |f: fn(&Record) -> Option<String>| -> Vec<String> {
        records.iter().filter_map(|r| f(r)).collect::<BTreeSet<_>>().into_iter().collect()
    };
    Report {
        runs: records.len(),
        tasks: by_task.len(),
        setup: records.first().map(|r| r.setup.clone()),
        molt_versions: set(|r| Some(r.molt_version.clone())),
        git_commits: set(|r| r.git_commit.clone()),
        arms,
        pairs,
        groups,
        by_task,
    }
}

/// One way of grouping runs: the value a record has.
type Facet = fn(&Record) -> String;

fn secs(ms: u64) -> f64 {
    ms as f64 / 1000.0
}

fn arm_stats(arm: &str, rs: &[&Record]) -> ArmStats {
    let runs = rs.len() as u32;
    let passed = rs.iter().filter(|r| r.passed).count() as u32;
    let total_cost: f64 = rs.iter().map(|r| r.cost_usd).sum();
    let times: Vec<f64> = rs.iter().map(|r| secs(r.wall_ms)).collect();
    let solved_times: Vec<f64> = rs.iter().filter(|r| r.passed).map(|r| secs(r.wall_ms)).collect();
    let costs: Vec<f64> = rs.iter().map(|r| r.cost_usd).collect();
    ArmStats {
        arm: arm.into(),
        args: rs.first().map(|r| r.arm_args.clone()).unwrap_or_default(),
        runs,
        passed,
        success_rate: if runs == 0 { 0.0 } else { f64::from(passed) / f64::from(runs) },
        success_ci: wilson(passed, runs),
        claimed_done: rs.iter().filter(|r| r.claimed_done).count() as u32,
        false_done: rs.iter().filter(|r| r.claimed_done && !r.passed).count() as u32,
        errors: rs.iter().filter(|r| r.error.is_some()).count() as u32,
        timeouts: rs.iter().filter(|r| r.timed_out).count() as u32,
        median_time_to_done_s: median(&solved_times),
        median_time_s: median(&times),
        total_cost_usd: total_cost,
        mean_cost_usd: mean(&costs),
        median_cost_usd: median(&costs),
        cost_per_solve_usd: (passed > 0).then(|| total_cost / f64::from(passed)),
        mean_model_calls: mean(&rs.iter().map(|r| f64::from(r.model_calls)).collect::<Vec<_>>()),
    }
}

fn pair(a: &str, b: &str, of_a: &[&Record], of_b: &[&Record]) -> Pair {
    let b_runs: HashMap<(&str, u32), &Record> = of_b.iter().map(|r| ((r.task.as_str(), r.trial), *r)).collect();
    let mut p = Pair {
        a: a.into(),
        b: b.into(),
        pairs: 0,
        both: 0,
        only_a: 0,
        only_b: 0,
        neither: 0,
        p_value: 1.0,
        both_median_time_s: None,
        both_median_cost_usd: None,
    };
    let (mut times, mut costs) = ((Vec::new(), Vec::new()), (Vec::new(), Vec::new()));
    for ra in of_a {
        let Some(rb) = b_runs.get(&(ra.task.as_str(), ra.trial)) else { continue };
        p.pairs += 1;
        match (ra.passed, rb.passed) {
            (true, true) => {
                p.both += 1;
                times.0.push(secs(ra.wall_ms));
                times.1.push(secs(rb.wall_ms));
                costs.0.push(ra.cost_usd);
                costs.1.push(rb.cost_usd);
            }
            (true, false) => p.only_a += 1,
            (false, true) => p.only_b += 1,
            (false, false) => p.neither += 1,
        }
    }
    p.p_value = mcnemar(p.only_a, p.only_b);
    p.both_median_time_s = median(&times.0).zip(median(&times.1));
    p.both_median_cost_usd = median(&costs.0).zip(median(&costs.1));
    p
}

/// The report as Markdown.
pub fn markdown(r: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Benchmark results\n");
    if r.runs == 0 {
        out.push_str("No runs yet.\n");
        return out;
    }
    let _ = write!(out, "{} runs of {} tasks", r.runs, r.tasks);
    if let Some(s) = &r.setup {
        let _ = write!(
            out,
            "; model {}, effort {}, max turns {}, ${} and {} per run",
            s.model.as_deref().unwrap_or("default"),
            s.effort.as_deref().unwrap_or("default"),
            s.max_turns.map_or("default".to_owned(), |t| t.to_string()),
            s.task_usd,
            duration(s.timeout_s as f64)
        );
    }
    let _ = writeln!(out, ".");
    let _ = writeln!(
        out,
        "Molt {}{}.\n",
        r.molt_versions.join(", "),
        if r.git_commits.is_empty() { String::new() } else { format!(", tasks at {}", r.git_commits.join(", ")) }
    );

    out.push_str("## Arms\n\n");
    out.push_str(
        "| Arm | Solved | 95% CI | Time to done | Time, all runs | Cost per run | Cost per solve | Said done, failed | Errors | Timeouts |\n",
    );
    out.push_str("|---|---|---|---|---|---|---|---|---|---|\n");
    for a in &r.arms {
        let _ = writeln!(
            out,
            "| {} | {}/{} ({}) | {}–{} | {} | {} | {} | {} | {}/{} | {} | {} |",
            a.arm,
            a.passed,
            a.runs,
            pct(a.success_rate),
            pct(a.success_ci.0),
            pct(a.success_ci.1),
            opt(a.median_time_to_done_s, duration),
            opt(a.median_time_s, duration),
            opt(a.mean_cost_usd, usd),
            opt(a.cost_per_solve_usd, usd),
            a.false_done,
            a.claimed_done,
            a.errors,
            a.timeouts
        );
    }
    out.push_str(
        "\nTimes are medians: time to done over the solved runs. Cost per run is the mean; cost per solve is \
         everything the arm spent over the runs it solved.\n",
    );
    for a in &r.arms {
        let args = if a.args.is_empty() { "the defaults".to_owned() } else { format!("`{}`", a.args.join(" ")) };
        let _ = writeln!(out, "- {}: `molt do` with {args}", a.arm);
    }

    if !r.pairs.is_empty() {
        out.push_str("\n## Head to head\n\n");
        for p in &r.pairs {
            let _ = writeln!(
                out,
                "{} against {} on {} paired runs: both solved {}, only {} {}, only {} {}, neither {} \
                 (exact McNemar p = {:.3}).",
                p.a, p.b, p.pairs, p.both, p.a, p.only_a, p.b, p.only_b, p.neither, p.p_value
            );
            if let (Some(t), Some(c)) = (p.both_median_time_s, p.both_median_cost_usd) {
                let _ = writeln!(
                    out,
                    "Where both solved it, the median time was {} against {}, and the median cost {} against {}.",
                    duration(t.0),
                    duration(t.1),
                    usd(c.0),
                    usd(c.1)
                );
            }
            out.push('\n');
        }
    }

    let header = |out: &mut String, first: &str| {
        let _ = write!(out, "| {first} |");
        for a in &r.arms {
            let _ = write!(out, " {} |", a.arm);
        }
        out.push_str("\n|---|");
        out.push_str(&"---|".repeat(r.arms.len()));
        out.push('\n');
    };
    out.push_str("\n## By kind of task\n\n");
    header(&mut out, "Group");
    for g in &r.groups {
        let _ = write!(out, "| {} {} |", g.by, g.value);
        for (passed, runs) in &g.counts {
            let _ = write!(out, " {passed}/{runs} |");
        }
        out.push('\n');
    }

    out.push_str("\n## By task\n\nEach cell: solved/runs, median time, mean cost.\n\n");
    header(&mut out, "Task");
    for t in &r.by_task {
        let _ = write!(out, "| {} ({}) |", t.task, t.split);
        for c in &t.cells {
            if c.runs == 0 {
                out.push_str(" – |");
            } else {
                let _ = write!(
                    out,
                    " {}/{} · {} · {} |",
                    c.passed,
                    c.runs,
                    opt(c.median_time_s, duration),
                    opt(c.mean_cost_usd, usd)
                );
            }
        }
        out.push('\n');
    }
    out
}

fn pct(x: f64) -> String {
    format!("{:.0}%", x * 100.0)
}

fn usd(x: f64) -> String {
    format!("${x:.2}")
}

fn duration(s: f64) -> String {
    let s = s.round() as u64;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
    }
}

fn opt(x: Option<f64>, f: fn(f64) -> String) -> String {
    x.map_or("–".to_owned(), f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::tests::record;

    fn run(task: &str, arm: &str, trial: u32, passed: bool, wall_s: u64, cost: f64) -> Record {
        let mut r = record(task, arm, trial, passed);
        r.wall_ms = wall_s * 1000;
        r.cost_usd = cost;
        r
    }

    #[test]
    fn arms_are_summed_and_compared_on_the_same_runs() {
        let mut records = vec![
            run("py-a", "plain", 0, true, 60, 0.2),
            run("py-a", "molt", 0, true, 120, 0.8),
            run("py-b", "molt", 0, true, 300, 1.0),
            run("py-b", "plain", 0, false, 100, 0.3),
            run("py-c", "molt", 0, false, 600, 2.0),
            run("py-c", "plain", 0, false, 50, 0.1),
            // A run only one arm did is not paired.
            run("py-d", "molt", 0, true, 30, 0.5),
        ];
        // Said done but failed.
        records[3].claimed_done = true;
        records[5].claimed_done = false;
        // A retried run: the first record no longer counts.
        let mut retried = run("py-c", "molt", 0, false, 1, 9.0);
        retried.error = Some("boom".into());
        records.insert(0, retried);

        let r = report(&records);
        assert_eq!((r.runs, r.tasks), (7, 4));
        assert_eq!(r.arms.iter().map(|a| a.arm.as_str()).collect::<Vec<_>>(), ["molt", "plain"]);
        let molt = &r.arms[0];
        assert_eq!((molt.runs, molt.passed, molt.errors), (4, 3, 0));
        assert_eq!(molt.median_time_to_done_s, Some(120.0));
        assert_eq!(molt.median_time_s, Some(210.0));
        assert!((molt.total_cost_usd - 4.3).abs() < 1e-9);
        assert!((molt.cost_per_solve_usd.unwrap() - 4.3 / 3.0).abs() < 1e-9);
        let plain = &r.arms[1];
        assert_eq!((plain.passed, plain.claimed_done, plain.false_done), (1, 2, 1));

        let p = &r.pairs[0];
        assert_eq!((p.a.as_str(), p.b.as_str()), ("molt", "plain"));
        assert_eq!((p.pairs, p.both, p.only_a, p.only_b, p.neither), (3, 1, 1, 0, 1));
        assert_eq!(p.both_median_time_s, Some((120.0, 60.0)));
        assert!((p.p_value - 1.0).abs() < 1e-9);

        let split = r.groups.iter().find(|g| g.by == "split").unwrap();
        assert_eq!(split.counts, [(3, 4), (1, 3)]);
        assert_eq!(r.by_task.len(), 4);
        assert_eq!(r.by_task[3].cells[1].runs, 0);

        let md = markdown(&r);
        assert!(md.contains("| molt | 3/4 (75%) |"), "{md}");
        assert!(md.contains("molt against plain on 3 paired runs: both solved 1, only molt 1, only plain 0"), "{md}");
        assert!(md.contains("| py-d (dev) | 1/1 · 30s · $0.50 | – |"), "{md}");
        assert!(md.contains("2m 00s against 1m 00s"), "{md}");
        assert!(serde_json::to_value(&r).unwrap()["arms"][0]["passed"] == 3);
    }

    #[test]
    fn an_empty_file_reports_no_runs() {
        assert!(markdown(&report(&[])).contains("No runs yet"));
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(42.4), "42s");
        assert_eq!(duration(250.0), "4m 10s");
        assert_eq!(duration(3720.0), "1h 02m");
    }
}
