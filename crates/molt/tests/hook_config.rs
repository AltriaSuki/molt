//! Lifecycle hooks come from the user's config, never a project's.

use std::path::Path;

use molt::layers::{Layered, PROJECT_FILE, USER_FILE};
use molt::Config;
use molt_planner::hooks::Event;

const DECIDING: &str = "[[hooks]]\nevent = \"tool.before\"\nmatcher = \"edit_file|write_file\"\n\
                        paths = [\"migrations/**\"]\ncommand = \"./deny-migrations.sh\"\nblocking = true\n";

fn build(home: &Path, workspace: &Path) -> anyhow::Result<Layered> {
    let config_home = home.to_str().unwrap().to_owned();
    let var = move |name: &str| (name == "XDG_CONFIG_HOME").then(|| config_home.clone());
    let defaults = Config::agent(workspace, Path::new("/data"), Path::new("/bin")).unwrap();
    Layered::build(&defaults, workspace, None, &var)
}

#[test]
fn the_user_config_adds_deciding_hooks_and_a_project_config_cannot() {
    let root = tempfile::tempdir().unwrap();
    let (home, workspace) = (root.path().join("config"), root.path().join("project"));
    std::fs::create_dir_all(home.join("molt")).unwrap();
    std::fs::create_dir_all(workspace.join(".molt")).unwrap();
    // A hook as written before #165 still reads, as the event it became.
    let legacy = "[[hooks]]\nevent = \"before_apply\"\ncommand = \"cargo fmt\"\nblocking = true\n";
    std::fs::write(home.join(USER_FILE), format!("{DECIDING}\n{legacy}")).unwrap();
    let hooks = build(&home, &workspace).unwrap().config.hooks;
    assert_eq!(hooks.iter().map(|h| h.event).collect::<Vec<_>>(), [Event::ToolBefore, Event::ResultReady]);
    assert_eq!(hooks[0].matcher.as_deref(), Some("edit_file|write_file"));
    assert_eq!(hooks[0].paths, ["migrations/**"]);
    molt_planner::hooks::validate(&hooks).unwrap();

    std::fs::write(workspace.join(PROJECT_FILE), DECIDING).unwrap();
    let refused = format!("{:#}", build(&home, &workspace).unwrap_err());
    assert!(refused.contains("a project config may not") && refused.contains("user config"), "{refused}");
}
