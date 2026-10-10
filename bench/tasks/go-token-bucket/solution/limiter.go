// Package ratelimit implements token-bucket rate limiting: a Limiter for a
// single stream of events, a KeyedLimiter that keeps one Limiter per client,
// and HTTP middleware built on top of them.
package ratelimit

import (
	"fmt"
	"math"
	"sync"
	"time"
)

// A Limiter controls how often events may happen. It holds a bucket of up to
// burst tokens that refills continuously at rate tokens per second; an event
// costs one token. A new Limiter starts with a full bucket.
//
// Reserve may take the balance below zero; the bucket then refills from the
// negative balance at the same rate.
//
// A Limiter is safe for concurrent use.
type Limiter struct {
	rate  float64 // tokens added per second
	burst int     // bucket capacity
	clock Clock

	mu     sync.Mutex
	tokens float64   // balance as of last
	last   time.Time // latest clock reading seen; the balance is up to date as of it
}

// New returns a Limiter that refills at rate tokens per second and holds at
// most burst tokens. It panics if rate is not a positive, finite number or if
// burst is less than 1. A nil clock means RealClock.
func New(rate float64, burst int, clock Clock) *Limiter {
	mustValidate(rate, burst)
	if clock == nil {
		clock = RealClock{}
	}
	return &Limiter{
		rate:   rate,
		burst:  burst,
		clock:  clock,
		tokens: float64(burst),
		last:   clock.Now(),
	}
}

func mustValidate(rate float64, burst int) {
	if !(rate > 0) || math.IsInf(rate, 1) {
		panic(fmt.Sprintf("ratelimit: rate must be a positive, finite number, got %v", rate))
	}
	if burst < 1 {
		panic(fmt.Sprintf("ratelimit: burst must be at least 1, got %d", burst))
	}
}

// Rate returns the refill rate in tokens per second.
func (l *Limiter) Rate() float64 { return l.rate }

// Burst returns the bucket capacity.
func (l *Limiter) Burst() int { return l.burst }

// Allow reports whether one event may happen now, spending a token if so.
// It is shorthand for AllowN(1).
func (l *Limiter) Allow() bool { return l.AllowN(1) }

// AllowN reports whether n events may happen now. If they may, it spends n
// tokens and returns true; otherwise, or if n < 1, it returns false and the
// bucket is left as it was.
func (l *Limiter) AllowN(n int) bool {
	if n < 1 {
		return false
	}
	l.mu.Lock()
	defer l.mu.Unlock()
	l.refill()
	if l.tokens < float64(n) {
		return false
	}
	l.tokens -= float64(n)
	return true
}

// Reserve spends n tokens now, even if that takes the balance below zero, and
// returns how long the bucket needs to refill back to a balance of zero (0 if
// the balance did not go negative). It returns 0, false and changes nothing
// if n < 1 or n > Burst(), since such a reservation could never be met.
func (l *Limiter) Reserve(n int) (wait time.Duration, ok bool) {
	if n < 1 || n > l.burst {
		return 0, false
	}
	l.mu.Lock()
	defer l.mu.Unlock()
	l.refill()
	l.tokens -= float64(n)
	if l.tokens >= 0 {
		return 0, true
	}
	return l.timeToEarn(-l.tokens), true
}

// Tokens returns the current balance, which is negative while reservations
// are outstanding.
func (l *Limiter) Tokens() float64 {
	l.mu.Lock()
	defer l.mu.Unlock()
	l.refill()
	return l.tokens
}

// refill credits the tokens earned since the latest clock reading the limiter
// has seen, up to burst. A reading earlier than that one (the clock went
// backwards) earns nothing and does not move the limiter's reference time, so
// the interval is not credited a second time when the clock catches up. The
// caller must hold l.mu.
func (l *Limiter) refill() {
	now := l.clock.Now()
	if !now.After(l.last) {
		return
	}
	l.tokens = math.Min(float64(l.burst), l.tokens+now.Sub(l.last).Seconds()*l.rate)
	l.last = now
}

// timeToEarn returns how long the limiter takes to earn tokens, rounded up to
// a whole nanosecond.
func (l *Limiter) timeToEarn(tokens float64) time.Duration {
	ns := math.Ceil(tokens / l.rate * float64(time.Second))
	if ns >= math.MaxInt64 {
		return time.Duration(math.MaxInt64)
	}
	return time.Duration(ns)
}
