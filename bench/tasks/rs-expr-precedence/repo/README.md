# calc

A small calculator for arithmetic expressions with variables: a library
(lexer, recursive-descent parser, evaluator) and a command-line tool that
evaluates one line of stdin at a time.

```
$ printf 'r = 2\npi * r^2\n' | cargo run -q
2
12.566370614359172
```

## The language

- Numbers: `42`, `3.25`, `.5`, `7.`. All arithmetic is `f64`.
- Variables: a letter or `_` followed by letters, digits and `_` (`x`,
  `rate_2`, `π`). Using an unbound name is an error.
- Binary operators `+`, `-`, `*`, `/` and `^` (power), unary `-` and `+`,
  parentheses. Division by zero is an error rather than infinity.
- Built-in functions: `sqrt`, `abs`, `floor`, `ceil`, `round`, `ln`, `exp`
  (one argument) and `min`, `max`, `hypot` (two).
- `name = expr` (through `calc::run` and the CLI) evaluates `expr` and binds
  the result to `name`.

Precedence, loosest first: `+ -`, then `* /`, then the unary signs, then `^`.
`^` is right-associative and the other binary operators are left-associative,
so `-x^2` is `-(x^2)`, `2^3^2` is `2^9` and `10 - 2 - 3` is `5`.

## Library

```rust
use calc::{eval, run, Env};

let mut env = Env::new();
env.set("x", 3.0);
assert_eq!(eval("2 * x + 1", &env), Ok(7.0));
assert_eq!(run("y = x * x", &mut env), Ok(9.0));
```

- `calc::eval(src, &env)` parses and evaluates one expression.
- `calc::run(src, &mut env)` also accepts assignments.
- `calc::parse(src)` returns the `Expr` tree; its `Display` is fully
  parenthesised (`(1 + (2 * 3))`), which is handy for checking grouping.
- Every failure is a `calc::CalcError`. Errors that point into the input carry
  `pos`, a 0-based byte offset.

## Layout

- `src/lexer.rs`: tokens with their byte spans.
- `src/parser.rs`: tokens to `ast::Expr` / `ast::Statement`.
- `src/evaluator.rs`: walks the tree against an `Env`.
- `src/env.rs`, `src/functions.rs`: variables and built-in functions.
- `src/error.rs`: `CalcError`.
- `src/main.rs`: the CLI. Blank lines and `#` comments are skipped; errors go
  to stderr with a caret under the offending character.

## Tests

Rust 1.80 or newer, no dependencies:

```
cargo test --offline --lib --test lexer --test parser --test eval --test cli
```
