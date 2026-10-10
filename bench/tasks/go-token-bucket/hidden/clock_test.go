package ratelimit

import (
	"testing"
	"time"
)

var epoch = time.Date(2024, 3, 1, 12, 0, 0, 0, time.UTC)

func TestManualClockStaysPut(t *testing.T) {
	c := NewManualClock(epoch)
	if got := c.Now(); !got.Equal(epoch) {
		t.Fatalf("Now() = %v, want %v", got, epoch)
	}
	if got := c.Now(); !got.Equal(epoch) {
		t.Fatalf("second Now() = %v, want %v", got, epoch)
	}
}

func TestManualClockAdvanceAndSet(t *testing.T) {
	c := NewManualClock(epoch)
	c.Advance(90 * time.Second)
	if got, want := c.Now(), epoch.Add(90*time.Second); !got.Equal(want) {
		t.Fatalf("after Advance: Now() = %v, want %v", got, want)
	}
	c.Advance(-30 * time.Second)
	if got, want := c.Now(), epoch.Add(time.Minute); !got.Equal(want) {
		t.Fatalf("after negative Advance: Now() = %v, want %v", got, want)
	}
	c.Set(epoch)
	if got := c.Now(); !got.Equal(epoch) {
		t.Fatalf("after Set: Now() = %v, want %v", got, epoch)
	}
}

func TestRealClockIsNotZero(t *testing.T) {
	if (RealClock{}).Now().IsZero() {
		t.Fatal("RealClock.Now() returned the zero time")
	}
}
