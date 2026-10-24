//! What the next task runs with, and the preferences saved across sessions.
use super::*;

/// What the next task runs with.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub attempts: Option<u32>,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub budget_usd: Option<f64>,
    /// The most designing a task's check may spend, out of its budget.
    pub design_budget_usd: Option<f64>,
    /// A check command of the user's, used instead of a designed one.
    pub check: Option<String>,
    /// The model memory learns with: a model, [`agent::SAME_MODEL`], or
    /// `None` for memory's default.
    pub memory_model: Option<String>,
    pub stream: bool,
    pub notify: Notify,
    /// Commit each applied result to git, as `molt do --commit` does.
    pub commit: bool,
    /// The workflow of a task `/ask`, `/quick` or `/verify` started; `None`:
    /// the planner picks one (`auto`). Set for one task, never saved.
    #[serde(skip)]
    pub workflow: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            attempts: None,
            model: None,
            effort: None,
            budget_usd: None,
            design_budget_usd: None,
            check: None,
            stream: true,
            memory_model: None,
            notify: Notify::default(),
            commit: false,
            workflow: None,
        }
    }
}

impl App {
    pub fn configure_preferences(
        &mut self,
        defaults: crate::tui::settings::Saved,
        saved: Option<crate::tui::settings::Saved>,
    ) {
        let active = saved.unwrap_or_else(|| defaults.clone());
        self.mode = active.mode;
        self.settings = active.settings;
        self.preference_defaults = defaults;
        self.preference_changes.clear();
    }

    pub fn take_preference_changes(&mut self) -> Vec<crate::tui::settings::Change> {
        std::mem::take(&mut self.preference_changes)
    }
}

/// The budget a task runs with, when the user set one.
pub(super) fn task_budget(settings: &Settings) -> Option<f64> {
    settings.budget_usd
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    #[test]
    fn saved_settings_override_config_until_reset_and_feed_the_next_request() {
        let root = tempfile::tempdir().unwrap();
        let store = crate::tui::state::Store::new(root.path(), root.path()).unwrap();
        let config = crate::Config::parse("[tui]\nmode='auto'\nattempts=2\ncheck='cargo test'").unwrap();
        let defaults = crate::tui::settings::Saved::defaults(&config.tui);
        let mut app = App::new("/w", None);
        app.configure_preferences(defaults.clone(), None);
        for line in ["/attempts 4", "/check make test", "/model opus", "/effort high", "/budget 2.5"] {
            type_in(&mut app, line);
        }
        for change in app.take_preference_changes() {
            crate::tui::settings::persist(&store, &change, &[]).unwrap();
        }
        let mut restarted = App::new("/w", None);
        restarted.configure_preferences(defaults.clone(), crate::tui::settings::load(&store, &[]).unwrap());
        up(&mut restarted);
        assert_eq!(restarted.mode, Mode::Auto);
        assert!(frame_text(&mut restarted).contains("auto (shift+tab)"));
        let actions = type_in(&mut restarted, "implement it");
        assert!(
            matches!(&actions[0], Action::Run { req, .. } if req.attempts == 4 && req.check.as_deref() == Some("make test")
            && req.model.as_deref() == Some("opus") && req.effort == Some(Effort::High) && req.budget_usd == Some(2.5))
        );
        restarted.task = None;
        type_in(&mut restarted, "/reset");
        for change in restarted.take_preference_changes() {
            crate::tui::settings::persist(&store, &change, &[]).unwrap();
        }
        assert_eq!(restarted.settings.attempts, Some(2));
        assert_eq!(restarted.settings.check.as_deref(), Some("cargo test"));
        assert!(crate::tui::settings::load(&store, &[]).unwrap().is_none());
        let mut plain = App::new("/w", None);
        type_in(&mut plain, "/attempts 3");
        type_in(&mut plain, "/reset");
        assert_eq!(plain.mode, Mode::Confirm);
        assert_eq!(plain.settings, Settings::default());
    }

    #[test]
    fn invalid_settings_and_project_template_overrides_do_not_get_saved() {
        let mut app = App::new("/w", None);
        up(&mut app);
        for line in ["/attempts 0", "/budget NaN", "/effort unknown"] {
            type_in(&mut app, line);
        }
        assert!(app.take_preference_changes().is_empty());
        app.commands.insert(
            "local".into(),
            crate::tui::commands::Command {
                template: "fix it".into(),
                attempts: Some(4),
                check: Some("true".into()),
                ..Default::default()
            },
        );
        let before = app.settings.clone();
        type_in(&mut app, "/local");
        assert_eq!(app.settings, before);
        assert!(app.take_preference_changes().is_empty());
    }
}
