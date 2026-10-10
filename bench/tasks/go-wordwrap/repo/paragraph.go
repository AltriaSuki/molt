package textfmt

import "strings"

// paragraphs splits s into paragraphs. A paragraph is a run of consecutive
// non-blank lines; the blank lines between paragraphs are not returned.
func paragraphs(s string) [][]string {
	var paras [][]string
	var cur []string
	for _, line := range strings.Split(s, "\n") {
		if isBlank(line) {
			if len(cur) > 0 {
				paras = append(paras, cur)
				cur = nil
			}
			continue
		}
		cur = append(cur, line)
	}
	if len(cur) > 0 {
		paras = append(paras, cur)
	}
	return paras
}

// isBlank reports whether line is a paragraph separator.
func isBlank(line string) bool {
	return line == ""
}

// words returns the words of a paragraph's lines, in order. The break
// between two lines separates words just like a space does.
func words(lines []string) []string {
	var ws []string
	for _, line := range lines {
		ws = append(ws, strings.Split(strings.TrimSpace(line), " ")...)
	}
	return ws
}
