package ratelimit

import (
	"sync"
	"time"
)

// Clock is where limiters get the time from. Limiters never call time.Now
// themselves, so tests and simulations can drive them with a ManualClock.
type Clock interface {
	Now() time.Time
}

// RealClock is a Clock backed by the system clock.
type RealClock struct{}

// Now returns time.Now().
func (RealClock) Now() time.Time { return time.Now() }

// ManualClock is a Clock that only moves when told to. It is meant for tests
// and is safe for concurrent use.
type ManualClock struct {
	mu  sync.Mutex
	now time.Time
}

// NewManualClock returns a ManualClock that reads start until it is moved.
func NewManualClock(start time.Time) *ManualClock {
	return &ManualClock{now: start}
}

// Now returns the clock's current reading.
func (c *ManualClock) Now() time.Time {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.now
}

// Advance moves the clock by d. A negative d moves it backwards, the way a
// host clock does when NTP steps it.
func (c *ManualClock) Advance(d time.Duration) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.now = c.now.Add(d)
}

// Set moves the clock to t.
func (c *ManualClock) Set(t time.Time) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.now = t
}
