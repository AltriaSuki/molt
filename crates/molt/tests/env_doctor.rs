//! `molt env doctor` on real projects, in the real sandbox.

use std::process::Command;

fn doctor(workspace: &std::path::Path, cargo_home: &std::path::Path) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_molt"))
        .args(["env", "doctor", "--workspace"])
        .arg(workspace)
        .env("CARGO_HOME", cargo_home)
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
    let failed = said.find("failed  cargo tree --offline").expect(&said);
    assert!(
        said[failed..].lines().nth(1).unwrap().contains("fix: ") && said[failed..].contains("cargo fetch"),
        "{said}"
    );

    let empty = tmp.path().join("notes");
    std::fs::create_dir(&empty).unwrap();
    let (ok, said) = doctor(&empty, &cargo_home);
    assert!(ok && said.contains("No environment profile applies"), "{said}");
}
