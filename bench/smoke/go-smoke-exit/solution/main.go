// Command greet greets someone by name.
package main

import (
	"fmt"
	"os"
)

func greeting(name string) string {
	return "hello, " + name
}

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: greet NAME")
		os.Exit(2)
	}
	fmt.Println(greeting(os.Args[1]))
}
