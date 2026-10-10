package textfmt

import (
	"reflect"
	"strings"
	"testing"
)

func TestWrap(t *testing.T) {
	tests := []struct {
		name  string
		in    string
		width int
		want  string
	}{
		{"fits on one line", "hello world", 20, "hello world"},
		{"breaks between words", "the quick brown fox jumps over the lazy dog", 10,
			"the quick\nbrown fox\njumps over\nthe lazy\ndog"},
		{"exact fit", "aaaa bbbb", 9, "aaaa bbbb"},
		{"one short of exact fit", "aaaa bbbb", 8, "aaaa\nbbbb"},
		{"word as long as the width", "abcde fg", 5, "abcde\nfg"},
		{"newline inside a paragraph is a space", "one\ntwo three", 20, "one two three"},
		{"short lines are refilled", "a\nb\nc\nd", 3, "a b\nc d"},
		{"width one", "a b c", 1, "a\nb\nc"},
		{"empty input", "", 10, ""},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := Wrap(tt.in, tt.width); got != tt.want {
				t.Errorf("Wrap(%q, %d) = %q, want %q", tt.in, tt.width, got, tt.want)
			}
		})
	}
}

func TestWrapNonPositiveWidthReturnsInput(t *testing.T) {
	in := "  keep\n\n\nme   as I am \n"
	for _, width := range []int{0, -1, -80} {
		if got := Wrap(in, width); got != in {
			t.Errorf("Wrap(%q, %d) = %q, want the input unchanged", in, width, got)
		}
	}
}

const lorem = `Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do
eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim
veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea
commodo consequat.`

func TestWrapKeepsLinesWithinWidth(t *testing.T) {
	for width := 13; width <= 60; width++ {
		got := Wrap(lorem, width)
		for _, line := range strings.Split(got, "\n") {
			if len(line) > width {
				t.Errorf("width %d: line %q is %d characters long", width, line, len(line))
			}
		}
		if !reflect.DeepEqual(strings.Fields(got), strings.Fields(lorem)) {
			t.Errorf("width %d: words changed:\n%s", width, got)
		}
	}
}

func TestWrapIsGreedy(t *testing.T) {
	// Each line takes as many words as fit, so the first word of the next
	// line would not have fitted on the line before it.
	const width = 30
	lines := strings.Split(Wrap(lorem, width), "\n")
	for i := 1; i < len(lines); i++ {
		next := strings.Fields(lines[i])[0]
		if len(lines[i-1])+1+len(next) <= width {
			t.Errorf("%q would have fitted after %q", next, lines[i-1])
		}
	}
}
