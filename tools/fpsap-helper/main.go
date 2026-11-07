// fpsap-helper computes the 20-byte FairPlay SAP hash for fp-setup step 2.
// Separate GPL-3.0 component; see LICENSE.
package main

import (
	"encoding/hex"
	"fmt"
	"io"
	"os"
	"strings"

	"rottingapple/fpsap-helper/fpemu"
)

func main() {
	raw, err := io.ReadAll(os.Stdin)
	if err != nil {
		fatal(err)
	}
	hexIn := strings.TrimSpace(string(raw))
	if hexIn == "" {
		fatal(fmt.Errorf("stdin: expected hex-encoded m2 (142 bytes) or payload (128 bytes)"))
	}

	data, err := hex.DecodeString(hexIn)
	if err != nil {
		fatal(fmt.Errorf("decode hex: %w", err))
	}

	var payload [128]byte
	switch len(data) {
	case 142:
		copy(payload[:], data[14:142])
	case 128:
		copy(payload[:], data)
	default:
		fatal(fmt.Errorf("expected 142-byte m2 or 128-byte payload, got %d bytes", len(data)))
	}

	hash := fpemu.FPSAPExchangeStandalone(payload)
	if _, err := os.Stdout.Write([]byte(hex.EncodeToString(hash[:]))); err != nil {
		fatal(err)
	}
}

func fatal(err error) {
	fmt.Fprintf(os.Stderr, "fpsap-helper: %v\n", err)
	os.Exit(1)
}
