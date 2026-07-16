package cmd

import (
	"os"
	"path/filepath"
	"testing"
)

func TestResolveSecretInput(t *testing.T) {
	literal, err := resolveSecretInput("sk-literal")
	if err != nil {
		t.Fatalf("resolve literal: %v", err)
	}
	if literal != "sk-literal" {
		t.Fatalf("unexpected literal value: %q", literal)
	}

	path := filepath.Join(t.TempDir(), "api_key.txt")
	if err := os.WriteFile(path, []byte("  sk-from-file\n"), 0o600); err != nil {
		t.Fatalf("write secret file: %v", err)
	}
	fromFile, err := resolveSecretInput("@" + path)
	if err != nil {
		t.Fatalf("resolve @file: %v", err)
	}
	if fromFile != "sk-from-file" {
		t.Fatalf("unexpected file value: %q", fromFile)
	}

	if _, err := resolveSecretInput("@"); err == nil {
		t.Fatalf("expected error for empty path after '@'")
	}
	if _, err := resolveSecretInput("@" + filepath.Join(t.TempDir(), "missing.txt")); err == nil {
		t.Fatalf("expected error for missing secret file")
	}

	emptyPath := filepath.Join(t.TempDir(), "empty.txt")
	if err := os.WriteFile(emptyPath, []byte(" \n"), 0o600); err != nil {
		t.Fatalf("write empty file: %v", err)
	}
	if _, err := resolveSecretInput("@" + emptyPath); err == nil {
		t.Fatalf("expected error for empty secret file")
	}
}

func TestParseACLDefaults(t *testing.T) {
	defaults, err := parseACLDefaults([]string{"internal=staff", " wiki = public "})
	if err != nil {
		t.Fatalf("parse pairs: %v", err)
	}
	if len(defaults) != 2 || defaults["internal"] != "staff" || defaults["wiki"] != "public" {
		t.Fatalf("unexpected defaults: %+v", defaults)
	}

	// An empty list yields an empty (non-nil) map: sent as {} it clears all
	// namespace defaults on the server.
	cleared, err := parseACLDefaults(nil)
	if err != nil {
		t.Fatalf("parse empty pairs: %v", err)
	}
	if cleared == nil || len(cleared) != 0 {
		t.Fatalf("expected empty map, got %+v", cleared)
	}

	for _, bad := range []string{"no-separator", "=label", "ns=", " = "} {
		if _, err := parseACLDefaults([]string{bad}); err == nil {
			t.Fatalf("expected error for entry %q", bad)
		}
	}
}
