package ratelimit

import (
	"sort"
	"sync"
)

// KeyedLimiter keeps a separate Limiter for every key, typically one per
// client IP or API key. A key's Limiter is created the first time the key is
// seen, with the KeyedLimiter's rate, burst and clock.
//
// A KeyedLimiter is safe for concurrent use.
type KeyedLimiter struct {
	rate  float64
	burst int
	clock Clock

	mu       sync.Mutex
	limiters map[string]*Limiter
}

// NewKeyed returns a KeyedLimiter whose limiters refill at rate tokens per
// second and hold at most burst tokens. It panics on the same arguments New
// panics on. A nil clock means RealClock.
func NewKeyed(rate float64, burst int, clock Clock) *KeyedLimiter {
	mustValidate(rate, burst)
	if clock == nil {
		clock = RealClock{}
	}
	return &KeyedLimiter{
		rate:     rate,
		burst:    burst,
		clock:    clock,
		limiters: make(map[string]*Limiter),
	}
}

// Rate returns the refill rate of the limiters, in tokens per second.
func (k *KeyedLimiter) Rate() float64 { return k.rate }

// Burst returns the bucket capacity of the limiters.
func (k *KeyedLimiter) Burst() int { return k.burst }

// Get returns the Limiter for key, creating it if the key is new.
func (k *KeyedLimiter) Get(key string) *Limiter {
	k.mu.Lock()
	defer k.mu.Unlock()
	l, ok := k.limiters[key]
	if !ok {
		l = New(k.rate, k.burst, k.clock)
		k.limiters[key] = l
	}
	return l
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
