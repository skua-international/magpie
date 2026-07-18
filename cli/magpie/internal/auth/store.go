package auth

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
)

// ErrNotLoggedIn is returned by Load when no credentials file exists yet.
var ErrNotLoggedIn = errors.New("not logged in -- run `magpie login`")

// configPath returns ~/.config/magpie/credentials.json (or the
// platform-appropriate equivalent via os.UserConfigDir), matching every
// other CLI tool's convention rather than inventing a new one.
func configPath() (string, error) {
	dir, err := os.UserConfigDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(dir, "magpie", "credentials.json"), nil
}

// Save writes creds to disk, mode 0600 -- these are bearer tokens, not
// meant to be group/world readable.
func Save(creds *Credentials) error {
	path, err := configPath()
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return err
	}
	data, err := json.MarshalIndent(creds, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, data, 0o600)
}

// Load reads previously saved credentials, or ErrNotLoggedIn if none
// exist yet.
func Load() (*Credentials, error) {
	path, err := configPath()
	if err != nil {
		return nil, err
	}
	data, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return nil, ErrNotLoggedIn
	}
	if err != nil {
		return nil, err
	}
	var creds Credentials
	if err := json.Unmarshal(data, &creds); err != nil {
		return nil, err
	}
	return &creds, nil
}
