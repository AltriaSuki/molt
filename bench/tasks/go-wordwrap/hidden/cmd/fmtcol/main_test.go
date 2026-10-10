package main

import (
	"bytes"
	"errors"
	"strings"
	"testing"
)

func runFmtcol(t *testing.T, input string, args ...string) (stdout, stderr string, code int) {
	t.Helper()
	var out, errOut bytes.Buffer
	code = run(args, strings.NewReader(input), &out, &errOut)
	return out.String(), errOut.String(), code
}

func TestWrapsStdin(t *testing.T) {
	out, errOut, code := runFmtcol(t, "the quick brown fox jumps\nover the lazy dog\n", "-w", "12")
	if code != 0 {
		t.Fatalf("exit %d, stderr %q", code, errOut)
	}
	want := "the quick\nbrown fox\njumps over\nthe lazy dog\n"
	if out != want {
		t.Errorf("stdout = %q, want %q", out, want)
	}
}

func TestDefaultWidthIs72(t *testing.T) {
	in := strings.Repeat("abcde ", 20) // 20 five-letter words
	out, _, code := runFmtcol(t, in)
	if code != 0 {
		t.Fatalf("exit %d", code)
	}
	// 12 words take 12*5+11 = 71 columns; a 13th would need 77.
	line := strings.TrimSuffix(strings.Repeat("abcde ", 12), " ")
	want := line + "\n" + strings.TrimSuffix(strings.Repeat("abcde ", 8), " ") + "\n"
	if out != want {
		t.Errorf("stdout = %q, want %q", out, want)
	}
}

func TestPrefixDoesNotCountTowardsWidth(t *testing.T) {
	out, _, code := runFmtcol(t, "one two three four", "-w", "9", "-p", "> ")
	if code != 0 {
		t.Fatalf("exit %d", code)
	}
	want := "> one two\n> three\n> four\n"
	if out != want {
		t.Errorf("stdout = %q, want %q", out, want)
	}
}

func TestEmptyInputPrintsNothing(t *testing.T) {
	out, errOut, code := runFmtcol(t, "", "-w", "20")
	if code != 0 || out != "" || errOut != "" {
		t.Errorf("got exit %d, stdout %q, stderr %q; want 0, empty, empty", code, out, errOut)
	}
}

func TestBadUsage(t *testing.T) {
	tests := []struct {
		name    string
		args    []string
		wantErr string
	}{
		{"zero width", []string{"-w", "0"}, "width must be at least 1"},
		{"negative width", []string{"-w", "-5"}, "width must be at least 1"},
		{"not a number", []string{"-w", "wide"}, "invalid value"},
		{"stray argument", []string{"notes.txt"}, "unexpected argument"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			out, errOut, code := runFmtcol(t, "some text", tt.args...)
			if code != 2 {
				t.Errorf("exit = %d, want 2", code)
			}
			if out != "" {
				t.Errorf("stdout = %q, want nothing", out)
			}
			if !strings.Contains(errOut, tt.wantErr) {
				t.Errorf("stderr = %q, want it to mention %q", errOut, tt.wantErr)
			}
		})
	}
}

type failingReader struct{}

func (failingReader) Read([]byte) (int, error) { return 0, errors.New("disk on fire") }

func TestReadError(t *testing.T) {
	var out, errOut bytes.Buffer
	code := run(nil, failingReader{}, &out, &errOut)
	if code != 1 {
		t.Errorf("exit = %d, want 1", code)
	}
	if !strings.Contains(errOut.String(), "disk on fire") {
		t.Errorf("stderr = %q, want the read error", errOut.String())
	}
}
