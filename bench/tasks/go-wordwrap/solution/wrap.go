package textfmt

import (
	"strings"
	"unicode/utf8"
)

// Wrap reflows s so that no line is longer than width characters, counting
// runes rather than bytes.
//
// The text is split into paragraphs at blank lines (lines that are empty or
// hold only spaces and tabs) and each paragraph is filled on its own: its
// words are packed greedily onto lines, separated by single spaces, so a
// line break inside a paragraph counts as ordinary whitespace. A word longer
// than width is cut into pieces of width runes, each on a line of its own;
// the words after it may share the line of its last piece. Paragraphs are
// separated by exactly one empty line, no line starts or ends with
// whitespace, and the result has no trailing newline.
//
// If width is zero or negative, Wrap returns s unchanged.
func Wrap(s string, width int) string {
	if width <= 0 {
		return s
	}
	var out []string
	for i, para := range paragraphs(s) {
		if i > 0 {
			out = append(out, "")
		}
		out = append(out, fill(words(para), width)...)
	}
	return strings.Join(out, "\n")
}

// fill packs words greedily onto lines of at most width runes and returns
// the lines.
func fill(words []string, width int) []string {
	var lines []string
	var line strings.Builder
	n := 0 // runes on the current line
	flush := func() {
		if n > 0 {
			lines = append(lines, line.String())
			line.Reset()
			n = 0
		}
	}
	for _, w := range words {
		wn := utf8.RuneCountInString(w)
		switch {
		case wn > width:
			// Too long for any line: cut it into width-rune pieces, each
			// on its own line. The last piece becomes the current line.
			flush()
			for wn > width {
				cut := runeOffset(w, width)
				lines = append(lines, w[:cut])
				w, wn = w[cut:], wn-width
			}
		case n > 0 && n+1+wn > width:
			flush()
		}
		if n > 0 {
			line.WriteByte(' ')
			n++
		}
		line.WriteString(w)
		n += wn
	}
	flush()
	return lines
}

// runeOffset returns the byte offset of the rune at index i in s, or len(s)
// if s has no more than i runes.
func runeOffset(s string, i int) int {
	for off := range s {
		if i == 0 {
			return off
		}
		i--
	}
	return len(s)
}
