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
// A Limiter is safe for concurrent use.
type Limiter struct {
	rate  float64 // tokens added per second
	burst int     // bucket capacity
	clock Clock

	mu     sync.Mutex
	tokens float64   // balance as of last
	last   time.Time // clock reading the balance was last brought up to date at
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
// tokens and returns true; otherwise it returns false and the bucket is left
// as it was.
func (l *Limiter) AllowN(n int) bool {
	l.mu.Lock()
	defer l.mu.Unlock()
	l.refill()
	l.tokens -= float64(n)
	if l.tokens < 0 {
		l.tokens = 0
		return false
	}
	return true
}

// refill credits the tokens earned since the last refill. The caller must
// hold l.mu.
func (l *Limiter) refill() {
	now := l.clock.Now()
	elapsed := now.Sub(l.last).Seconds()
	if elapsed < 0 {
		// The clock went backwards (NTP step, VM migration). Don't let
		// that take tokens away.
		elapsed = 0
	}
	if l.tokens < float64(l.burst) {
		l.tokens += elapsed * l.rate
	}
	l.last = now
}
