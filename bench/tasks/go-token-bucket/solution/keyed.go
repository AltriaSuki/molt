package ratelimit

import (
	"sort"
	"sync"
	"time"
)

// KeyedLimiter keeps a separate Limiter for every key, typically one per
// client IP or API key. A key's Limiter is created the first time the key is
// seen, with the KeyedLimiter's rate, burst and clock.
//
// Every Get, Allow and AllowN records the clock's reading as the key's last
// use. Sweep forgets keys that have not been used for the idle TTL, so the
// next use of such a key starts again from a full bucket.
//
// A KeyedLimiter is safe for concurrent use.
type KeyedLimiter struct {
	rate    float64
	burst   int
	idleTTL time.Duration
	clock   Clock

	mu       sync.Mutex
	limiters map[string]*keyedEntry
}

type keyedEntry struct {
	limiter *Limiter
	lastUse time.Time
}

// NewKeyed returns a KeyedLimiter whose limiters refill at rate tokens per
// second and hold at most burst tokens, and whose Sweep removes keys unused
// for idleTTL or longer. An idleTTL of zero or less keeps keys forever. It
// panics on the same arguments New panics on. A nil clock means RealClock.
func NewKeyed(rate float64, burst int, idleTTL time.Duration, clock Clock) *KeyedLimiter {
	mustValidate(rate, burst)
	if clock == nil {
		clock = RealClock{}
	}
	return &KeyedLimiter{
		rate:     rate,
		burst:    burst,
		idleTTL:  idleTTL,
		clock:    clock,
		limiters: make(map[string]*keyedEntry),
	}
}

// Rate returns the refill rate of the limiters, in tokens per second.
func (k *KeyedLimiter) Rate() float64 { return k.rate }

// Burst returns the bucket capacity of the limiters.
func (k *KeyedLimiter) Burst() int { return k.burst }

// IdleTTL returns how long a key may go unused before Sweep removes it.
func (k *KeyedLimiter) IdleTTL() time.Duration { return k.idleTTL }

// Get returns the Limiter for key, creating it if the key is new, and records
// the use.
func (k *KeyedLimiter) Get(key string) *Limiter {
	k.mu.Lock()
	defer k.mu.Unlock()
	now := k.clock.Now()
	e, ok := k.limiters[key]
	if !ok {
		e = &keyedEntry{limiter: New(k.rate, k.burst, k.clock)}
		k.limiters[key] = e
	}
	e.lastUse = now
	return e.limiter
}

// Allow reports whether one event for key may happen now, spending a token
// from key's Limiter if so.
func (k *KeyedLimiter) Allow(key string) bool {
	return k.Get(key).Allow()
}

// AllowN reports whether n events for key may happen now, spending n tokens
// from key's Limiter if so.
func (k *KeyedLimiter) AllowN(key string, n int) bool {
	return k.Get(key).AllowN(n)
}

// Sweep removes every key whose last use was at least the idle TTL ago and
// returns how many it removed. With an idle TTL of zero or less it removes
// nothing.
func (k *KeyedLimiter) Sweep() int {
	if k.idleTTL <= 0 {
		return 0
	}
	k.mu.Lock()
	defer k.mu.Unlock()
	now := k.clock.Now()
	removed := 0
	for key, e := range k.limiters {
		if now.Sub(e.lastUse) >= k.idleTTL {
			delete(k.limiters, key)
			removed++
		}
	}
	return removed
}

// Len returns the number of keys that currently have a Limiter.
func (k *KeyedLimiter) Len() int {
	k.mu.Lock()
	defer k.mu.Unlock()
	return len(k.limiters)
}

// Keys returns the keys that currently have a Limiter, sorted.
func (k *KeyedLimiter) Keys() []string {
	k.mu.Lock()
	keys := make([]string, 0, len(k.limiters))
	for key := range k.limiters {
		keys = append(keys, key)
	}
	k.mu.Unlock()
	sort.Strings(keys)
	return keys
}
