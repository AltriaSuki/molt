# ringbuf

A fixed-capacity ring buffer for Rust, and a moving average built on it.
No dependencies.

`RingBuffer<T>` holds at most `capacity` elements. Pushing into a full buffer
overwrites the oldest element and hands it back to you; popping takes the
oldest element; iteration goes from the oldest element to the newest.

```rust
use ringbuf::RingBuffer;

let mut recent = RingBuffer::with_capacity(3);
recent.push("GET /");
recent.push("GET /about");
recent.push("POST /login");
let evicted = recent.push("GET /admin"); // Some("GET /")
assert_eq!(recent.len(), 3);
assert_eq!(recent.pop(), Some("GET /about"));
```

## API

`RingBuffer<T>` works for any `T` (no `Clone` or `Default` needed). Every
value pushed is dropped exactly once: values it hands back are yours,
`clear()` and `extend` drop what they remove, and dropping the buffer drops
what is left.

- `RingBuffer::with_capacity(n)`: an empty buffer for up to `n` elements.
  Panics if `n` is 0.
- `push(value) -> Option<T>`: appends `value` as the newest element. When the
  buffer is full, the oldest element is overwritten and returned.
- `pop() -> Option<T>`: removes and returns the oldest element.
- `pop_newest() -> Option<T>`: removes and returns the newest element.
- `get(i) -> Option<&T>`: the element `i` places after the oldest (`0` is the
  oldest). `buffer[i]` does the same and panics when `i >= len()`.
- `clear()`: removes every element, keeping the capacity.
- `extend(items)`: pushes each item in turn, overwriting when full.
- `len()`, `is_empty()`, `is_full()`, `capacity()`.
- `iter()`: a `ringbuf::Iter` over `&T`, oldest first. `for x in &buffer`
  does the same. It is double-ended (`iter().rev()` goes newest first) and
  exact-size.
- `Debug` prints the elements as a list, oldest first: `[3, 4, 5]`.

`MovingAverage` (in `ringbuf::moving`, re-exported at the root) averages the
last `n` samples: `add(sample)` returns the new average; `average()`,
`samples()` (oldest first), `min()`, `max()`, `delta()` (newest minus
oldest), `is_warm()` and `reset()` report on the current window.

## Layout

- `src/ring.rs`: `RingBuffer`.
- `src/iter.rs`: `Iter`, the borrowing iterator.
- `src/moving.rs`: `MovingAverage`.

## Tests

Rust 1.80 or newer:

```
cargo test --offline --lib --test ring --test moving
```
