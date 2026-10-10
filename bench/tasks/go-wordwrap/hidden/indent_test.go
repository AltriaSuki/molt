package textfmt

import "testing"

func TestIndent(t *testing.T) {
	tests := []struct {
		name   string
		in     string
		prefix string
		want   string
	}{
		{"single line", "hello", "> ", "> hello"},
		{"several lines", "a\nb\nc", "  ", "  a\n  b\n  c"},
		{"empty lines stay empty", "a\n\nb", "> ", "> a\n\n> b"},
		{"trailing newline kept", "a\nb\n", "# ", "# a\n# b\n"},
		{"leading newline", "\na", "| ", "\n| a"},
		{"whitespace-only line is prefixed", "a\n  \nb", "> ", "> a\n>   \n> b"},
		{"empty prefix", "a\nb", "", "a\nb"},
		{"empty input", "", "> ", ""},
		{"only newlines", "\n\n", "> ", "\n\n"},
		{"multibyte prefix", "x\ny", "│ ", "│ x\n│ y"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := Indent(tt.in, tt.prefix); got != tt.want {
				t.Errorf("Indent(%q, %q) = %q, want %q", tt.in, tt.prefix, got, tt.want)
			}
		})
	}
}

func TestIndentAfterWrap(t *testing.T) {
	got := Indent(Wrap("Please review the attached proposal before Friday.", 20), "> ")
	want := "> Please review the\n> attached proposal\n> before Friday."
	if got != want {
		t.Errorf("got %q, want %q", got, want)
	}
}
