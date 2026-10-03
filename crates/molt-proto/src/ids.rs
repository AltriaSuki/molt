use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdError {
    #[error("invalid service id {0:?}: use 1-64 of [a-z0-9_-]")]
    Service(String),
    #[error("invalid target {0:?}: expected `service.method`, `service.*` or `topic:<name>`")]
    Target(String),
}

/// Name of a service, e.g. `planner`. Also used as the NATS subject token and
/// the Unix socket file name, so the alphabet is deliberately narrow.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ServiceId(String);

impl ServiceId {
    pub fn new(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        let ok = !s.is_empty()
            && s.len() <= 64
            && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        if ok {
            Ok(Self(s))
        } else {
            Err(IdError::Service(s))
        }
    }

    pub fn kernel() -> Self {
        Self(crate::KERNEL.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ServiceId {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, IdError> {
        Self::new(s)
    }
}

impl From<ServiceId> for String {
    fn from(id: ServiceId) -> String {
        id.0
    }
}

impl FromStr for ServiceId {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, IdError> {
        Self::new(s)
    }
}

impl fmt::Display for ServiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

macro_rules! random_id {
    ($name:ident, $prefix:literal, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// A fresh random id (128 bits).
            pub fn random() -> Self {
                Self(format!(concat!($prefix, "{:032x}"), rand::random::<u128>()))
            }

            pub fn from_raw(s: impl Into<String>) -> Self {
                Self(s.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

random_id!(MsgId, "msg_", "Unique id of one envelope.");
random_id!(TraceId, "trace_", "Groups every message that belongs to one task.");
random_id!(
    CapId,
    "cap_",
    "Unforgeable handle to a capability. Knowing the id is not enough: the kernel also checks the holder."
);

/// What a message is addressed to.
///
/// Text forms: `memory.recall` (one method), `tools.*` (every method of a
/// service) and `topic:audit` (a publish/subscribe topic).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Target {
    Method { service: ServiceId, method: String },
    AnyMethod { service: ServiceId },
    Topic { name: String },
}

impl Target {
    pub fn method(service: &str, method: &str) -> Result<Self, IdError> {
        format!("{service}.{method}").parse()
    }

    /// The service a message to this target is delivered to (none for topics).
    pub fn service(&self) -> Option<&ServiceId> {
        match self {
            Target::Method { service, .. } | Target::AnyMethod { service } => Some(service),
            Target::Topic { .. } => None,
        }
    }

    /// True when every message allowed by `other` is also allowed by `self`.
    pub fn covers(&self, other: &Target) -> bool {
        match (self, other) {
            (Target::AnyMethod { service: a }, Target::AnyMethod { service: b })
            | (Target::AnyMethod { service: a }, Target::Method { service: b, .. }) => a == b,
            (a, b) => a == b,
        }
    }
}

impl FromStr for Target {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, IdError> {
        let bad = || IdError::Target(s.to_owned());
        if let Some(name) = s.strip_prefix("topic:") {
            let ok = !name.is_empty()
                && name.len() <= 128
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
            return if ok { Ok(Target::Topic { name: name.to_owned() }) } else { Err(bad()) };
        }
        let (service, method) = s.split_once('.').ok_or_else(bad)?;
        let service = ServiceId::new(service).map_err(|_| bad())?;
        if method == "*" {
            return Ok(Target::AnyMethod { service });
        }
        let ok = !method.is_empty()
            && method.len() <= 128
            && method.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
        if ok {
            Ok(Target::Method { service, method: method.to_owned() })
        } else {
            Err(bad())
        }
    }
}

impl TryFrom<String> for Target {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, IdError> {
        s.parse()
    }
}

impl From<Target> for String {
    fn from(t: Target) -> String {
        t.to_string()
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Method { service, method } => write!(f, "{service}.{method}"),
            Target::AnyMethod { service } => write!(f, "{service}.*"),
            Target::Topic { name } => write!(f, "topic:{name}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_round_trip() {
        for s in ["memory.recall", "tools.*", "topic:audit.tail", "kernel.registry.promote"] {
            let t: Target = s.parse().unwrap();
            assert_eq!(t.to_string(), s);
        }
        for s in ["", "memory", ".x", "Memory.recall", "topic:", "a.b c"] {
            assert!(s.parse::<Target>().is_err(), "{s:?} should be rejected");
        }
    }

    #[test]
    fn wildcard_covers_methods_of_its_service_only() {
        let any: Target = "tools.*".parse().unwrap();
        assert!(any.covers(&"tools.shell".parse().unwrap()));
        assert!(any.covers(&any));
        assert!(!any.covers(&"memory.recall".parse().unwrap()));
        let one: Target = "tools.shell".parse().unwrap();
        assert!(!one.covers(&any));
    }
}
