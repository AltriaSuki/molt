// Command greet greets someone by name.
package main

import (
	"fmt"
	"os"
	"strings"
)

func greeting(name string) string {
	return "hello, " + name
}

func main() {
	fmt.Println(greeting(strings.Join(os.Args[1:], " ")))
}
