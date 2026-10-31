//! `molt env doctor`, and the readiness check before a run: which
//! environment profiles apply to a workspace, whether their probes pass in
//! the sandbox as a run's commands would see it, whether the toolchains
//! the project declares are the ones commands get, whether its
//! dependencies and required variables are there, and whether its own
//! build passes before any change. Each check that fails says why and how
//! to fix it. The CLI, the interface and CI all go through [`check`].
//!
//! Every command runs in a fork of the workspace, which is dropped after,
//! so the project is never written; nothing is installed. Passing
//! environment checks are kept by fingerprint (see `readiness`).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Context;
use molt_api::fs::ForkResponse;
use molt_api::shell::RunResponse;
use molt_tools::{env_profile, EnvProfile, ExecutionPolicy, Fs, Roots, SandboxPolicy, Shell};
use serde_json::json;

use crate::readiness::{self, Item, Kind, ReadinessSection, Report, Status};

/// How long one probe may take.
const PROBE_MS: u64 = 120_000;
/// How long the project's build may take.
const BUILD_MS: u64 = 1_800_000;

/// What to check.
pub struct Options<'a> {
    /// The canonical workspace.
    pub workspace: &'a Path,
    /// The user's profiles, over the built-in ones.
    pub user: &'a BTreeMap<String, EnvProfile>,
    /// The user's `--sandbox-policy`, or the default, under the profiles.
    pub policy: SandboxPolicy,
    pub readiness: &'a ReadinessSection,
    /// Where a ready check is kept, if anywhere.
    pub cache: Option<PathBuf>,
    /// Check everything again, whatever is kept.
    pub fresh: bool,
    /// What `molt env prepare` made, to say whether the lock is prepared.
    pub deps: Option<crate::prepare::Deps>,
}

/// A profile that applies, what matched, and what it could not provide.
#[derive(Clone, Debug)]
pub struct Found {
    pub name: String,
    pub why: String,
    pub notes: Vec<String>,
}

/// The check of one workspace.
pub struct Checked {
    pub report: Report,
    pub profiles: Vec<Found>,
    /// The names of all the profiles, when none applies.
    pub known: Vec<String>,
}

impl Checked {
    /// The report as `molt env doctor` prints it.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.profiles.is_empty() && self.report.items.is_empty() {
            let _ = writeln!(
                out,
                "No environment profile applies to {} ({} look for their files).",
                self.report.workspace.display(),
                self.known.join(", ")
            );
            return out;
        }
        for found in &self.profiles {
            let _ = writeln!(out, "{} (found {})", found.name, found.why);
            for note in &found.notes {
                let _ = writeln!(out, "  note    {note}");
            }
        }
        out.push_str(&self.report.render());
        out
    }
}

/// Check the workspace in `opts`.
pub async fn check(opts: Options<'_>) -> anyhow::Result<Checked> {
    let started = Instant::now();
    let workspace = opts.workspace;
    let profiles = env_profile::profiles(opts.user);
    let found = env_profile::detect(&profiles, workspace);
    let declared = readiness::declared(workspace);
    let tmp = tempfile::Builder::new().prefix("molt-env-doctor-").tempdir()?;
    let scratch = tmp.path().join("scratch");
    let var = |name: &str| std::env::var_os(name);
    let mut policy = opts.policy;
    let applied = env_profile::apply(&mut policy, workspace, &[workspace, &scratch], &profiles, &var);
    let profiles_found: Vec<Found> = found
        .iter()
        .zip(&applied)
        .map(|((name, _, why), applied)| Found {
            name: (*name).to_owned(),
            why: (*why).to_owned(),
            notes: applied.notes.clone(),
        })
        .collect();
    let known = profiles.keys().cloned().collect();
    let present: Vec<(String, bool)> =
        opts.readiness.env.iter().map(|name| (name.clone(), std::env::var_os(name).is_some())).collect();
    let fingerprint = readiness::fingerprint(&readiness::Inputs {
        root: workspace,
        profiles: found.iter().map(|(name, profile, _)| (*name, *profile)).collect(),
        policy: &policy,
        present: present.clone(),
        path: std::env::var_os("PATH"),
    });
    let build = opts.readiness.build.as_deref();
    if found.is_empty() && declared.is_empty() && present.is_empty() && build.is_none() {
        let report = Report::new(workspace, fingerprint, Vec::new(), false, elapsed(started));
        return Ok(Checked { report, profiles: profiles_found, known });
    }
    let kept =
        opts.cache.as_deref().filter(|_| !opts.fresh).and_then(|p| readiness::cached(p, workspace, &fingerprint));
    let cached = kept.is_some();
    let mut sandbox: Option<Sandbox> = None;
    let mut items = match kept {
        Some(items) => items,
        None => {
            let sandbox = match Sandbox::start(workspace, &scratch, policy.clone()).await {
                Ok(started) => sandbox.insert(started),
                Err(item) => {
                    let report = Report::new(workspace, fingerprint, vec![item], false, elapsed(started));
                    return Ok(Checked { report, profiles: profiles_found, known });
                }
            };
            let mut items = Vec::new();
            for (_, profile, _) in &found {
                for probe in &profile.probes {
                    let ran = sandbox.run(&probe.command, PROBE_MS).await?;
                    let (status, detail) = match outcome(&ran) {
                        None => (Status::Ready, "passes".to_owned()),
                        Some((Status::Unknown, ending)) => (Status::Unknown, ending),
                        Some((_, ending)) => (Status::Blocked, format!("{ending}: {}", said(&ran))),
                    };
                    let fix = (!probe.fix.is_empty()).then(|| probe.fix.clone());
                    items.push(Item {
                        kind: Kind::Probe,
                        name: probe.command.clone(),
                        status,
                        detail,
                        fix,
                        ms: ran.duration_ms,
                    });
                }
            }
            for d in &declared {
                let ran = sandbox.run(d.command, PROBE_MS).await?;
                let (status, detail, fix) = match outcome(&ran) {
                    Some((Status::Unknown, ending)) => (Status::Unknown, format!("`{}` {ending}", d.command), None),
                    _ => readiness::judge(d, ran.success(), &format!("{}\n{}", ran.stdout, ran.stderr)),
                };
                items.push(Item {
                    kind: Kind::Toolchain,
                    name: d.tool.to_owned(),
                    status,
                    detail,
                    fix,
                    ms: ran.duration_ms,
                });
            }
            // Whether `molt env prepare` made the lock's environment, or it changed since.
            let prepared = opts.deps.as_ref().and_then(|deps| deps.readiness(workspace));
            if !policy.network {
                let cargo = policy.cargo_cache.is_some() || policy.environment.iter().any(|n| n == "CARGO_HOME");
                let python_env =
                    prepared.is_some() || found.iter().any(|(name, _, _)| *name == crate::prepare::PROFILE);
                for missing in crate::agent::missing_dependencies(workspace, cargo, python_env) {
                    let (name, detail) = missing.split_once(": ").unwrap_or(("dependencies", &missing));
                    items.push(Item {
                        kind: Kind::Dependency,
                        name: name.to_owned(),
                        status: Status::Blocked,
                        detail: detail.to_owned(),
                        fix: None,
                        ms: 0,
                    });
                }
            }
            items.extend(prepared);
            for (name, set) in &present {
                items.push(readiness::env_item(name, &policy.set_env, &policy.environment, *set));
            }
            items
        }
    };
    if let Some(command) = build {
        let item = if readiness::verdict(&items) == Status::Blocked {
            Item {
                kind: Kind::Build,
                name: command.to_owned(),
                status: Status::Skipped,
                detail: "not run, since the environment is blocked".to_owned(),
                fix: None,
                ms: 0,
            }
        } else {
            if sandbox.is_none() {
                match Sandbox::start(workspace, &scratch, policy.clone()).await {
                    Ok(started) => sandbox = Some(started),
                    Err(item) => {
                        items.push(item);
                        let report = Report::new(workspace, fingerprint, items, false, elapsed(started));
                        return Ok(Checked { report, profiles: profiles_found, known });
                    }
                }
            }
            let sandbox = sandbox.as_ref().expect("started above");
            let ran = sandbox.run(command, BUILD_MS).await?;
            build_item(command, &ran)
        };
        items.push(item);
    }
    if let Some(sandbox) = sandbox {
        sandbox.drop_fork().await;
    }
    let report = Report::new(workspace, fingerprint, items, cached, elapsed(started));
    if let Some(path) = &opts.cache {
        if let Err(e) = readiness::store(path, &report) {
            tracing::warn!("could not keep the readiness check in {}: {e:#}", path.display());
        }
    }
    Ok(Checked { report, profiles: profiles_found, known })
}

/// The project's build, judged: a failure is the project's own unless it is
/// a download the sandbox has no network for.
fn build_item(command: &str, ran: &RunResponse) -> Item {
    let (status, detail, fix) = match outcome(ran) {
        None => (Status::Ready, "passes before any change".to_owned(), None),
        Some((Status::Unknown, ending)) => (Status::Unknown, ending, None),
        Some(_) if ran.offline_failure => (
            Status::Blocked,
            format!("fails for want of a dependency it cannot download without network: {}", said(ran)),
            Some("provide the project's dependencies on this machine first (see the dependency checks)".to_owned()),
        ),
        Some((_, ending)) => (
            Status::BuildFails,
            format!("already fails before any change ({ending}): {}; a run's change did not cause it", said(ran)),
            None,
        ),
    };
    Item { kind: Kind::Build, name: command.to_owned(), status, detail, fix, ms: ran.duration_ms }
}

/// `None` when the command passed, else how it ended: unknown when it was
/// timed out or killed, failed with its exit code otherwise.
fn outcome(ran: &RunResponse) -> Option<(Status, String)> {
    if ran.success() {
        return None;
    }
    Some(match (ran.timed_out, ran.exit_code) {
        (true, _) => (Status::Unknown, "timed out".to_owned()),
        (false, Some(code)) => (Status::Blocked, format!("exit code {code}")),
        (false, None) => (Status::Unknown, "was killed".to_owned()),
    })
}

/// The line of a command's output that says why it failed.
fn said(ran: &RunResponse) -> String {
    readiness::last_line(&format!("{}\n{}", ran.stderr.trim(), ran.stdout.trim()))
}

fn elapsed(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The sandbox and a fork of the workspace that the checks run in.
pub(crate) struct Sandbox {
    shell: Shell,
    files: Fs,
    fork: String,
}

impl Sandbox {
    /// Or the check that says the sandbox does not start, with its fix.
    pub(crate) async fn start(workspace: &Path, scratch: &Path, policy: SandboxPolicy) -> Result<Self, Item> {
        let roots = Roots { root: workspace.to_path_buf(), scratch: scratch.to_path_buf() };
        let blocked = |detail: String, fix: &str| Item {
            kind: Kind::Sandbox,
            name: "bubblewrap".to_owned(),
            status: Status::Blocked,
            detail,
            fix: Some(fix.to_owned()),
            ms: 0,
        };
        let shell = Shell::with_policy(roots.clone(), ExecutionPolicy::Configured(policy)).map_err(|e| {
            use molt_tools::resources::Setup;
            let setup = e.downcast_ref::<molt_tools::sandbox::Unavailable>().map(|u| u.setup);
            let fix = match setup {
                // The host has a sandbox, and not the room to start one now (#230).
                Some(Setup::Short(_)) => {
                    "free what it names, or run fewer commands at once (the shell service's --jobs), then check \
                     again; it is not a missing sandbox"
                }
                Some(Setup::Refused) => {
                    "allow unprivileged user namespaces for this user (see CONTRIBUTING.md), or choose --no-sandbox \
                     to run commands unconfined"
                }
                _ => "install bubblewrap 0.9 or newer, or choose --no-sandbox to run commands unconfined",
            };
            blocked(format!("The sandbox does not start here: {e:#}"), fix)
        })?;
        let forked = async {
            let files = Fs::new(roots)?;
            let fork =
                files.handle("fork", json!({ "workspace": workspace })).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            let fork: ForkResponse = serde_json::from_value(fork)?;
            anyhow::Ok((files, fork.fork))
        };
        let (files, fork) = forked.await.map_err(|e| {
            blocked(format!("the workspace could not be forked: {e:#}"), "check the workspace can be read")
        })?;
        Ok(Self { shell, files, fork })
    }

    pub(crate) async fn run(&self, command: &str, timeout_ms: u64) -> anyhow::Result<RunResponse> {
        let ran = self
            .shell
            .handle("run", json!({ "workspace": self.fork, "command": command, "timeout_ms": timeout_ms }))
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
            .with_context(|| format!("running `{command}`"))?;
        Ok(serde_json::from_value(ran)?)
    }

    pub(crate) async fn drop_fork(self) {
        let _ = self.files.handle("drop", json!({ "fork": self.fork })).await;
    }
}
