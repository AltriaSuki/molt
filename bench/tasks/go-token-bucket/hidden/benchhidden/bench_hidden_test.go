package benchhidden

import (
	"net/http"
	"net/http/httptest"
	"reflect"
	"strconv"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"example.com/ratelimit"
)

// fakeClock is a Clock the tests move by hand.
type fakeClock struct {
	mu  sync.Mutex
	now time.Time
}

func newFakeClock() *fakeClock {
	return &fakeClock{now: time.Date(2025, 6, 1, 8, 0, 0, 0, time.UTC)}
}

func (c *fakeClock) Now() time.Time {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.now
}

func (c *fakeClock) Advance(d time.Duration) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.now = c.now.Add(d)
}

var _ ratelimit.Clock = (*fakeClock)(nil)

func wantTokens(t *testing.T, l *ratelimit.Limiter, want float64) {
	t.Helper()
	if got := l.Tokens(); got != want {
		t.Fatalf("Tokens() = %v, want %v", got, want)
	}
}

func wantReserve(t *testing.T, l *ratelimit.Limiter, n int, wantWait time.Duration, wantOK bool) {
	t.Helper()
	wait, ok := l.Reserve(n)
	if wait != wantWait || ok != wantOK {
		t.Fatalf("Reserve(%d) = (%v, %v), want (%v, %v)", n, wait, ok, wantWait, wantOK)
	}
}

// ---- Tokens and refill ----

func TestTokensStartsFull(t *testing.T) {
	l := ratelimit.New(1, 5, newFakeClock())
	wantTokens(t, l, 5)
}

func TestTokensReflectsSpendingAndRefill(t *testing.T) {
	clk := newFakeClock()
	l := ratelimit.New(2, 4, clk)
	if !l.AllowN(3) {
		t.Fatal("AllowN(3) on a full bucket of 4 = false")
	}
	wantTokens(t, l, 1)
	clk.Advance(250 * time.Millisecond) // +0.5
	wantTokens(t, l, 1.5)
	clk.Advance(500 * time.Millisecond) // +1
	wantTokens(t, l, 2.5)
}

func TestTokensDoesNotChangeBalanceWithoutTime(t *testing.T) {
	clk := newFakeClock()
	l := ratelimit.New(4, 8, clk)
	l.AllowN(5)
	for i := 0; i < 3; i++ {
		wantTokens(t, l, 3)
	}
}

func TestRefillCappedAtBurstAfterIdle(t *testing.T) {
	clk := newFakeClock()
	l := ratelimit.New(1, 3, clk)
	if !l.Allow() {
		t.Fatal("Allow on a fresh limiter = false")
	}
	clk.Advance(time.Hour)
	wantTokens(t, l, 3)
	if !l.AllowN(3) {
		t.Fatal("AllowN(3) after an idle hour = false")
	}
	if l.Allow() {
		t.Fatal("Allow after spending the whole burst = true; the bucket refilled past burst")
	}
}

func TestRefillCappedAtBurstWithoutTokens(t *testing.T) {
	// Same bug seen only through Allow/AllowN.
	clk := newFakeClock()
	l := ratelimit.New(10, 5, clk)
	l.AllowN(5)
	clk.Advance(10 * time.Minute)
	allowed := 0
	for i := 0; i < 20; i++ {
		if l.Allow() {
			allowed++
		}
	}
	if allowed != 5 {
		t.Fatalf("after a long idle period %d of 20 Allow calls succeeded, want burst = 5", allowed)
	}
}

func TestRefillCapAppliesToPartialSteps(t *testing.T) {
	clk := newFakeClock()
	l := ratelimit.New(2, 2, clk)
	l.Allow()                           // 1
	clk.Advance(2 * time.Second)        // would be 5 uncapped
	wantTokens(t, l, 2)                 // capped
	l.Allow()                           // 1
	clk.Advance(250 * time.Millisecond) // +0.5
	wantTokens(t, l, 1.5)
}

// ---- AllowN ----

func TestAllowNFailureLeavesBalance(t *testing.T) {
	l := ratelimit.New(1, 5, newFakeClock())
	if !l.AllowN(3) {
		t.Fatal("AllowN(3) with 5 tokens = false")
	}
	if l.AllowN(3) {
		t.Fatal("AllowN(3) with 2 tokens = true")
	}
	wantTokens(t, l, 2)
	if !l.AllowN(2) {
		t.Fatal("AllowN(2) after a refused AllowN(3) = false; the refused call spent tokens")
	}
	wantTokens(t, l, 0)
}

func TestAllowFailureWithFractionalBalance(t *testing.T) {
	clk := newFakeClock()
	l := ratelimit.New(2, 1, clk)
	l.Allow()
	clk.Advance(250 * time.Millisecond) // 0.5
	if l.Allow() {
		t.Fatal("Allow with 0.5 tokens = true")
	}
	wantTokens(t, l, 0.5)
	clk.Advance(250 * time.Millisecond) // 1.0
	if !l.Allow() {
		t.Fatal("Allow with 1 token = false; the refused Allow discarded the half token")
	}
}

func TestAllowNMoreThanBurst(t *testing.T) {
	l := ratelimit.New(1, 3, newFakeClock())
	if l.AllowN(4) {
		t.Fatal("AllowN(4) with burst 3 = true")
	}
	wantTokens(t, l, 3)
}

func TestAllowNNonPositive(t *testing.T) {
	l := ratelimit.New(1, 3, newFakeClock())
	l.Allow()
	for _, n := range []int{0, -1, -5} {
		if l.AllowN(n) {
			t.Errorf("AllowN(%d) = true, want false", n)
		}
	}
	wantTokens(t, l, 2)
}

// ---- clock going backwards ----

func TestClockBackwardsDoesNotAddTokens(t *testing.T) {
	clk := newFakeClock()
	l := ratelimit.New(1, 10, clk)
	if !l.AllowN(10) {
		t.Fatal("AllowN(10) on a full bucket of 10 = false")
	}
	clk.Advance(-5 * time.Second)
	if l.Allow() {
		t.Fatal("Allow after the clock stepped back = true")
	}
	wantTokens(t, l, 0)
	clk.Advance(5 * time.Second) // back where it was: nothing new has been earned
	wantTokens(t, l, 0)
	if l.Allow() {
		t.Fatal("Allow once the clock caught up again = true; the stepped-back interval was credited twice")
	}
	clk.Advance(2 * time.Second)
	wantTokens(t, l, 2)
}

func TestClockBackwardsPartialCatchUp(t *testing.T) {
	clk := newFakeClock()
	l := ratelimit.New(2, 10, clk)
	l.AllowN(10)
	clk.Advance(time.Second) // 2
	wantTokens(t, l, 2)
	clk.Advance(-3 * time.Second) // 2 seconds before creation
	wantTokens(t, l, 2)
	clk.Advance(2 * time.Second) // still 1s behind the latest reading
	wantTokens(t, l, 2)
	clk.Advance(1500 * time.Millisecond) // 0.5s past the latest reading: +1
	wantTokens(t, l, 3)
}

func TestClockBackwardsRightAfterNew(t *testing.T) {
	clk := newFakeClock()
	l := ratelimit.New(1, 2, clk)
	clk.Advance(-time.Minute)
	if !l.AllowN(2) {
		t.Fatal("AllowN(2) on a full bucket after a backwards step = false")
	}
	clk.Advance(time.Minute)
	wantTokens(t, l, 0)
	clk.Advance(time.Second)
	wantTokens(t, l, 1)
}

func TestClockBackwardsDoesNotTakeTokens(t *testing.T) {
	clk := newFakeClock()
	l := ratelimit.New(1, 4, clk)
	l.AllowN(2)
	clk.Advance(-time.Hour)
	wantTokens(t, l, 2)
	if !l.AllowN(2) {
		t.Fatal("AllowN(2) with 2 tokens after a backwards step = false")
	}
}

// ---- Reserve ----

func TestReserveFromFullBucket(t *testing.T) {
	l := ratelimit.New(2, 4, newFakeClock())
	wantReserve(t, l, 3, 0, true)
	wantTokens(t, l, 1)
	wantReserve(t, l, 1, 0, true) // exactly zero is not a debt
	wantTokens(t, l, 0)
}

func TestReserveGoesNegative(t *testing.T) {
	clk := newFakeClock()
	l := ratelimit.New(2, 4, clk)
	wantReserve(t, l, 4, 0, true)
	wantReserve(t, l, 3, 1500*time.Millisecond, true)
	wantTokens(t, l, -3)
	wantReserve(t, l, 1, 2*time.Second, true)
	wantTokens(t, l, -4)
}

func TestReservePartlyCovered(t *testing.T) {
	l := ratelimit.New(1, 5, newFakeClock())
	l.AllowN(4)                               // 1 left
	wantReserve(t, l, 3, 2*time.Second, true) // -2
	wantTokens(t, l, -2)
}

func TestReserveRefillsFirst(t *testing.T) {
	clk := newFakeClock()
	l := ratelimit.New(4, 4, clk)
	l.AllowN(4)
	clk.Advance(500 * time.Millisecond) // 2
	wantReserve(t, l, 3, 250*time.Millisecond, true)
	wantTokens(t, l, -1)
}

func TestReserveWaitRoundsUpToNanosecond(t *testing.T) {
	clk := newFakeClock()
	l := ratelimit.New(3, 1, clk)
	wantReserve(t, l, 1, 0, true)
	wait, ok := l.Reserve(1)
	if !ok || wait != 333333334*time.Nanosecond {
		t.Fatalf("Reserve(1) at rate 3 with an empty bucket = (%v, %v), want (333.333334ms, true)", wait, ok)
	}
	clk.Advance(wait - time.Nanosecond)
	if got := l.Tokens(); got >= 0 {
		t.Fatalf("Tokens() 1ns before the wait is over = %v, want < 0", got)
	}
	clk.Advance(time.Nanosecond)
	if got := l.Tokens(); got < 0 {
		t.Fatalf("Tokens() once the wait is over = %v, want >= 0", got)
	}
}

func TestReserveInvalidChangesNothing(t *testing.T) {
	clk := newFakeClock()
	l := ratelimit.New(1, 3, clk)
	l.Allow() // 2
	for _, n := range []int{0, -1, 4, 100} {
		wantReserve(t, l, n, 0, false)
	}
	wantTokens(t, l, 2)
	wantReserve(t, l, 3, time.Second, true) // n == burst is fine
	wantTokens(t, l, -1)
	wantReserve(t, l, 4, 0, false)
	wantTokens(t, l, -1)
}

func TestNegativeBalanceRefillsAndBlocksAllow(t *testing.T) {
	clk := newFakeClock()
	l := ratelimit.New(2, 2, clk)
	wantReserve(t, l, 2, 0, true)
	wantReserve(t, l, 2, time.Second, true) // -2
	if l.Allow() {
		t.Fatal("Allow with a negative balance = true")
	}
	wantTokens(t, l, -2)
	clk.Advance(1500 * time.Millisecond) // +3 -> 1
	wantTokens(t, l, 1)
	if l.AllowN(2) {
		t.Fatal("AllowN(2) with 1 token = true")
	}
	if !l.Allow() {
		t.Fatal("Allow with 1 token = false")
	}
	wantTokens(t, l, 0)
}

func TestNegativeBalanceCappedAfterIdle(t *testing.T) {
	clk := newFakeClock()
	l := ratelimit.New(1, 2, clk)
	l.Reserve(2)
	l.Reserve(2) // -2
	clk.Advance(time.Hour)
	wantTokens(t, l, 2)
}

func TestReserveIgnoresBackwardsClock(t *testing.T) {
	clk := newFakeClock()
	l := ratelimit.New(1, 2, clk)
	l.AllowN(2)
	clk.Advance(-10 * time.Second)
	wantReserve(t, l, 1, time.Second, true)
	clk.Advance(10 * time.Second) // back to the latest reading: still -1
	wantTokens(t, l, -1)
}

// ---- KeyedLimiter ----

func TestNewKeyedIdleTTL(t *testing.T) {
	k := ratelimit.NewKeyed(3, 7, 90*time.Second, newFakeClock())
	if k.IdleTTL() != 90*time.Second {
		t.Fatalf("IdleTTL() = %v, want 1m30s", k.IdleTTL())
	}
	if k.Rate() != 3 || k.Burst() != 7 {
		t.Fatalf("Rate(), Burst() = %v, %v; want 3, 7", k.Rate(), k.Burst())
	}
	l := k.Get("a")
	if l.Rate() != 3 || l.Burst() != 7 {
		t.Fatalf("key limiter has rate %v burst %d, want 3 and 7", l.Rate(), l.Burst())
	}
}

func TestNewKeyedPanicsOnBadRateOrBurst(t *testing.T) {
	for _, tc := range []struct {
		rate  float64
		burst int
	}{{0, 1}, {-2, 1}, {1, 0}} {
		func() {
			defer func() {
				if recover() == nil {
					t.Errorf("NewKeyed(%v, %d, ...) did not panic", tc.rate, tc.burst)
				}
			}()
			ratelimit.NewKeyed(tc.rate, tc.burst, time.Minute, newFakeClock())
		}()
	}
}

func TestNewKeyedNilClock(t *testing.T) {
	k := ratelimit.NewKeyed(1, 1, time.Minute, nil)
	if !k.Allow("a") {
		t.Fatal("Allow on a new key with the real clock = false")
	}
	if n := k.Sweep(); n != 0 {
		t.Fatalf("Sweep right after use with the real clock removed %d keys", n)
	}
}

func TestSweepRemovesIdleKeys(t *testing.T) {
	clk := newFakeClock()
	k := ratelimit.NewKeyed(1, 2, time.Minute, clk)
	k.Allow("10.0.0.1")
	k.Allow("10.0.0.2")
	clk.Advance(30 * time.Second)
	k.Allow("10.0.0.3")
	if n := k.Sweep(); n != 0 {
		t.Fatalf("Sweep after 30s removed %d keys, want 0", n)
	}
	clk.Advance(30 * time.Second) // .1 and .2 idle exactly 1m, .3 idle 30s
	if n := k.Sweep(); n != 2 {
		t.Fatalf("Sweep removed %d keys, want 2", n)
	}
	if got, want := k.Keys(), []string{"10.0.0.3"}; !reflect.DeepEqual(got, want) {
		t.Fatalf("Keys() after Sweep = %v, want %v", got, want)
	}
	if k.Len() != 1 {
		t.Fatalf("Len() after Sweep = %d, want 1", k.Len())
	}
	if n := k.Sweep(); n != 0 {
		t.Fatalf("second Sweep removed %d keys, want 0", n)
	}
	clk.Advance(30 * time.Second)
	if n := k.Sweep(); n != 1 {
		t.Fatalf("Sweep once .3 is idle 1m removed %d keys, want 1", n)
	}
	if k.Len() != 0 || len(k.Keys()) != 0 {
		t.Fatalf("after sweeping everything: Len() = %d, Keys() = %v", k.Len(), k.Keys())
	}
}

func TestSweepBoundary(t *testing.T) {
	clk := newFakeClock()
	k := ratelimit.NewKeyed(1, 1, 10*time.Second, clk)
	k.Allow("a")
	clk.Advance(10*time.Second - time.Nanosecond)
	if n := k.Sweep(); n != 0 {
		t.Fatalf("Sweep 1ns before the TTL removed %d keys, want 0", n)
	}
	clk.Advance(time.Nanosecond)
	if n := k.Sweep(); n != 1 {
		t.Fatalf("Sweep at exactly the TTL removed %d keys, want 1", n)
	}
}

func TestSweepUseRefreshesKey(t *testing.T) {
	clk := newFakeClock()
	k := ratelimit.NewKeyed(1, 1, time.Minute, clk)
	k.Allow("a")
	k.Allow("b")
	clk.Advance(50 * time.Second)
	k.Allow("a")     // a use
	k.AllowN("b", 5) // refused (more than burst), but still a use
	clk.Advance(50 * time.Second)
	if n := k.Sweep(); n != 0 {
		t.Fatalf("Sweep removed %d keys that were used 50s ago, want 0", n)
	}
	if got, want := k.Keys(), []string{"a", "b"}; !reflect.DeepEqual(got, want) {
		t.Fatalf("Keys() = %v, want %v", got, want)
	}
}

func TestSweepGetCountsAsUse(t *testing.T) {
	clk := newFakeClock()
	k := ratelimit.NewKeyed(1, 1, time.Minute, clk)
	k.Get("a") // creates it; never spends a token
	k.Get("b")
	clk.Advance(40 * time.Second)
	k.Get("a")
	clk.Advance(40 * time.Second)
	if n := k.Sweep(); n != 1 {
		t.Fatalf("Sweep removed %d keys, want 1 (only b is idle for a minute)", n)
	}
	if got, want := k.Keys(), []string{"a"}; !reflect.DeepEqual(got, want) {
		t.Fatalf("Keys() = %v, want %v", got, want)
	}
}

func TestSweptKeyStartsFresh(t *testing.T) {
	clk := newFakeClock()
	k := ratelimit.NewKeyed(0.001, 3, 10*time.Second, clk) // practically no refill
	if !k.AllowN("a", 3) {
		t.Fatal("AllowN(a, 3) on a new key = false")
	}
	old := k.Get("a")
	clk.Advance(10 * time.Second)
	if k.Allow("a") {
		t.Fatal("Allow(a) before Sweep = true; bucket should still be empty")
	}
	clk.Advance(10 * time.Second)
	if n := k.Sweep(); n != 1 {
		t.Fatalf("Sweep removed %d keys, want 1", n)
	}
	if k.Get("a") == old {
		t.Fatal("Get(a) after a was swept returned the old limiter")
	}
	if !k.AllowN("a", 3) {
		t.Fatal("AllowN(a, 3) after a was swept = false; want a fresh, full bucket")
	}
}

func TestSweepDisabledByNonPositiveTTL(t *testing.T) {
	for _, ttl := range []time.Duration{0, -time.Second} {
		clk := newFakeClock()
		k := ratelimit.NewKeyed(1, 1, ttl, clk)
		k.Allow("a")
		k.Get("b")
		clk.Advance(24 * time.Hour)
		if n := k.Sweep(); n != 0 {
			t.Errorf("idleTTL %v: Sweep removed %d keys, want 0", ttl, n)
		}
		if k.Len() != 2 {
			t.Errorf("idleTTL %v: Len() = %d after Sweep, want 2", ttl, k.Len())
		}
	}
}

func TestSweepEmpty(t *testing.T) {
	k := ratelimit.NewKeyed(1, 1, time.Second, newFakeClock())
	if n := k.Sweep(); n != 0 {
		t.Fatalf("Sweep on an empty KeyedLimiter = %d, want 0", n)
	}
}

func TestMiddlewareClientForgottenAfterSweep(t *testing.T) {
	clk := newFakeClock()
	k := ratelimit.NewKeyed(0.001, 1, time.Minute, clk)
	h := ratelimit.Middleware(k, nil)(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusNoContent)
	}))
	do := func() int {
		req := httptest.NewRequest(http.MethodGet, "/", nil)
		req.RemoteAddr = "198.51.100.4:4000"
		rec := httptest.NewRecorder()
		h.ServeHTTP(rec, req)
		return rec.Code
	}
	if c := do(); c != http.StatusNoContent {
		t.Fatalf("first request: status %d, want 204", c)
	}
	if c := do(); c != http.StatusTooManyRequests {
		t.Fatalf("second request: status %d, want 429", c)
	}
	clk.Advance(30 * time.Second)
	if c := do(); c != http.StatusTooManyRequests { // a refused request is still a use
		t.Fatalf("request after 30s: status %d, want 429", c)
	}
	clk.Advance(30 * time.Second)
	if n := k.Sweep(); n != 0 {
		t.Fatalf("Sweep 30s after the last request removed %d keys, want 0", n)
	}
	clk.Advance(30 * time.Second)
	if n := k.Sweep(); n != 1 {
		t.Fatalf("Sweep 60s after the last request removed %d keys, want 1", n)
	}
	if c := do(); c != http.StatusNoContent {
		t.Fatalf("request after the client was swept: status %d, want 204", c)
	}
}

// ---- concurrency ----

func TestConcurrentAllow(t *testing.T) {
	l := ratelimit.New(1, 40, newFakeClock())
	var allowed int64
	var wg sync.WaitGroup
	for g := 0; g < 16; g++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for i := 0; i < 10; i++ {
				if l.Allow() {
					atomic.AddInt64(&allowed, 1)
				}
				l.Tokens()
			}
		}()
	}
	wg.Wait()
	if allowed != 40 {
		t.Fatalf("%d concurrent Allow calls succeeded, want burst = 40", allowed)
	}
	wantTokens(t, l, 0)
}

func TestConcurrentReserve(t *testing.T) {
	l := ratelimit.New(1, 10, newFakeClock())
	var wg sync.WaitGroup
	for g := 0; g < 10; g++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for i := 0; i < 500; i++ {
				if _, ok := l.Reserve(1); !ok {
					t.Error("Reserve(1) = not ok")
					return
				}
			}
		}()
	}
	wg.Wait()
	wantTokens(t, l, -4990)
}

func TestConcurrentKeyedWithSweep(t *testing.T) {
	clk := newFakeClock()
	k := ratelimit.NewKeyed(1, 5, time.Hour, clk)
	keys := []string{"a", "b", "c", "d", "e", "f", "g", "h"}
	var allowed int64
	var wg sync.WaitGroup
	for g := 0; g < 16; g++ {
		wg.Add(1)
		go func(g int) {
			defer wg.Done()
			for i := 0; i < 20; i++ {
				if k.Allow(keys[(g+i)%len(keys)]) {
					atomic.AddInt64(&allowed, 1)
				}
				if i%5 == 0 {
					k.Sweep()
					k.Len()
					k.Keys()
				}
			}
		}(g)
	}
	wg.Wait()
	if allowed != int64(5*len(keys)) {
		t.Fatalf("%d Allow calls succeeded across %d keys with burst 5, want %d", allowed, len(keys), 5*len(keys))
	}
	if k.Len() != len(keys) {
		t.Fatalf("Len() = %d, want %d", k.Len(), len(keys))
	}
}

func TestConcurrentSweepWhileKeysAreAdded(t *testing.T) {
	clk := newFakeClock()
	k := ratelimit.NewKeyed(1, 1, time.Hour, clk)
	const workers, perWorker = 8, 2000
	var refused int64
	var wg sync.WaitGroup
	for g := 0; g < workers; g++ {
		wg.Add(1)
		go func(g int) {
			defer wg.Done()
			for i := 0; i < perWorker; i++ {
				if !k.Allow(strconv.Itoa(g*perWorker + i)) {
					atomic.AddInt64(&refused, 1)
				}
			}
		}(g)
	}
	done := make(chan struct{})
	swept := make(chan int)
	go func() {
		total := 0
		for {
			select {
			case <-done:
				swept <- total
				return
			default:
				total += k.Sweep()
				k.Len()
				k.Keys()
			}
		}
	}()
	wg.Wait()
	close(done)
	if n := <-swept; n != 0 {
		t.Fatalf("Sweep removed %d keys used less than idleTTL ago", n)
	}
	if refused != 0 {
		t.Fatalf("%d first Allow calls on new keys were refused", refused)
	}
	if got := k.Len(); got != workers*perWorker {
		t.Fatalf("Len() = %d, want %d", got, workers*perWorker)
	}
	clk.Advance(time.Hour)
	if n := k.Sweep(); n != workers*perWorker {
		t.Fatalf("Sweep after an idle hour removed %d keys, want %d", n, workers*perWorker)
	}
}
