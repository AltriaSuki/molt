package textfmt

import "strings"

// Indent returns s with prefix added to the start of every non-empty line.
//
// Empty lines are left empty, so indenting never adds trailing whitespace,
// and a trailing newline in s stays a trailing newline (no prefix is added
// after it). Lines are separated by "\n"; a line holding only spaces is not
// empty and gets the prefix like any other.
func Indent(s, prefix string) string {
	if prefix == "" || s == "" {
		return s
	}
	var b strings.Builder
	b.Grow(len(s) + len(prefix)*(strings.Count(s, "\n")+1))
	for _, line := range strings.SplitAfter(s, "\n") {
		if line != "" && line != "\n" {
			b.WriteString(prefix)
		}
		b.WriteString(line)
	}
	return b.String()
}
