package steamlogin

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"sync"
)

// Token stores the refresh token and associated guard data for one Steam account.
type Token struct {
	RefreshToken string `json:"refresh_token"`
	GuardData    string `json:"guard_data,omitempty"`
}

type tokenStore struct {
	dir    string
	mu     sync.Mutex
	tokens map[string]Token
}

func openTokenStore(dir string) (*tokenStore, error) {
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return nil, err
	}
	s := &tokenStore{dir: dir, tokens: map[string]Token{}}
	if err := readJSON(filepath.Join(dir, "tokens.json"), &s.tokens); err != nil {
		return nil, err
	}
	return s, nil
}

func (s *tokenStore) Token(username string) (Token, bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	t, ok := s.tokens[username]
	return t, ok
}

func (s *tokenStore) SaveToken(username string, token Token) error {
	s.mu.Lock()
	s.tokens[username] = token
	snapshot := make(map[string]Token, len(s.tokens))
	for k, v := range s.tokens {
		snapshot[k] = v
	}
	s.mu.Unlock()
	return writeJSON(filepath.Join(s.dir, "tokens.json"), snapshot)
}

func readJSON(path string, v any) error {
	b, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	if err != nil {
		return err
	}
	if len(b) == 0 {
		return nil
	}
	return json.Unmarshal(b, v)
}

func writeJSON(path string, v any) error {
	b, err := json.MarshalIndent(v, "", "  ")
	if err != nil {
		return err
	}
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, b, 0o600); err != nil {
		return err
	}
	return os.Rename(tmp, path)
}
