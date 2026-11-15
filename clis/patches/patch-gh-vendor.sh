#!/usr/bin/env bash
# Patches gh CLI's vendored dependencies for wasip3/wasm32 compilation.
# Run from the gh-cli directory after `go mod vendor`.
set -euo pipefail

V=vendor

echo "=== Adding wasip3 stub files ==="

# 1. mattn/go-isatty
cat > "$V/github.com/mattn/go-isatty/isatty_wasip3.go" << 'GO'
//go:build wasip3

package isatty

func IsTerminal(fd uintptr) bool    { return false }
func IsCygwinTerminal(fd uintptr) bool { return false }
GO

# 2. atotto/clipboard
cat > "$V/github.com/atotto/clipboard/clipboard_wasip3.go" << 'GO'
//go:build wasip3

package clipboard

import "errors"

var errNotSupported = errors.New("clipboard not supported on wasip3")

func readAll() (string, error) { return "", errNotSupported }
func writeAll(text string) error { return errNotSupported }
GO

# 3. muesli/termenv
cat > "$V/github.com/muesli/termenv/termenv_wasip3.go" << 'GO'
//go:build wasip3

package termenv

import "io"

func (o Output) ColorProfile() Profile    { return ANSI256 }
func (o Output) foregroundColor() Color   { return ANSIColor(7) }
func (o Output) backgroundColor() Color   { return ANSIColor(0) }

func EnableVirtualTerminalProcessing(w io.Writer) (func() error, error) {
	return func() error { return nil }, nil
}
GO

# 4. gdamore/tcell/v2
cat > "$V/github.com/gdamore/tcell/v2/tscreen_wasip3.go" << 'GO'
//go:build wasip3

package tcell

import "errors"

func (t *tScreen) initialize() error {
	return errors.New("terminal not supported on wasip3")
}
GO

# 5. google/certificate-transparency-go/x509
cat > "$V/github.com/google/certificate-transparency-go/x509/root_wasip3.go" << 'GO'
//go:build wasip3

package x509

var certFiles = []string{}

func loadSystemRoots() (*CertPool, error)                                   { return NewCertPool(), nil }
func (c *Certificate) systemVerify(opts *VerifyOptions) ([][]*Certificate, error) { return nil, nil }
GO

# 6. charmbracelet/bubbletea
cat > "$V/github.com/charmbracelet/bubbletea/tty_wasip3.go" << 'GO'
//go:build wasip3

package tea

import (
	"errors"
	"os"
)

const suspendSupported = false

func (p *Program) initInput() (err error)    { return nil }
func openInputTTY() (*os.File, error) { return nil, errors.New("TTY not supported on wasip3") }
func suspendProcess()                 {}
GO

cat > "$V/github.com/charmbracelet/bubbletea/signals_wasip3.go" << 'GO'
//go:build wasip3

package tea

func (p *Program) listenForResize(done chan struct{}) { <-done }
GO

# 7. in-toto/in-toto-golang
cat > "$V/github.com/in-toto/in-toto-golang/in_toto/util_wasip3.go" << 'GO'
//go:build wasip3

package in_toto

func isWritable(path string) error { return nil }
GO

# 8. AlecAivazis/survey/v2/terminal
cat > "$V/github.com/AlecAivazis/survey/v2/terminal/runereader_wasip3.go" << 'GO'
//go:build wasip3

package terminal

import (
	"bytes"
	"io"
)

type runeReaderState struct{}

func newRuneReaderState(input FileReader) runeReaderState { return runeReaderState{} }

func (rr *RuneReader) Buffer() *bytes.Buffer { return &bytes.Buffer{} }

func (rr *RuneReader) ReadRune() (rune, int, error) {
	var buf [1]byte
	n, err := rr.stdio.In.Read(buf[:])
	if err != nil { return 0, 0, err }
	if n == 0 { return 0, 0, io.EOF }
	return rune(buf[0]), 1, nil
}

func (rr *RuneReader) SetTermMode() error     { return nil }
func (rr *RuneReader) RestoreTermMode() error  { return nil }
GO

# 9. sirupsen/logrus
cat > "$V/github.com/sirupsen/logrus/terminal_check_wasip3.go" << 'GO'
//go:build wasip3

package logrus

import "io"

func isTerminal(fd int) bool            { return false }
func checkIfTerminal(w io.Writer) bool  { return false }
GO

echo "=== Patching overly-broad build constraints ==="

# survey/v2/terminal/runereader_posix.go: !windows -> !windows && !wasip3
sed -i '1s|//go:build !windows|//go:build !windows \&\& !wasip3|' \
    "$V/github.com/AlecAivazis/survey/v2/terminal/runereader_posix.go"
sed -i 's|// +build !windows$|// +build !windows,!wasip3|' \
    "$V/github.com/AlecAivazis/survey/v2/terminal/runereader_posix.go"

# logrus/terminal_check_notappengine.go: add !wasip3
sed -i 's|!appengine,!js,!windows,!nacl,!plan9|!appengine,!js,!windows,!nacl,!plan9,!wasip3|' \
    "$V/github.com/sirupsen/logrus/terminal_check_notappengine.go"

# in-toto util_unix.go: (linux || darwin || !windows) && !wasip3
# The go:build line has pipes so we use perl instead of sed
perl -pi -e 's{^//go:build linux \|\| darwin \|\| !windows$}{//go:build (linux || darwin || !windows) \&\& !wasip3}' \
    "$V/github.com/in-toto/in-toto-golang/in_toto/util_unix.go"
sed -i 's|// +build linux darwin !windows|// +build !wasip3|' \
    "$V/github.com/in-toto/in-toto-golang/in_toto/util_unix.go"

echo "=== Done. Patched $(find $V -name '*wasip3*' | wc -l) stub files ==="
