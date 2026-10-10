// Command fmtcol reflows text read from standard input to a column width.
//
// Usage:
//
//	fmtcol [-w width] [-p prefix] < input
//
// The input is wrapped with textfmt.Wrap to at most width characters per
// line (72 by default). With -p, every non-empty output line is then
// prefixed with the given string, which does not count towards the width:
//
//	git log -1 --format=%B | fmtcol -w 60 -p '> '
//
// Output ends with a newline unless it is empty.
package main

import (
	"flag"
	"fmt"
	"io"
	"os"

	"example.com/textfmt"
)

const defaultWidth = 72

func main() {
	os.Exit(run(os.Args[1:], os.Stdin, os.Stdout, os.Stderr))
}

// run is main without the process around it: it parses args, filters stdin
// to stdout and returns the exit status (0 ok, 1 I/O error, 2 bad usage).
func run(args []string, stdin io.Reader, stdout, stderr io.Writer) int {
	fs := flag.NewFlagSet("fmtcol", flag.ContinueOnError)
	fs.SetOutput(stderr)
	fs.Usage = func() {
		fmt.Fprintln(stderr, "usage: fmtcol [-w width] [-p prefix] < input")
		fs.PrintDefaults()
	}
	width := fs.Int("w", defaultWidth, "maximum line `width` in characters, not counting the prefix")
	prefix := fs.String("p", "", "`prefix` added to every non-empty output line")
	if err := fs.Parse(args); err != nil {
		return 2
	}
	if fs.NArg() > 0 {
		fmt.Fprintf(stderr, "fmtcol: unexpected argument %q (input is read from stdin)\n", fs.Arg(0))
		return 2
	}
	if *width < 1 {
		fmt.Fprintf(stderr, "fmtcol: width must be at least 1, got %d\n", *width)
		return 2
	}

	in, err := io.ReadAll(stdin)
	if err != nil {
		fmt.Fprintf(stderr, "fmtcol: reading input: %v\n", err)
		return 1
	}
	out := textfmt.Indent(textfmt.Wrap(string(in), *width), *prefix)
	if out != "" {
		out += "\n"
	}
	if _, err := io.WriteString(stdout, out); err != nil {
		fmt.Fprintf(stderr, "fmtcol: writing output: %v\n", err)
		return 1
	}
	return 0
}
