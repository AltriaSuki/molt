//! `molt env doctor`: which environment profiles apply to a workspace, and
//! whether their probes pass in the sandbox, as a run's commands would see
//! it, with a one-line fix for each that fails.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::Context;
use molt_api::fs::ForkResponse;
use molt_api::shell::RunResponse;
use molt_tools::{env_profile, EnvProfile, ExecutionPolicy, Fs, Roots, SandboxPolicy, Shell};
use serde_json::json;

/// How long one probe may take.
const PROBE_MS: u64 = 120_000;
/// Characters of a failed probe's output shown.
const SHOWN: usize = 300;

/// The report for the canonical `workspace`, with the user's `profiles`
/// over the built-in ones and `policy` (the user's `--sandbox-policy`, or
/// the default) under them, and whether every probe passed.
pub async fn run(
    workspace: &Path,
    user: &BTreeMap<String, EnvProfile>,
    mut policy: SandboxPolicy,
) -> anyhow::Result<(String, bool)> {
    let profiles = env_profile::profiles(user);
    let found = env_profile::detect(&profiles, workspace);
    let mut out = String::new();
    if found.is_empty() {
        let names: Vec<&str> = profiles.keys().map(String::as_str).collect();
        writeln!(
            out,
            "No environment profile applies to {} ({} look for their files).",
            workspace.display(),
            names.join(", ")
        )?;
        return Ok((out, true));
    }
    let tmp = tempfile::Builder::new().prefix("molt-env-doctor-").tempdir()?;
    let scratch = tmp.path().join("scratch");
    let var = |name: &str| std::env::var_os(name);
    let applied = env_profile::apply(&mut policy, workspace, &[workspace, &scratch], &profiles, &var);
    let roots = Roots { root: workspace.to_path_buf(), scratch };
    let shell = match Shell::with_policy(roots.clone(), ExecutionPolicy::Configured(policy)) {
        Ok(shell) => shell,
        Err(e) => {
            writeln!(out, "The sandbox does not start here: {e:#}")?;
            writeln!(out, "  fix: install bubblewrap 0.9 or newer, or choose --no-sandbox to run commands unconfined")?;
            return Ok((out, false));
        }
    };
    let files = Fs::new(roots)?;
    let fork = files.handle("fork", json!({ "workspace": workspace })).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let fork: ForkResponse = serde_json::from_value(fork)?;
    let mut ok = true;
    for ((name, profile, why), applied) in found.iter().zip(&applied) {
        writeln!(out, "{name} (found {why})")?;
        for note in &applied.notes {
            writeln!(out, "  note    {note}")?;
        }
        for probe in &profile.probes {
            let ran = shell
                .handle("run", json!({ "workspace": fork.fork, "command": probe.command, "timeout_ms": PROBE_MS }))
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))
                .with_context(|| format!("running `{}`", probe.command))?;
            let ran: RunResponse = serde_json::from_value(ran)?;
            if ran.success() {
                writeln!(out, "  ok      {}", probe.command)?;
                continue;
            }
            ok = false;
            let said = format!("{}\n{}", ran.stderr.trim(), ran.stdout.trim());
            // The error line when there is one, else the last thing said.
            let mut lines = said.lines().map(str::trim).filter(|l| !l.is_empty());
            let error = said.lines().map(str::trim).find(|l| l.to_ascii_lowercase().starts_with("error"));
            let last = error.or_else(|| lines.next_back()).unwrap_or("no output");
            let ending = match (ran.timed_out, ran.exit_code) {
                (true, _) => "timed out".to_owned(),
                (false, Some(code)) => format!("exit code {code}"),
                (false, None) => "killed".to_owned(),
            };
            writeln!(out, "  failed  {} ({ending}): {}", probe.command, last.chars().take(SHOWN).collect::<String>())?;
            if !probe.fix.is_empty() {
                writeln!(out, "          fix: {}", probe.fix)?;
            }
        }
    }
    let _ = files.handle("drop", json!({ "fork": fork.fork })).await;
    Ok((out, ok))
}
