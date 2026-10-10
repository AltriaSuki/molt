package ratelimit

import (
	"testing"
	"time"
)

func TestNewStartsFull(t *testing.T) {
	l := New(1, 3, NewManualClock(epoch))
	for i := 0; i < 3; i++ {
		if !l.Allow() {
			t.Fatalf("Allow #%d = false on a fresh limiter with burst 3", i+1)
		}
	}
	if l.Allow() {
		t.Fatal("4th Allow = true, want false once the burst is spent")
	}
}

func TestRefillOverTime(t *testing.T) {
	clk := NewManualClock(epoch)
	l := New(2, 2, clk) // one token every 500ms
	l.Allow()
	l.Allow()

	clk.Advance(500 * time.Millisecond)
	if !l.Allow() {
		t.Fatal("Allow after 500ms at 2/s = false, want true")
	}
	if l.Allow() {
		t.Fatal("second Allow after 500ms = true, want false")
	}

	clk.Advance(time.Second)
	if !l.Allow() || !l.Allow() {
		t.Fatal("two Allows after another second = false, want true")
	}
	if l.Allow() {
		t.Fatal("third Allow = true, want false")
	}
}

func TestAllowN(t *testing.T) {
	l := New(1, 5, NewManualClock(epoch))
	if !l.AllowN(3) {
		t.Fatal("AllowN(3) with 5 tokens = false")
	}
	if !l.AllowN(2) {
		t.Fatal("AllowN(2) with 2 tokens = false")
	}
	if l.AllowN(1) {
		t.Fatal("AllowN(1) with 0 tokens = true")
	}
}

func TestRateAndBurst(t *testing.T) {
	l := New(2.5, 7, NewManualClock(epoch))
	if l.Rate() != 2.5 || l.Burst() != 7 {
		t.Fatalf("Rate(), Burst() = %v, %v; want 2.5, 7", l.Rate(), l.Burst())
	}
}

func TestNilClockMeansRealClock(t *testing.T) {
	l := New(1, 1, nil)
	if !l.Allow() {
		t.Fatal("Allow on a fresh limiter with the real clock = false")
	}
}

func TestNewPanicsOnBadArguments(t *testing.T) {
	cases := []struct {
		name  string
		rate  float64
		burst int
	}{
		{"zero rate", 0, 1},
		{"negative rate", -1, 1},
		{"zero burst", 1, 0},
		{"negative burst", 1, -3},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			defer func() {
				if recover() == nil {
					t.Fatalf("New(%v, %d) did not panic", tc.rate, tc.burst)
				}
			}()
			New(tc.rate, tc.burst, NewManualClock(epoch))
		})
	}
}
