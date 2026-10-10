package ratelimit

import (
	"math"
	"net"
	"net/http"
	"strconv"
	"strings"
)

// KeyFunc picks the rate-limiting key for a request.
type KeyFunc func(r *http.Request) string

// ClientIP keys a request by the host part of its RemoteAddr. If RemoteAddr
// has no port, it is used as it is.
func ClientIP(r *http.Request) string {
	host, _, err := net.SplitHostPort(r.RemoteAddr)
	if err != nil {
		return r.RemoteAddr
	}
	return host
}

// ForwardedFor keys a request by the first address in its X-Forwarded-For
// header, falling back to ClientIP when the header is missing or empty. Use
// it only behind a proxy that sets the header, since clients can forge it.
func ForwardedFor(r *http.Request) string {
	if xff := r.Header.Get("X-Forwarded-For"); xff != "" {
		first, _, _ := strings.Cut(xff, ",")
		if ip := strings.TrimSpace(first); ip != "" {
			return ip
		}
	}
	return ClientIP(r)
}

// Middleware returns a wrapper that charges every request one token from the
// limiter for key(r). A request that is out of tokens gets 429 Too Many
// Requests, with a Retry-After header saying how many seconds one token takes
// to come back, and never reaches the wrapped handler. A nil key means
// ClientIP.
func Middleware(k *KeyedLimiter, key KeyFunc) func(http.Handler) http.Handler {
	if key == nil {
		key = ClientIP
	}
	retryAfter := strconv.Itoa(retryAfterSeconds(k.Rate()))
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			if !k.Allow(key(r)) {
				w.Header().Set("Retry-After", retryAfter)
				http.Error(w, "rate limit exceeded", http.StatusTooManyRequests)
				return
			}
			next.ServeHTTP(w, r)
		})
	}
}

// retryAfterSeconds is the time one token takes to refill at rate, rounded up
// to whole seconds (the unit of Retry-After), and at least 1.
func retryAfterSeconds(rate float64) int {
	s := math.Ceil(1 / rate)
	if s < 1 {
		return 1
	}
	if s > math.MaxInt32 {
		return math.MaxInt32
	}
	return int(s)
}
