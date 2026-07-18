// Package auth implements the browser-loopback login flow against
// services/identity (see that service's handlers.rs doc for the
// server-side half: start?redirect_uri=... -> browser redirect with a
// one-time code -> POST /auth/exchange for the real tokens) and persists
// the resulting credentials to disk.
package auth

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"net"
	"net/http"
	"net/url"
	"os/exec"
	"runtime"
	"time"
)

// Credentials is the on-disk shape (see config.Path) -- ExpiresAt lets
// callers decide to refresh proactively rather than waiting for a 401.
type Credentials struct {
	AccessToken  string    `json:"access_token"`
	RefreshToken string    `json:"refresh_token"`
	ExpiresAt    time.Time `json:"expires_at"`
}

const loginTimeout = 5 * time.Minute

// Login opens the system browser to identity's OAuth (or Steam OpenID)
// start URL for provider, receives the resulting one-time code on a
// one-shot local HTTP listener, and exchanges it for real tokens. Blocks
// until the browser round trip completes, times out, or ctx is
// cancelled.
func Login(ctx context.Context, baseURL, provider string) (*Credentials, error) {
	return loginOrLink(ctx, baseURL, provider, "")
}

// LinkAccount attaches provider as an additional login method on the
// already-authenticated account behind accessToken -- identity's own
// start handler reads that bearer token to learn which existing user to
// link to (see its own doc), rather than creating a new one, exactly the
// same start/callback/exchange dance as Login otherwise. The returned
// Credentials are for the same user as before (just a freshly rotated
// pair), safe to save over the existing session unconditionally.
func LinkAccount(ctx context.Context, baseURL, provider, accessToken string) (*Credentials, error) {
	if accessToken == "" {
		return nil, fmt.Errorf("linking an account requires an existing session -- run `magpie login` first")
	}
	return loginOrLink(ctx, baseURL, provider, accessToken)
}

func loginOrLink(ctx context.Context, baseURL, provider, linkAccessToken string) (*Credentials, error) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return nil, fmt.Errorf("failed to open local callback listener: %w", err)
	}
	port := listener.Addr().(*net.TCPAddr).Port
	redirectURI := fmt.Sprintf("http://127.0.0.1:%d/callback", port)

	startURL := fmt.Sprintf("%s/auth/%s/start?redirect_uri=%s", baseURL, provider, url.QueryEscape(redirectURI))
	authorizeURL, err := fetchStartURL(ctx, startURL, linkAccessToken)
	if err != nil {
		listener.Close()
		return nil, err
	}

	if err := openBrowser(authorizeURL); err != nil {
		listener.Close()
		return nil, fmt.Errorf("failed to open browser (open %s manually): %w", authorizeURL, err)
	}

	code, err := awaitCallback(ctx, listener)
	if err != nil {
		return nil, err
	}

	return exchangeCode(ctx, baseURL, code)
}

// fetchStartURL calls identity's /auth/:provider/start, which returns
// {"url": "..."} rather than a redirect itself (see that handler's own
// doc) -- this process is the one that actually opens a browser to it.
// When accessToken is non-empty, it's sent as a Bearer header so
// identity's start handler links the new provider onto that existing
// user instead of creating a fresh one (see LinkAccount's doc).
func fetchStartURL(ctx context.Context, startURL, accessToken string) (string, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, startURL, nil)
	if err != nil {
		return "", err
	}
	if accessToken != "" {
		req.Header.Set("Authorization", "Bearer "+accessToken)
	}
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return "", fmt.Errorf("failed to reach identity service: %w", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("identity returned %s starting login", resp.Status)
	}
	var body struct {
		URL string `json:"url"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&body); err != nil {
		return "", fmt.Errorf("failed to parse identity's start response: %w", err)
	}
	return body.URL, nil
}

// awaitCallback serves exactly one request on listener (the browser
// redirect landing after a completed login) and returns its exchange
// code.
func awaitCallback(ctx context.Context, listener net.Listener) (string, error) {
	codeCh := make(chan string, 1)
	errCh := make(chan error, 1)

	mux := http.NewServeMux()
	mux.HandleFunc("/callback", func(w http.ResponseWriter, r *http.Request) {
		if errMsg := r.URL.Query().Get("error"); errMsg != "" {
			errCh <- fmt.Errorf("login failed: %s", errMsg)
			http.Error(w, "Login failed. You can close this tab.", http.StatusBadRequest)
			return
		}
		code := r.URL.Query().Get("code")
		if code == "" {
			errCh <- fmt.Errorf("callback landed with no code")
			http.Error(w, "Missing code. You can close this tab.", http.StatusBadRequest)
			return
		}
		codeCh <- code
		w.Header().Set("Content-Type", "text/plain; charset=utf-8")
		fmt.Fprint(w, "Login successful -- you can close this tab and return to the terminal.")
	})

	srv := &http.Server{Handler: mux}
	go func() { _ = srv.Serve(listener) }()
	defer func() {
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		_ = srv.Shutdown(shutdownCtx)
	}()

	select {
	case code := <-codeCh:
		return code, nil
	case err := <-errCh:
		return "", err
	case <-time.After(loginTimeout):
		return "", fmt.Errorf("timed out waiting for login (%s)", loginTimeout)
	case <-ctx.Done():
		return "", ctx.Err()
	}
}

func exchangeCode(ctx context.Context, baseURL, code string) (*Credentials, error) {
	body, err := json.Marshal(struct {
		Code string `json:"code"`
	}{Code: code})
	if err != nil {
		return nil, err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, baseURL+"/auth/exchange", bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, fmt.Errorf("failed to exchange login code: %w", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("identity returned %s exchanging login code", resp.Status)
	}

	var tokens struct {
		AccessToken  string `json:"access_token"`
		RefreshToken string `json:"refresh_token"`
		ExpiresIn    int64  `json:"expires_in"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&tokens); err != nil {
		return nil, fmt.Errorf("failed to parse exchange response: %w", err)
	}

	return &Credentials{
		AccessToken:  tokens.AccessToken,
		RefreshToken: tokens.RefreshToken,
		ExpiresAt:    time.Now().Add(time.Duration(tokens.ExpiresIn) * time.Second),
	}, nil
}

// Refresh trades a stored refresh token for a fresh token pair -- the
// same rotate-on-use flow identity's own tokens.rs implements: the
// returned RefreshToken always replaces whatever was stored, the old one
// is no longer valid.
func Refresh(ctx context.Context, baseURL, refreshToken string) (*Credentials, error) {
	body, err := json.Marshal(struct {
		RefreshToken string `json:"refresh_token"`
	}{RefreshToken: refreshToken})
	if err != nil {
		return nil, err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, baseURL+"/auth/refresh", bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, fmt.Errorf("failed to reach identity service: %w", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("refresh token is invalid or expired, log in again")
	}

	var tokens struct {
		AccessToken  string `json:"access_token"`
		RefreshToken string `json:"refresh_token"`
		ExpiresIn    int64  `json:"expires_in"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&tokens); err != nil {
		return nil, fmt.Errorf("failed to parse refresh response: %w", err)
	}

	return &Credentials{
		AccessToken:  tokens.AccessToken,
		RefreshToken: tokens.RefreshToken,
		ExpiresAt:    time.Now().Add(time.Duration(tokens.ExpiresIn) * time.Second),
	}, nil
}

func openBrowser(target string) error {
	switch runtime.GOOS {
	case "darwin":
		return exec.Command("open", target).Start()
	case "windows":
		return exec.Command("rundll32", "url.dll,FileProtocolHandler", target).Start()
	default:
		return exec.Command("xdg-open", target).Start()
	}
}
