package ratelimit

import (
	"reflect"
	"testing"
	"time"
)

func TestKeyedSeparateBuckets(t *testing.T) {
	k := NewKeyed(1, 2, NewManualClock(epoch))
	if !k.Allow("a") || !k.Allow("a") {
		t.Fatal("first two Allow(a) should succeed")
	}
	if k.Allow("a") {
		t.Fatal("third Allow(a) = true, want false")
	}
	if !k.Allow("b") {
		t.Fatal("Allow(b) = false; b has its own bucket")
	}
}

func TestKeyedGetReturnsSameLimiter(t *testing.T) {
	k := NewKeyed(1, 2, NewManualClock(epoch))
	a1 := k.Get("a")
	a2 := k.Get("a")
	if a1 != a2 {
		t.Fatal("Get(a) returned two different limiters")
	}
	if a1 == k.Get("b") {
		t.Fatal("Get(a) and Get(b) returned the same limiter")
	}
	if a1.Rate() != 1 || a1.Burst() != 2 {
		t.Fatalf("limiter has rate %v burst %d, want 1 and 2", a1.Rate(), a1.Burst())
	}
}

func TestKeyedAllowN(t *testing.T) {
	clk := NewManualClock(epoch)
	k := NewKeyed(1, 4, clk)
	if !k.AllowN("a", 4) {
		t.Fatal("AllowN(a, 4) on a fresh key = false")
	}
	if k.AllowN("a", 1) {
		t.Fatal("AllowN(a, 1) with an empty bucket = true")
	}
	clk.Advance(2 * time.Second)
	if !k.AllowN("a", 2) {
		t.Fatal("AllowN(a, 2) after 2s at 1/s = false")
	}
}

func TestKeyedLenAndKeys(t *testing.T) {
	k := NewKeyed(1, 1, NewManualClock(epoch))
	if k.Len() != 0 || len(k.Keys()) != 0 {
		t.Fatal("new KeyedLimiter is not empty")
	}
	k.Allow("10.0.0.2")
	k.Get("10.0.0.1")
	k.Allow("10.0.0.2")
	if k.Len() != 2 {
		t.Fatalf("Len() = %d, want 2", k.Len())
	}
	if got, want := k.Keys(), []string{"10.0.0.1", "10.0.0.2"}; !reflect.DeepEqual(got, want) {
		t.Fatalf("Keys() = %v, want %v", got, want)
	}
}

func TestNewKeyedPanicsOnBadArguments(t *testing.T) {
	defer func() {
		if recover() == nil {
			t.Fatal("NewKeyed with burst 0 did not panic")
		}
	}()
	NewKeyed(1, 0, NewManualClock(epoch))
}
