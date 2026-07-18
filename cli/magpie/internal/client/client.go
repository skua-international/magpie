// Package client builds authenticated Connect clients against magpie's
// public API surface (server-api + registry, both reachable through the
// cluster's Ingress) -- every RPC call this CLI/TUI makes goes through
// one of these.
package client

import (
	"net/http"

	controllerv1connect "github.com/skua-international/magpie/generated/go/controller/v1/controllerv1connect"
	registryv1connect "github.com/skua-international/magpie/generated/go/registry/v1/registryv1connect"
)

// Config is where each service actually lives -- defaults to this
// project's own Ingress hostnames (see charts/magpie/values.yaml's
// ingress.baseDomain), overridable for anyone fronting the cluster
// differently.
type Config struct {
	ServerAPIURL string
	RegistryURL  string
}

func DefaultConfig() Config {
	return Config{
		ServerAPIURL: "http://server-api.magpie.local",
		RegistryURL:  "http://registry.magpie.local",
	}
}

// Clients bundles every generated Connect client this tool talks to.
// sync-daemon's own SyncService is deliberately absent -- it's an
// internal, in-cluster-only API (never exposed through Ingress); every
// RPC on it that a caller outside the cluster could ever need is already
// proxied through registry's ModSourceService/AdminService.
type Clients struct {
	Servers    controllerv1connect.ServerServiceClient
	ModSources registryv1connect.ModSourceServiceClient
	Missions   registryv1connect.MissionServiceClient
	Admin      registryv1connect.AdminServiceClient
}

// New builds every client, all sharing one underlying http.Client whose
// RoundTripper injects accessToken as a bearer token on every request.
func New(cfg Config, accessToken string) *Clients {
	httpClient := &http.Client{Transport: &authTransport{token: accessToken, base: http.DefaultTransport}}
	return &Clients{
		Servers:    controllerv1connect.NewServerServiceClient(httpClient, cfg.ServerAPIURL),
		ModSources: registryv1connect.NewModSourceServiceClient(httpClient, cfg.RegistryURL),
		Missions:   registryv1connect.NewMissionServiceClient(httpClient, cfg.RegistryURL),
		Admin:      registryv1connect.NewAdminServiceClient(httpClient, cfg.RegistryURL),
	}
}

type authTransport struct {
	token string
	base  http.RoundTripper
}

func (t *authTransport) RoundTrip(req *http.Request) (*http.Response, error) {
	req = req.Clone(req.Context())
	req.Header.Set("Authorization", "Bearer "+t.token)
	return t.base.RoundTrip(req)
}
