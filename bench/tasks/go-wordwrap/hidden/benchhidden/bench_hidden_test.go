package benchhidden

import (
	"strings"
	"testing"
	"unicode/utf8"

	"example.com/textfmt"
)

type wrapCase struct {
	name  string
	in    string
	width int
	want  string
}

func runWrapCases(t *testing.T, cases []wrapCase) {
	t.Helper()
	for _, tc := range cases {
		tc := tc
		t.Run(tc.name, func(t *testing.T) {
			if got := textfmt.Wrap(tc.in, tc.width); got != tc.want {
				t.Errorf("Wrap(%q, %d)\n got  %q\n want %q", tc.in, tc.width, got, tc.want)
			}
		})
	}
}

func TestBasicFilling(t *testing.T) {
	runWrapCases(t, []wrapCase{
		{"fits", "hello world", 11, "hello world"},
		{"greedy", "the quick brown fox jumps over the lazy dog", 10,
			"the quick\nbrown fox\njumps over\nthe lazy\ndog"},
		{"one short of fitting", "aaaa bbbb", 8, "aaaa\nbbbb"},
		{"word equal to width is not split", "abcd ef", 4, "abcd\nef"},
		{"width one", "ab c", 1, "a\nb\nc"},
		{"huge width", "one two\nthree", 1000, "one two three"},
		{"newline inside paragraph", "one\ntwo\nthree", 20, "one two three"},
	})
}

func TestWidthCountsRunes(t *testing.T) {
	runWrapCases(t, []wrapCase{
		{"latin accents exact fit", "Zürich ist schön und groß", 10, "Zürich ist\nschön und\ngroß"},
		{"latin accents one line", "café crème", 10, "café crème"},
		{"greek", "αβγ δεζ ηθι", 7, "αβγ δεζ\nηθι"},
		{"cyrillic", "привет мир как дела", 10, "привет мир\nкак дела"},
		{"cjk", "日本語 の 文章", 6, "日本語 の\n文章"},
		{"cjk exact", "日本語の文章", 6, "日本語の文章"},
		{"emoji", "😀😀 ok 🎉", 5, "😀😀 ok\n🎉"},
		{"mixed", "naïve façade coöperate", 12, "naïve façade\ncoöperate"},
	})
}

func TestLongWords(t *testing.T) {
	runWrapCases(t, []wrapCase{
		{"alone", "abcdefghij", 4, "abcd\nefgh\nij"},
		{"exact multiple", "abcdefgh", 4, "abcd\nefgh"},
		{"one longer than width", "abcde", 4, "abcd\ne"},
		{"starts its own line", "a bcdefgh", 5, "a\nbcdef\ngh"},
		{"following words join the last piece", "go supercalifragilistic now", 6,
			"go\nsuperc\nalifra\ngilist\nic now"},
		{"following word too long for the last piece", "abcdefghijk xyz", 6, "abcdef\nghijk\nxyz"},
		{"following words after exact multiple", "abcdef gh", 3, "abc\ndef\ngh"},
		{"two long words in a row", "abcdefg hijklmn", 3, "abc\ndef\ng\nhij\nklm\nn"},
		{"width one", "abc de", 1, "a\nb\nc\nd\ne"},
		{"url in a sentence", "see https://example.com/a/very/long/path for details", 16,
			"see\nhttps://example.\ncom/a/very/long/\npath for details"},
		{"cyrillic pieces", "достопримечательность рядом", 8, "достопри\nмечатель\nность\nрядом"},
		{"cyrillic last piece joined", "достопримечательность тут", 9, "достоприм\nечательно\nсть тут"},
		{"cjk pieces", "東京特許許可局 は", 3, "東京特\n許許可\n局 は"},
		{"emoji pieces", "😀😀😀😀😀", 2, "😀😀\n😀😀\n😀"},
		{"long word ends paragraph", "abcdefg\n\nhi", 3, "abc\ndef\ng\n\nhi"},
	})
}

func TestParagraphs(t *testing.T) {
	runWrapCases(t, []wrapCase{
		{"two paragraphs", "a b\n\nc d", 10, "a b\n\nc d"},
		{"paragraphs are filled separately", "one two three\n\nfour five six", 9,
			"one two\nthree\n\nfour five\nsix"},
		{"many blank lines become one", "a\n\n\n\nb", 10, "a\n\nb"},
		{"whitespace-only line separates", "a\n \t \nb", 10, "a\n\nb"},
		{"tab-only line separates", "first para\n\t\nsecond para", 20, "first para\n\nsecond para"},
		{"run of mixed blank lines", "a\n\n   \n\t\n\nb\nc\n\n\nd", 10, "a\n\nb c\n\nd"},
		{"leading blank lines dropped", "\n\n  \nhello", 10, "hello"},
		{"trailing blank lines dropped", "hello\n\n \n", 10, "hello"},
		{"trailing newline dropped", "hello world\n", 20, "hello world"},
		{"three paragraphs", "x\n\ny\n\nz", 1, "x\n\ny\n\nz"},
	})
}

func TestWhitespace(t *testing.T) {
	runWrapCases(t, []wrapCase{
		{"runs of spaces collapse", "a   b    c", 20, "a b c"},
		{"tabs separate words", "a\tb\t\tc", 20, "a b c"},
		{"mixed spaces and tabs", "a \t b\t \tc", 20, "a b c"},
		{"leading and trailing whitespace on lines", "   indented\n\tline   \n  here\t", 30, "indented line here"},
		{"double space after full stop at a break", "First sentence.  Second sentence.", 16,
			"First sentence.\nSecond sentence."},
		{"double space after full stop mid line", "Stop.  Go.", 20, "Stop. Go."},
		{"trailing space at exact width", "abcd  efgh", 5, "abcd\nefgh"},
		{"tab at a break", "abcd\tefgh", 4, "abcd\nefgh"},
		{"empty", "", 10, ""},
		{"only spaces", "   ", 10, ""},
		{"only tabs and newlines", "\t\n\n \t\n", 10, ""},
		{"only newlines", "\n\n\n", 5, ""},
	})
}

func TestNoBreakSpaceIsPartOfAWord(t *testing.T) {
	runWrapCases(t, []wrapCase{
		{"glued units", "it is 10\u00a0km away", 8, "it is\n10\u00a0km\naway"},
		{"fits", "it is 10\u00a0km", 20, "it is 10\u00a0km"},
		{"long glued word is split by runes", "10\u00a0000\u00a0km", 4, "10\u00a00\n00\u00a0k\nm"},
		{"other unicode spaces too", "x\u2003y z", 3, "x\u2003y\nz"},
	})
}

func TestNonPositiveWidthReturnsInputUnchanged(t *testing.T) {
	inputs := []string{
		"",
		"plain",
		"  lead and trail  \n\n\n\tpara two\t\n",
		"Zürich ist schön  \n \n",
		"abcdefghijklmnopqrstuvwxyz",
	}
	for _, in := range inputs {
		for _, w := range []int{0, -1, -100} {
			if got := textfmt.Wrap(in, w); got != in {
				t.Errorf("Wrap(%q, %d) = %q, want input unchanged", in, w, got)
			}
		}
	}
}

// corpus mixes everything the contract talks about.
var corpus = []string{
	"Release 2.4 brings faster startup, a new  config format and   fewer\ndependencies.\n\n\n" +
		"Breaking:\tthe --legacy flag is gone.  Use --compat instead.\n \n" +
		"See https://example.com/releases/2.4/notes-and-migration-guide for the full list.\n",
	"Zürich, Genève und Lugano sind schön.\nΑθήνα και Θεσσαλονίκη.\n\t\n" +
		"Москва — достопримечательность на достопримечательности.\n\n" +
		"東京特許許可局 日本語の文章 です。 😀 🎉🎉🎉 ok\n",
	"\n\n   leading blank lines, then\ttabs\t\tand  spaces   \nand 10\u00a0km of road.\n\n\n\n",
	"a bb ccc dddd eeeee ffffff ggggggg hhhhhhhh iiiiiiiii jjjjjjjjjj kkkkkkkkkkk",
	"x",
}

// refWrap is an independent model of the contract, working on rune slices.
func refWrap(s string, width int) string {
	if width <= 0 {
		return s
	}
	var paras [][]string
	var cur []string
	for _, line := range strings.Split(s, "\n") {
		ws := strings.FieldsFunc(line, func(r rune) bool { return r == ' ' || r == '\t' })
		if len(ws) == 0 {
			if len(cur) > 0 {
				paras = append(paras, cur)
				cur = nil
			}
			continue
		}
		cur = append(cur, ws...)
	}
	if len(cur) > 0 {
		paras = append(paras, cur)
	}
	var out []string
	for i, p := range paras {
		if i > 0 {
			out = append(out, "")
		}
		var line []rune
		for _, w := range p {
			r := []rune(w)
			if len(r) > width {
				if len(line) > 0 {
					out = append(out, string(line))
				}
				for len(r) > width {
					out = append(out, string(r[:width]))
					r = r[width:]
				}
				line = append([]rune(nil), r...)
				continue
			}
			if len(line) > 0 && len(line)+1+len(r) > width {
				out = append(out, string(line))
				line = nil
			}
			if len(line) > 0 {
				line = append(line, ' ')
			}
			line = append(line, r...)
		}
		if len(line) > 0 {
			out = append(out, string(line))
		}
	}
	return strings.Join(out, "\n")
}

func TestMatchesContractAcrossWidths(t *testing.T) {
	for ci, text := range corpus {
		for width := 1; width <= 60; width++ {
			got := textfmt.Wrap(text, width)
			if want := refWrap(text, width); got != want {
				t.Errorf("corpus[%d], width %d:\n got  %q\n want %q", ci, width, got, want)
				break // one report per text is enough
			}
		}
	}
}

func TestOutputInvariants(t *testing.T) {
	stripWS := func(s string) string {
		return strings.Map(func(r rune) rune {
			if r == ' ' || r == '\t' || r == '\n' {
				return -1
			}
			return r
		}, s)
	}
	for ci, text := range corpus {
		for width := 1; width <= 60; width++ {
			if t.Failed() {
				return // the first violations are enough to go on
			}
			got := textfmt.Wrap(text, width)
			if !utf8.ValidString(got) {
				t.Errorf("corpus[%d], width %d: output is not valid UTF-8: %q", ci, width, got)
				continue
			}
			if strings.HasPrefix(got, "\n") || strings.HasSuffix(got, "\n") || strings.Contains(got, "\n\n\n") {
				t.Errorf("corpus[%d], width %d: bad blank lines in %q", ci, width, got)
			}
			if strings.Contains(got, "\t") || strings.Contains(got, "  ") {
				t.Errorf("corpus[%d], width %d: tab or double space in %q", ci, width, got)
			}
			for _, line := range strings.Split(got, "\n") {
				if n := utf8.RuneCountInString(line); n > width {
					t.Errorf("corpus[%d], width %d: line %q has %d runes", ci, width, line, n)
				}
				if strings.HasPrefix(line, " ") || strings.HasSuffix(line, " ") {
					t.Errorf("corpus[%d], width %d: line %q has leading or trailing space", ci, width, line)
				}
			}
			if stripWS(got) != stripWS(text) {
				t.Errorf("corpus[%d], width %d: text was lost or changed:\n got  %q", ci, width, got)
			}
			if again := textfmt.Wrap(got, width); again != got {
				t.Errorf("corpus[%d], width %d: wrapping the output again changed it:\n first  %q\n second %q", ci, width, got, again)
			}
		}
	}
}

func TestIndentUnchanged(t *testing.T) {
	tests := []struct{ in, prefix, want string }{
		{"a\n\nb", "> ", "> a\n\n> b"},
		{"a\nb\n", "  ", "  a\n  b\n"},
		{"\n\na", "# ", "\n\n# a"},
		{"a\n \nb", "> ", "> a\n>  \n> b"},
		{"", "> ", ""},
		{"x", "", "x"},
		{"ü\nß", "» ", "» ü\n» ß"},
	}
	for _, tt := range tests {
		if got := textfmt.Indent(tt.in, tt.prefix); got != tt.want {
			t.Errorf("Indent(%q, %q) = %q, want %q", tt.in, tt.prefix, got, tt.want)
		}
	}
}

func TestIndentOfWrappedParagraphs(t *testing.T) {
	got := textfmt.Indent(textfmt.Wrap("Fixed the  crash.\n\n\nThanks to Zoë\tfor the report.\n", 12), "> ")
	want := "> Fixed the\n> crash.\n\n> Thanks to\n> Zoë for the\n> report."
	if got != want {
		t.Errorf("got  %q\nwant %q", got, want)
	}
}
