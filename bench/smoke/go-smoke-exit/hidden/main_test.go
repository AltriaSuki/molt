package main

import "testing"

func TestGreeting(t *testing.T) {
	if got := greeting("Ada"); got != "hello, Ada" {
		t.Fatalf("greeting(Ada) = %q", got)
	}
}
