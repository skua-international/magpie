// Package config persists the cluster target `magpiectl target` picks --
// identity/server-api/registry URLs plus namespace/release -- so it
// doesn't have to be re-passed as flags/env vars on every invocation.
// Kept separate from package auth (which persists login tokens) since
// the two have independent lifecycles: you can pick a target without
// being logged in to it yet, and logging in doesn't touch this file.
package config

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
)

// Target is the on-disk shape of a saved cluster target. Any empty field
// means "nothing saved for this" -- callers fall through to the next
// source in their own precedence chain (env var, then hardcoded
// default), never treat a zero Target as an error.
type Target struct {
	IdentityURL  string `json:"identity_url,omitempty"`
	ServerAPIURL string `json:"server_api_url,omitempty"`
	RegistryURL  string `json:"registry_url,omitempty"`
	Namespace    string `json:"namespace,omitempty"`
	Release      string `json:"release,omitempty"`
	// Context is the kubeconfig context name the user picked this target
	// from, kept only so `magpiectl target` can show what's active --
	// nothing here actually talks to that context (magpiectl only ever
	// calls kubectl for the per-server ConfigMap flow, and that still
	// uses whatever kubeconfig/KUBECONFIG is already in effect).
	Context string `json:"context,omitempty"`
}

// targetPath returns the same config dir auth's credentials.json uses
// (~/.config/magpiectl on Linux via os.UserConfigDir), just a different
// file in it -- one convention, not two.
func targetPath() (string, error) {
	dir, err := os.UserConfigDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(dir, "magpiectl", "target.json"), nil
}

// LoadTarget returns a zero Target, not an error, if none has been saved
// yet -- picking a target is optional and additive, never a required
// setup step.
func LoadTarget() (*Target, error) {
	path, err := targetPath()
	if err != nil {
		return nil, err
	}
	data, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return &Target{}, nil
	}
	if err != nil {
		return nil, err
	}
	var t Target
	if err := json.Unmarshal(data, &t); err != nil {
		return nil, err
	}
	return &t, nil
}

// SaveTarget writes t to disk, replacing whatever was saved before.
func SaveTarget(t *Target) error {
	path, err := targetPath()
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return err
	}
	data, err := json.MarshalIndent(t, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, data, 0o600)
}
