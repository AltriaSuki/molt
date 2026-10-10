# textfmt

Small plain-text formatting helpers for Go, standard library only. We use
them to lay out release notes, commit message bodies and CLI help text.

- `Wrap(s string, width int) string` (`wrap.go`, `paragraph.go`) reflows text
  so that no line is longer than `width`. Paragraphs are separated by blank
  lines and filled independently; inside a paragraph, line breaks are just
  whitespace. `width <= 0` returns the text unchanged.
- `Indent(s, prefix string) string` (`indent.go`) adds `prefix` to every
  non-empty line, e.g. to quote a reply or nest text under a bullet.

```go
body := textfmt.Wrap(notes, 72)
fmt.Println(textfmt.Indent(body, "    "))
```

## fmtcol

`cmd/fmtcol` is a filter around the two functions:

```
go run ./cmd/fmtcol -w 60 -p '> ' < notes.txt
```

`-w` is the width (default 72), `-p` an optional prefix that does not count
towards the width.

## Running the tests

Go 1.21 or newer, no dependencies:

```
go test ./ ./cmd/fmtcol/
```
