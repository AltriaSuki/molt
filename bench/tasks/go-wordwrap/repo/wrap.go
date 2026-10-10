package textfmt

import "strings"

// Wrap reflows s so that no line is longer than width characters.
//
// The text is split into paragraphs at blank lines and each paragraph is
// filled on its own: its words are packed greedily onto lines, separated by
// single spaces, so a line break inside a paragraph counts as ordinary
// whitespace. The result has no trailing newline.
//
// If width is zero or negative, Wrap returns s unchanged.
func Wrap(s string, width int) string {
	if width <= 0 {
		return s
	}
	var out []string
	for _, para := range paragraphs(s) {
		out = append(out, fill(words(para), width)...)
	}
	return strings.Join(out, "\n")
}

// fill packs words greedily onto lines of at most width characters and
// returns the lines.
func fill(words []string, width int) []string {
	var lines []string
	var line strings.Builder
	for _, w := range words {
		if line.Len() > 0 && line.Len()+1+len(w) > width {
			lines = append(lines, line.String())
			line.Reset()
		}
		if line.Len() > 0 {
			line.WriteByte(' ')
		}
		line.WriteString(w)
	}
	if line.Len() > 0 {
		lines = append(lines, line.String())
	}
	return lines
}
