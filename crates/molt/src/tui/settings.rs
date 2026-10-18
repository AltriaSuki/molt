//! Validated workspace preferences, layered over the explicitly selected config.
use anyhow::ensure;
use serde::{Deserialize, Serialize};

use super::app::{App, Mode, Notify, Settings};
use super::state::{keep_history, Store};
use crate::config::TuiSection;

const VERSION: u32 = 1;
const MAX_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Saved {
    version: u32,
    pub mode: Mode,
    pub settings: Settings,
}

#[derive(Debug)]
pub enum Change {
    Save(Saved),
    Reset,
}

impl Saved {
    pub fn new(mode: Mode, settings: Settings) -> Self {
        Self { version: VERSION, mode, settings }
    }

    pub fn defaults(config: &TuiSection) -> Self {
        let mode = match config.mode.as_deref() {
            Some("plan") => Mode::Plan,
            Some("auto") => Mode::Auto,
            _ => Mode::Confirm,
        };
        Self::new(
            mode,
            Settings {
                attempts: config.attempts,
                model: config.model.clone(),
                effort: config.effort,
                budget_usd: config.budget,
                design_budget_usd: None,
                check: config.check.clone(),
                memory_model: config.memory_model.clone(),
                stream: config.stream.unwrap_or(true),
                notify: match config.notify.as_deref() {
                    Some("off") => Notify::Off,
                    Some("all") => Notify::All,
                    _ => Notify::Waiting,
                },
                commit: config.commit.unwrap_or(false),
            },
        )
    }

    fn validate(&self, secrets: &[String]) -> anyhow::Result<()> {
        ensure!(self.version == VERSION, "unsupported saved settings version");
        ensure!(self.settings.attempts.is_none_or(|n| (1..=8).contains(&n)), "saved attempts must be 1 to 8");
        ensure!(
            self.settings.budget_usd.is_none_or(|b| b.is_finite() && b > 0.0),
            "saved budget must be finite and positive"
        );
        ensure!(
            self.settings.design_budget_usd.is_none_or(|b| b.is_finite() && b > 0.0),
            "saved design budget must be finite and positive"
        );
        for value in [&self.settings.model, &self.settings.memory_model, &self.settings.check].into_iter().flatten() {
            ensure!(!value.trim().is_empty() && value.len() <= 16 * 1024, "saved model or check is empty or too long");
            ensure!(keep_history(value, secrets), "settings containing credentials are not saved");
        }
        Ok(())
    }
}

pub fn load(store: &Store, secrets: &[String]) -> anyhow::Result<Option<Saved>> {
    let Some(bytes) = store.read("settings.json", MAX_BYTES)? else { return Ok(None) };
    let saved: Saved = serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("saved settings are malformed"))?;
    saved.validate(secrets)?;
    Ok(Some(saved))
}

pub fn restore(app: &mut App, config: &TuiSection, store: Option<&Store>) {
    let saved = store.and_then(|store| match load(store, &app.secrets) {
        Ok(saved) => saved,
        Err(e) => {
            app.flash = Some(format!("saved settings ignored: {e}"));
            None
        }
    });
    app.configure_preferences(Saved::defaults(config), saved);
}

pub fn persist(store: &Store, change: &Change, secrets: &[String]) -> anyhow::Result<()> {
    match change {
        Change::Save(saved) => {
            saved.validate(secrets)?;
            let bytes = serde_json::to_vec(saved)?;
            ensure!(bytes.len() <= MAX_BYTES, "saved settings exceed 64 KiB");
            store.replace("settings.json", &bytes)
        }
        Change::Reset => store.remove("settings.json"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preferences_are_separate_for_each_workspace_and_reset_removes_the_override() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let a = Store::new(root.path(), &a).unwrap();
        let b = Store::new(root.path(), &b).unwrap();
        let saved = Saved::new(
            Mode::Auto,
            Settings { attempts: Some(4), check: Some("make test".into()), ..Settings::default() },
        );
        persist(&a, &Change::Save(saved.clone()), &[]).unwrap();
        assert_eq!(load(&a, &[]).unwrap(), Some(saved));
        assert_eq!(load(&b, &[]).unwrap(), None);
        persist(&a, &Change::Reset, &[]).unwrap();
        assert!(load(&a, &[]).unwrap().is_none());
    }
    #[test]
    fn damaged_unknown_oversized_and_invalid_preferences_are_rejected_without_echoing_contents() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path(), root.path()).unwrap();
        for bytes in [b"broken secret contents".to_vec(), vec![b'x'; MAX_BYTES + 1]] {
            store.replace("settings.json", &bytes).unwrap();
            let error = load(&store, &[]).unwrap_err().to_string();
            assert!(!error.contains("secret contents"));
            let mut app = App::new("/workspace", None);
            restore(&mut app, &TuiSection::default(), Some(&store));
            assert_eq!(app.settings, Settings::default());
            assert!(app.flash.as_ref().unwrap().starts_with("saved settings ignored:"));
        }
        for (version, attempts, budget) in [(2, 1, 1.0), (1, 0, 1.0), (1, 1, -1.0)] {
            let saved = Saved {
                version,
                mode: Mode::Confirm,
                settings: Settings { attempts: Some(attempts), budget_usd: Some(budget), ..Settings::default() },
            };
            store.replace("settings.json", &serde_json::to_vec(&saved).unwrap()).unwrap();
            assert!(load(&store, &[]).is_err());
        }
        let saved = Saved::new(
            Mode::Confirm,
            Settings { check: Some("use known-sensitive-value".into()), ..Settings::default() },
        );
        assert!(persist(&store, &Change::Save(saved), &["known-sensitive-value".into()]).is_err());
    }
}
