//! `molt env doctor` on real projects, in the real sandbox.

use std::process::Command;

fn doctor(workspace: &std::path::Path, cargo_home: &std::path::Path) -> (bool, String) {
    doctor_with(workspace, cargo_home, &cargo_home.join("no-config"))
}

/// `molt env doctor` with `config_home` as `$XDG_CONFIG_HOME`, so the user config is the test's.
fn doctor_with(
    workspace: &std::path::Path,
    cargo_home: &std::path::Path,
    config_home: &std::path::Path,
) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_molt"))
        .args(["env", "doctor", "--workspace"])
        .arg(workspace)
        .env("CARGO_HOME", cargo_home)
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_CACHE_HOME", cargo_home.join("cache"))
        .output()
        .unwrap();
    (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

#[test]
fn the_doctor_reports_a_missing_cargo_cache_with_its_fix_and_is_quiet_without_a_toolchain() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"lines\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nmemchr = \"2\"\n",
    )
    .unwrap();
    std::fs::write(project.join("src/lib.rs"), "").unwrap();
    std::fs::write(
        project.join("Cargo.lock"),
        "version = 3\n\n[[package]]\nname = \"lines\"\nversion = \"0.1.0\"\ndependencies = [\"memchr\"]\n\n\
         [[package]]\nname = \"memchr\"\nversion = \"2.7.4\"\n\
         source = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
    )
    .unwrap();
    let cargo_home = tmp.path().join("cargo-home");
    std::fs::create_dir(&cargo_home).unwrap();
    let (ok, said) = doctor(&project, &cargo_home);
    if said.contains("The sandbox does not start here") {
        assert!(std::env::var_os("MOLT_REQUIRE_SANDBOX_TESTS").is_none(), "{said}");
        eprintln!("sandbox unavailable: {said}");
        return;
    }
    assert!(!ok, "{said}");
    assert!(said.contains("rust (found Cargo.toml)"), "{said}");
    assert!(said.contains(&format!("no Cargo cache in {}", cargo_home.join("registry").display())), "{said}");
    let failed = said.find("blocked probe cargo tree --offline").expect(&said);
    assert!(
        said[failed..].lines().nth(1).unwrap().contains("fix: ") && said[failed..].contains("cargo fetch"),
        "{said}"
    );

    let empty = tmp.path().join("notes");
    std::fs::create_dir(&empty).unwrap();
    let (ok, said) = doctor(&empty, &cargo_home);
    assert!(ok && said.contains("No environment profile applies"), "{said}");
}

#[test]
fn the_doctor_runs_the_probes_of_a_profile_in_the_user_config() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("notes.txt"), "").unwrap();
    let config_home = tmp.path().join("config");
    std::fs::create_dir_all(config_home.join("molt")).unwrap();
    std::fs::write(
        config_home.join("molt/config.toml"),
        "[environment.notes]\ndetect = [\"notes.txt\"]\n\
         probes = [{ command = \"test -f notes.txt\", fix = \"add notes.txt\" }]\n",
    )
    .unwrap();
    let (ok, said) = doctor_with(&project, tmp.path(), &config_home);
    if said.contains("The sandbox does not start here") {
        assert!(std::env::var_os("MOLT_REQUIRE_SANDBOX_TESTS").is_none(), "{said}");
        eprintln!("sandbox unavailable: {said}");
        return;
    }
    assert!(ok, "{said}");
    assert!(said.contains("notes (found notes.txt)") && said.contains("ok      probe test -f notes.txt"), "{said}");
}

/// A user's machine for the readiness checks: fake tools on PATH, its own
/// home, config and cache, and the environment variables `vars`.
struct Machine {
    tmp: tempfile::TempDir,
}

impl Machine {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        for dir in ["tools/bin", "home", "config/molt", "cache", "cargo-home"] {
            std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        }
        Self { tmp }
    }

    fn path(&self, rel: &str) -> std::path::PathBuf {
        self.tmp.path().canonicalize().unwrap().join(rel)
    }

    /// A tool that prints `said` and exits with `code`.
    fn tool(&self, name: &str, said: &str, code: i32) {
        use std::os::unix::fs::PermissionsExt;
        let path = self.path("tools/bin").join(name);
        // Replaced, never rewritten in place, as an install would.
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, format!("#!/bin/sh\necho '{said}'\nexit {code}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn project(&self, name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = self.path(name);
        for (rel, text) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    /// `molt env doctor --json` on `project` with `args`: the exit code, the report and what it said.
    fn check(
        &self,
        project: &std::path::Path,
        args: &[&str],
        vars: &[(&str, &str)],
    ) -> (i32, serde_json::Value, String) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_molt"));
        cmd.args(["env", "doctor", "--json", "--workspace"]).arg(project).args(args);
        cmd.env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", self.path("tools/bin").display()))
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_CACHE_HOME", self.path("cache"))
            .env("CARGO_HOME", self.path("cargo-home"));
        for (name, value) in vars {
            cmd.env(name, value);
        }
        let out = cmd.output().unwrap();
        let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        let report = serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null);
        (out.status.code().unwrap_or(-1), report, said)
    }
}

fn sandbox_missing(said: &str) -> bool {
    if said.contains("The sandbox does not start here") {
        assert!(std::env::var_os("MOLT_REQUIRE_SANDBOX_TESTS").is_none(), "{said}");
        eprintln!("sandbox unavailable: {said}");
        return true;
    }
    false
}

/// The checks of `kind` in `report`.
fn items<'a>(report: &'a serde_json::Value, kind: &str) -> Vec<&'a serde_json::Value> {
    report["items"].as_array().unwrap().iter().filter(|i| i["kind"] == kind).collect()
}

#[test]
fn missing_node_dependencies_a_wrong_rust_toolchain_and_a_missing_python_environment_each_get_their_reason() {
    let m = Machine::new();
    m.tool("node", "v20.11.0", 0);
    m.tool("npm", "npm ls: missing: left-pad@1", 1);
    m.tool("rustc", "rustc 1.70.0 (90c541806 2023-05-31)", 0);
    m.tool("cargo", "cargo 1.70.0", 0);
    m.tool("python3", "Python 3.11.4", 0);

    let node = m.project("node", &[("package.json", r#"{"dependencies": {"left-pad": "1"}}"#)]);
    let (code, report, said) = m.check(&node, &[], &[]);
    if sandbox_missing(&said) {
        return;
    }
    assert_eq!((code, report["status"].as_str()), (1, Some("blocked")), "{said}");
    let deps = items(&report, "dependency");
    assert_eq!(deps.len(), 1, "{said}");
    assert_eq!(deps[0]["name"], "Node");
    assert!(deps[0]["detail"].as_str().unwrap().contains("no node_modules"), "{said}");
    assert!(deps[0]["detail"].as_str().unwrap().contains("npm install"), "{said}");

    let rust = m.project(
        "rust",
        &[("Cargo.toml", "[package]\nname = \"a\"\n"), ("rust-toolchain.toml", "[toolchain]\nchannel = \"1.80.0\"\n")],
    );
    let (code, report, said) = m.check(&rust, &[], &[]);
    assert_eq!(code, 1, "{said}");
    let toolchain = items(&report, "toolchain");
    assert_eq!((toolchain.len(), toolchain[0]["status"].as_str()), (1, Some("blocked")), "{said}");
    let detail = toolchain[0]["detail"].as_str().unwrap();
    assert!(detail.contains("rust-toolchain.toml asks for 1.80.0, and commands get rustc 1.70.0"), "{detail}");
    assert!(toolchain[0]["fix"].as_str().unwrap().contains("rustup toolchain install"), "{said}");
    assert!(items(&report, "probe").iter().all(|p| p["status"] == "ready"), "only the toolchain is wrong: {said}");

    let python = m.project(
        "python",
        &[
            ("requirements.txt", "requests==2.31\n"),
            ("pyproject.toml", "[project]\nname = \"p\"\nrequires-python = \">=3.12\"\n"),
        ],
    );
    let (code, report, said) = m.check(&python, &[], &[]);
    assert_eq!(code, 1, "{said}");
    let deps = items(&report, "dependency");
    assert!(deps.len() == 1 && deps[0]["name"] == "Python", "{said}");
    assert!(deps[0]["detail"].as_str().unwrap().contains("no virtual environment (.venv)"), "{said}");
    let toolchain = items(&report, "toolchain");
    let detail = toolchain[0]["detail"].as_str().unwrap();
    assert!(detail.contains("requires-python asks for >=3.12, and commands get python3 3.11.4"), "{detail}");
    // Without --json the same reasons are printed with their fixes and the time each took.
    let out = Command::new(env!("CARGO_BIN_EXE_molt"))
        .args(["env", "doctor", "--workspace"])
        .arg(&python)
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", m.path("tools/bin").display()))
        .env("HOME", m.path("home"))
        .env("XDG_CONFIG_HOME", m.path("config"))
        .env("XDG_CACHE_HOME", m.path("cache"))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("blocked toolchain python3: ") && text.contains(" ms)\n"), "{text}");
    assert!(text.contains("fix: install Python >=3.12") && text.contains("readiness: blocked"), "{text}");
}

#[test]
fn once_the_environment_is_fixed_the_check_passes_and_its_cache_never_hides_a_lock_file_or_toolchain_change() {
    let m = Machine::new();
    m.tool("node", "v20.11.0", 0);
    m.tool("npm", "", 0);
    m.tool("rustc", "rustc 1.70.0 (90c541806 2023-05-31)", 0);
    m.tool("cargo", "cargo 1.70.0", 0);
    let node = m.project("node", &[("package.json", r#"{"dependencies": {"left-pad": "1"}}"#)]);
    let (code, _, said) = m.check(&node, &[], &[]);
    if sandbox_missing(&said) {
        return;
    }
    assert_eq!(code, 1, "{said}");
    // npm install, on the host.
    std::fs::create_dir(node.join("node_modules")).unwrap();
    let (code, report, said) = m.check(&node, &[], &[]);
    assert_eq!((code, &report["status"], &report["cached"]), (0, &"ready".into(), &false.into()), "{said}");
    assert!(report["fingerprint"].as_str().unwrap().len() == 64, "{said}");
    let (code, again, said) = m.check(&node, &[], &[]);
    assert_eq!((code, &again["cached"]), (0, &true.into()), "the same environment is not probed again: {said}");
    assert_eq!(again["fingerprint"], report["fingerprint"]);
    let (_, fresh, _) = m.check(&node, &["--fresh"], &[]);
    assert_eq!(fresh["cached"], false);
    // A new lock file is a new environment.
    std::fs::write(node.join("package-lock.json"), "{\"lockfileVersion\": 3}").unwrap();
    let (code, locked, said) = m.check(&node, &[], &[]);
    assert_eq!((code, &locked["cached"]), (0, &false.into()), "{said}");
    assert_ne!(locked["fingerprint"], report["fingerprint"]);

    let rust = m.project("rust", &[("Cargo.toml", "[package]\nname = \"a\"\nrust-version = \"1.80\"\n")]);
    let (code, _, said) = m.check(&rust, &[], &[]);
    assert_eq!(code, 1, "rustc 1.70 is older than rust-version: {said}");
    m.tool("rustc", "rustc 1.82.0 (f6e511eec 2024-10-15)", 0);
    let (code, report, said) = m.check(&rust, &[], &[]);
    assert_eq!((code, &report["cached"]), (0, &false.into()), "{said}");
    let (_, again, _) = m.check(&rust, &[], &[]);
    assert_eq!(again["cached"], true);
    // A toolchain that changes under the cache is probed again, and found wrong.
    m.tool("rustc", "rustc 1.75.0 (82e1608df 2023-12-21)", 0);
    let (code, report, said) = m.check(&rust, &[], &[]);
    assert_eq!((code, &report["cached"]), (1, &false.into()), "{said}");
    assert!(items(&report, "toolchain")[0]["detail"].as_str().unwrap().contains("rustc 1.75.0"), "{said}");
}

/// Every file of `dir` with its content and when it last changed.
fn snapshot(dir: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>, std::time::SystemTime)> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for entry in std::fs::read_dir(&d).unwrap().flatten() {
            let meta = entry.metadata().unwrap();
            if meta.is_dir() {
                todo.push(entry.path());
            } else {
                out.push((entry.path(), std::fs::read(entry.path()).unwrap(), meta.modified().unwrap()));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn the_build_runs_in_a_fork_its_failure_is_the_projects_own_and_variables_and_model_keys_stay_unread() {
    let m = Machine::new();
    // The build writes, fails with 3 when commands have no model key, and 9 when they do.
    let build = "touch built.txt; test -z \"${ANTHROPIC_API_KEY-}\" || exit 9; exit 3";
    let project = m.project(
        "app",
        &[
            ("notes.txt", "notes\n"),
            (".molt/config.toml", &format!("[readiness]\nbuild = {build:?}\nenv = [\"DEPLOY_TOKEN\"]\n")),
        ],
    );
    let before = snapshot(&project);
    let secret = "sekret-value-123";
    let key = "sk-ant-not-a-real-key";
    let vars = [("DEPLOY_TOKEN", secret), ("ANTHROPIC_API_KEY", key)];
    // Set here but not given to commands: the environment is blocked, and the build waits for it.
    let (code, report, said) = m.check(&project, &[], &vars);
    if sandbox_missing(&said) {
        return;
    }
    assert_eq!(code, 1, "{said}");
    let env = items(&report, "env");
    assert_eq!((env[0]["name"].as_str(), env[0]["status"].as_str()), (Some("DEPLOY_TOKEN"), Some("blocked")), "{said}");
    assert!(env[0]["fix"].as_str().unwrap().contains("--pass-env DEPLOY_TOKEN"), "{said}");
    assert_eq!(items(&report, "build")[0]["status"], "skipped", "{said}");
    // Passed, it is ready, and the build runs and fails as it did before any change.
    let (code, report, said) = m.check(&project, &["--pass-env", "DEPLOY_TOKEN"], &vars);
    assert_eq!((code, report["status"].as_str()), (2, Some("build_fails")), "{said}");
    let built = items(&report, "build");
    let detail = built[0]["detail"].as_str().unwrap();
    assert!(detail.contains("already fails before any change (exit code 3)"), "the key never reached it: {detail}");
    assert!(detail.contains("a run's change did not cause it"), "{detail}");
    assert!(!said.contains(secret) && !said.contains(key), "no value is printed: {said}");
    assert_eq!(snapshot(&project), before, "the project is not written");
    assert!(!project.join("built.txt").exists());
    // A build given on the command line takes the project's place.
    let (code, report, said) = m.check(&project, &["--pass-env", "DEPLOY_TOKEN", "--build", "true"], &vars);
    assert_eq!((code, report["status"].as_str()), (0, Some("ready")), "{said}");
    assert_eq!(report["cached"], true, "the environment checks are kept, the build is run again: {said}");
    assert_eq!(items(&report, "build")[0]["status"], "ready", "{said}");
    // A project cannot require a model key, nor turn on the check before runs.
    std::fs::write(project.join(".molt/config.toml"), "[readiness]\nenv = [\"ANTHROPIC_API_KEY\"]\n").unwrap();
    let (code, _, said) = m.check(&project, &[], &vars);
    assert!(code != 0 && said.contains("never given"), "{said}");
    std::fs::write(project.join(".molt/config.toml"), "[readiness]\nbefore_run = true\n").unwrap();
    let (code, _, said) = m.check(&project, &[], &vars);
    assert!(code != 0 && said.contains("before_run"), "{said}");
}

#[tokio::test]
async fn with_before_run_a_blocked_environment_stops_molt_do_before_any_model_call() {
    let server = wiremock::MockServer::start().await;
    let m = Machine::new();
    std::fs::write(m.path("config/molt/config.toml"), "[readiness]\nbefore_run = true\nenv = [\"DEPLOY_TOKEN\"]\n")
        .unwrap();
    let project = m.project("app", &[("notes.txt", "notes\n")]);
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_molt"))
        .args(["do", "--workspace"])
        .arg(&project)
        .args(["--check", "true", "add a line to notes.txt"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", m.path("home"))
        .env("XDG_CONFIG_HOME", m.path("config"))
        .env("XDG_CACHE_HOME", m.path("cache"))
        .env("XDG_DATA_HOME", m.path("data"))
        .env("ANTHROPIC_API_KEY", "sk-ant-test")
        .env("ANTHROPIC_BASE_URL", server.uri())
        .output()
        .await
        .unwrap();
    let said = String::from_utf8_lossy(&out.stderr);
    if sandbox_missing(&said) {
        return;
    }
    assert_eq!(out.status.code(), Some(1), "{said}");
    assert!(said.contains("readiness: blocked variable DEPLOY_TOKEN: is not set"), "{said}");
    assert!(said.contains("the run does not start"), "{said}");
    assert!(server.received_requests().await.unwrap().is_empty(), "no model was called");
}
