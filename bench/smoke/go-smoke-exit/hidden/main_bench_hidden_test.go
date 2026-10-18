package main

import (
	"bytes"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
)

// build compiles the command once per test binary.
func build(t *testing.T) string {
	t.Helper()
	exe := filepath.Join(t.TempDir(), "greet")
	cmd := exec.Command("go", "build", "-o", exe, ".")
	cmd.Env = append(os.Environ(), "GOTOOLCHAIN=local")
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("go build: %v\n%s", err, out)
	}
	return exe
}

func run(t *testing.T, exe string, args ...string) (string, string, int) {
	t.Helper()
	var stdout, stderr bytes.Buffer
	cmd := exec.Command(exe, args...)
	cmd.Stdout, cmd.Stderr = &stdout, &stderr
	err := cmd.Run()
	var exit *exec.ExitError
	switch {
	case err == nil:
		return stdout.String(), stderr.String(), 0
	case errors.As(err, &exit):
		return stdout.String(), stderr.String(), exit.ExitCode()
	default:
		t.Fatalf("running greet: %v", err)
		return "", "", -1
	}
}

func TestBenchHiddenUsage(t *testing.T) {
	exe := build(t)
	for _, args := range [][]string{nil, {"Ada", "Grace"}} {
		out, errOut, code := run(t, exe, args...)
		if code != 2 || out != "" || errOut != "usage: greet NAME\n" {
			t.Errorf("greet %q: status %d, stdout %q, stderr %q", args, code, out, errOut)
		}
	}
}

func TestBenchHiddenGreets(t *testing.T) {
	exe := build(t)
	out, errOut, code := run(t, exe, "Ada")
	if code != 0 || out != "hello, Ada\n" || errOut != "" {
		t.Errorf("greet Ada: status %d, stdout %q, stderr %q", code, out, errOut)
	}
}
