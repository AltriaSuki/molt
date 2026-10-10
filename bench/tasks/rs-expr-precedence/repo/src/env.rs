//! Variable bindings for evaluation.

use std::collections::HashMap;

/// A set of named values that expressions can refer to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Env {
    vars: HashMap<String, f64>,
}

impl Env {
    /// An empty environment.
    pub fn new() -> Self {
        Env::default()
    }

    /// An environment holding `pi`, `e` and `tau`. The command-line tool
    /// starts from this one.
    pub fn with_constants() -> Self {
        Env::from_iter([
            ("pi", std::f64::consts::PI),
            ("e", std::f64::consts::E),
            ("tau", std::f64::consts::TAU),
        ])
    }

    /// Binds `name` to `value`, returning the previous value if there was one.
    pub fn set(&mut self, name: impl Into<String>, value: f64) -> Option<f64> {
        self.vars.insert(name.into(), value)
    }

    pub fn get(&self, name: &str) -> Option<f64> {
        self.vars.get(name).copied()
    }

    pub fn remove(&mut self, name: &str) -> Option<f64> {
        self.vars.remove(name)
    }

    pub fn len(&self) -> usize {
        self.vars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    /// The bound names, sorted.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.vars.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }
}

impl<K: Into<String>> FromIterator<(K, f64)> for Env {
    fn from_iter<I: IntoIterator<Item = (K, f64)>>(iter: I) -> Self {
        Env {
            vars: iter.into_iter().map(|(k, v)| (k.into(), v)).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_returns_previous_value() {
        let mut env = Env::new();
        assert_eq!(env.set("x", 1.0), None);
        assert_eq!(env.set("x", 2.0), Some(1.0));
        assert_eq!(env.get("x"), Some(2.0));
    }

    #[test]
    fn names_are_sorted() {
        let env = Env::with_constants();
        assert_eq!(env.names(), ["e", "pi", "tau"]);
        assert_eq!(env.len(), 3);
    }

    #[test]
    fn remove_unbinds() {
        let mut env = Env::from_iter([("x", 1.0)]);
        assert_eq!(env.remove("x"), Some(1.0));
        assert!(env.is_empty());
        assert_eq!(env.get("x"), None);
    }
}
