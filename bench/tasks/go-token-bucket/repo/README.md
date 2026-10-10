# ratelimit

Token-bucket rate limiting for Go services, standard library only.

- `Limiter` (`limiter.go`): one bucket. `New(rate, burst, clock)` makes a
  bucket that holds up to `burst` tokens and refills continuously at `rate`
  tokens per second. It starts full. `Allow()` spends one token if there is
  one; `AllowN(n)` spends `n` tokens if there are that many.
- `KeyedLimiter` (`keyed.go`): one `Limiter` per key (client IP, API token,
  ...), created the first time a key is seen. `Get(key)`, `Allow(key)`,
  `AllowN(key, n)`, `Len()` and `Keys()`.
- HTTP middleware (`middleware.go`): `Middleware(k, keyFunc)` wraps a handler
  and answers `429 Too Many Requests` with a `Retry-After` header once a
  client is out of tokens. `ClientIP` and `ForwardedFor` are ready-made key
  functions.
- Clocks (`clock.go`): limiters read the time through the `Clock` interface.
  Use `RealClock{}` (or pass `nil`) in production and a `ManualClock` in
  tests.

```go
limits := ratelimit.NewKeyed(5, 20, nil) // 5 requests/s per client, bursts of 20
mux := http.NewServeMux()
mux.HandleFunc("/api/items", listItems)
http.ListenAndServe(":8080", ratelimit.Middleware(limits, ratelimit.ClientIP)(mux))
```

A `KeyedLimiter` keeps a limiter for every key it has ever seen, so on a
public endpoint it grows with the number of distinct clients.

## Running the tests

Go 1.21 or newer, no dependencies:

```
go test ./
```
