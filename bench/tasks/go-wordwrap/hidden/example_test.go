package textfmt_test

import (
	"fmt"

	"example.com/textfmt"
)

func ExampleWrap() {
	fmt.Println(textfmt.Wrap("The quick brown fox jumps over the lazy dog.", 16))
	// Output:
	// The quick brown
	// fox jumps over
	// the lazy dog.
}

func ExampleIndent() {
	fmt.Println(textfmt.Indent("first line\nsecond line", "> "))
	// Output:
	// > first line
	// > second line
}
