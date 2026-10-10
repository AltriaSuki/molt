# ratelimit

Token-bucket rate limiting for Go services, standard library only.

- `Limiter` (`limiter.go`): one bucket. `New(rate, burst, clock)` makes a
  bucket that holds up to `burst` tokens and refills continuously at `rate`
  tokens per second. It starts full. `Allow()` spends one token if there is
  one; `AllowN(n)` spends `n` tokens if there are that many. `Reserve(n)`
  spends `n` tokens unconditionally (the balance may go negative) and says how
  long to wait before acting; `Tokens()` reports the current balance.
- `KeyedLimiter` (`keyed.go`): one `Limiter` per key (client IP, API token,
  ...), created the first time a key is seen. `Get(key)`, `Allow(key)`,
  `AllowN(key, n)`, `Len()` and `Keys()`. `Sweep()` forgets keys that have not
  been used for the idle TTL.
- HTTP middleware (`middleware.go`): `Middleware(k, keyFunc)` wraps a handler
  and answers `429 Too Many Requests` with a `Retry-After` header once a
  client is out of tokens. `ClientIP` and `ForwardedFor` are ready-made key
  functions.
- Clocks (`clock.go`): limiters read the time through the `Clock` interface.
  Use `RealClock{}` (or pass `nil`) in production and a `ManualClock` in
  tests. A clock that steps backwards never earns tokens.

```go
// 5 requests/s per client, bursts of 20, forget clients idle for 10 minutes.
limits := ratelimit.NewKeyed(5, 20, 10*time.Minute, nil)
go func() {
	for range time.Tick(time.Minute) {
		limits.Sweep()
	}
}()
mux := http.NewServeMux()
mux.HandleFunc("/api/items", listItems)
http.ListenAndServe(":8080", ratelimit.Middleware(limits, ratelimit.ClientIP)(mux))
```

## Running the tests

Go 1.21 or newer, no dependencies:

```
go test ./
```
