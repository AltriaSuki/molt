// Package textfmt formats plain text for terminals, commit messages and
// release notes: Wrap reflows paragraphs to a column width and Indent
// prefixes lines, for quoting or nesting text.
//
// The fmtcol command (cmd/fmtcol) puts both behind a small filter that reads
// standard input and writes the formatted text to standard output.
package textfmt
