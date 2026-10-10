//! Built-in functions callable from expressions, such as `sqrt(x)`.

use crate::error::CalcError;

type Builtin = fn(&[f64]) -> f64;

/// Looks up a built-in by name: its arity and implementation.
fn lookup(name: &str) -> Option<(usize, Builtin)> {
    let builtin: (usize, Builtin) = match name {
        "sqrt" => (1, |a| a[0].sqrt()),
        "abs" => (1, |a| a[0].abs()),
        "floor" => (1, |a| a[0].floor()),
        "ceil" => (1, |a| a[0].ceil()),
        "round" => (1, |a| a[0].round()),
        "ln" => (1, |a| a[0].ln()),
        "exp" => (1, |a| a[0].exp()),
        "min" => (2, |a| a[0].min(a[1])),
        "max" => (2, |a| a[0].max(a[1])),
        "hypot" => (2, |a| a[0].hypot(a[1])),
        _ => return None,
    };
    Some(builtin)
}

/// The names of all built-in functions, sorted.
pub const NAMES: [&str; 10] = [
    "abs", "ceil", "exp", "floor", "hypot", "ln", "max", "min", "round", "sqrt",
];

/// Calls the built-in `name` with already evaluated arguments.
pub fn call(name: &str, args: &[f64]) -> Result<f64, CalcError> {
    let (arity, f) = lookup(name).ok_or_else(|| CalcError::UnknownFunction(name.to_string()))?;
    if args.len() != arity {
        return Err(CalcError::WrongArity {
            name: name.to_string(),
            expected: arity,
            found: args.len(),
        });
    }
    Ok(f(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_name_exists() {
        for name in NAMES {
            assert!(lookup(name).is_some(), "{name} is listed but not defined");
        }
    }

    #[test]
    fn checks_arity() {
        assert_eq!(
            call("max", &[1.0]),
            Err(CalcError::WrongArity {
                name: "max".into(),
                expected: 2,
                found: 1
            })
        );
        assert_eq!(call("hypot", &[3.0, 4.0]), Ok(5.0));
    }

    #[test]
    fn unknown_name() {
        assert_eq!(
            call("cbrt", &[8.0]),
            Err(CalcError::UnknownFunction("cbrt".into()))
        );
    }
}
