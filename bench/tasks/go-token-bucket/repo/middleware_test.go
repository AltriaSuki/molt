package ratelimit

import (
	"net/http"
	"net/http/httptest"
	"testing"
	"time"
)

var okHandler = http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
	w.WriteHeader(http.StatusOK)
})

func request(h http.Handler, remoteAddr string, header http.Header) *httptest.ResponseRecorder {
	req := httptest.NewRequest(http.MethodGet, "/api/items", nil)
	req.RemoteAddr = remoteAddr
	for name, values := range header {
		for _, v := range values {
			req.Header.Add(name, v)
		}
	}
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	return rec
}

func TestMiddlewareLimitsPerClient(t *testing.T) {
	k := NewKeyed(1, 2, NewManualClock(epoch))
	h := Middleware(k, nil)(okHandler)

	for i := 0; i < 2; i++ {
		if rec := request(h, "10.0.0.1:5000", nil); rec.Code != http.StatusOK {
			t.Fatalf("request %d: status %d, want 200", i+1, rec.Code)
		}
	}
	rec := request(h, "10.0.0.1:5001", nil) // same client, new port
	if rec.Code != http.StatusTooManyRequests {
		t.Fatalf("third request: status %d, want 429", rec.Code)
	}
	if got := rec.Header().Get("Retry-After"); got != "1" {
		t.Fatalf("Retry-After = %q, want %q", got, "1")
	}
	if rec := request(h, "10.0.0.2:5000", nil); rec.Code != http.StatusOK {
		t.Fatalf("other client: status %d, want 200", rec.Code)
	}
}

func TestMiddlewareRecoversWithTime(t *testing.T) {
	clk := NewManualClock(epoch)
	k := NewKeyed(0.5, 1, clk)
	h := Middleware(k, nil)(okHandler)

	request(h, "10.0.0.1:5000", nil)
	rec := request(h, "10.0.0.1:5000", nil)
	if rec.Code != http.StatusTooManyRequests {
		t.Fatalf("second request: status %d, want 429", rec.Code)
	}
	if got := rec.Header().Get("Retry-After"); got != "2" {
		t.Fatalf("Retry-After = %q, want %q", got, "2")
	}
	clk.Advance(2 * time.Second)
	if rec := request(h, "10.0.0.1:5000", nil); rec.Code != http.StatusOK {
		t.Fatalf("after 2s: status %d, want 200", rec.Code)
	}
}

func TestMiddlewareRetryAfterRoundsUp(t *testing.T) {
	for _, tc := range []struct {
		rate float64
		want string
	}{
		{100, "1"},
		{1, "1"},
		{0.4, "3"},
		{0.25, "4"},
	} {
		k := NewKeyed(tc.rate, 1, NewManualClock(epoch))
		h := Middleware(k, nil)(okHandler)
		request(h, "10.0.0.1:5000", nil)
		rec := request(h, "10.0.0.1:5000", nil)
		if got := rec.Header().Get("Retry-After"); got != tc.want {
			t.Errorf("rate %v: Retry-After = %q, want %q", tc.rate, got, tc.want)
		}
	}
}

func TestMiddlewareCustomKey(t *testing.T) {
	k := NewKeyed(1, 1, NewManualClock(epoch))
	byToken := func(r *http.Request) string { return r.Header.Get("X-Api-Token") }
	h := Middleware(k, byToken)(okHandler)

	alice := http.Header{"X-Api-Token": {"alice"}}
	if rec := request(h, "10.0.0.1:5000", alice); rec.Code != http.StatusOK {
		t.Fatalf("alice #1: status %d, want 200", rec.Code)
	}
	if rec := request(h, "10.0.0.9:5000", alice); rec.Code != http.StatusTooManyRequests {
		t.Fatalf("alice #2 from another IP: status %d, want 429", rec.Code)
	}
	bob := http.Header{"X-Api-Token": {"bob"}}
	if rec := request(h, "10.0.0.1:5000", bob); rec.Code != http.StatusOK {
		t.Fatalf("bob: status %d, want 200", rec.Code)
	}
}

func TestClientIP(t *testing.T) {
	for _, tc := range []struct{ remote, want string }{
		{"192.0.2.10:443", "192.0.2.10"},
		{"[2001:db8::1]:8080", "2001:db8::1"},
		{"192.0.2.10", "192.0.2.10"},
	} {
		req := httptest.NewRequest(http.MethodGet, "/", nil)
		req.RemoteAddr = tc.remote
		if got := ClientIP(req); got != tc.want {
			t.Errorf("ClientIP(%q) = %q, want %q", tc.remote, got, tc.want)
		}
	}
}

func TestForwardedFor(t *testing.T) {
	for _, tc := range []struct{ xff, want string }{
		{"203.0.113.7", "203.0.113.7"},
		{"203.0.113.7, 10.0.0.1", "203.0.113.7"},
		{" 203.0.113.7 ,10.0.0.1", "203.0.113.7"},
		{"", "192.0.2.10"},
	} {
		req := httptest.NewRequest(http.MethodGet, "/", nil)
		req.RemoteAddr = "192.0.2.10:443"
		if tc.xff != "" {
			req.Header.Set("X-Forwarded-For", tc.xff)
		}
		if got := ForwardedFor(req); got != tc.want {
			t.Errorf("ForwardedFor with X-Forwarded-For %q = %q, want %q", tc.xff, got, tc.want)
		}
	}
}
