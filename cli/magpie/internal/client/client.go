// Package client builds authenticated Connect clients against magpie's
// public API surface -- every RPC call this CLI/TUI makes goes through
// one of these.
//
// All of them share a single base URL. The cluster exposes one public
// host that routes to server-api, registry and identity by path prefix
// (charts/magpie/templates/ingress.yaml), so the RPC paths these
// generated clients already produce -- /controller.v1.ServerService/*
// and /registry.v1.*Service/* -- land on the right backend without this
// package having to know which service is which.
package client

import (
	"net/http"

	controllerv1connect "github.com/skua-international/magpie/generated/go/controller/v1/controllerv1connect"
	registryv1connect "github.com/skua-international/magpie/generated/go/registry/v1/registryv1connect"
)

// Config is where the cluster's public API lives -- defaults to this
// project's own Ingress hostname (see charts/magpie/values.yaml's
// ingress.host/baseDomain), overridable for anyone fronting the cluster
// differently.
type Config struct {
	APIURL string
}

func DefaultConfig() Config {
	return Config{APIURL: DefaultAPIURL}
}

// DefaultAPIURL matches the chart's default ingress.host/baseDomain.
// Shared with the cmd package so the flag default and this one can't
// drift apart.
const DefaultAPIURL = "http://api.magpie.local"

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
		Servers:    controllerv1connect.NewServerServiceClient(httpClient, cfg.APIURL),
		ModSources: registryv1connect.NewModSourceServiceClient(httpClient, cfg.APIURL),
		Missions:   registryv1connect.NewMissionServiceClient(httpClient, cfg.APIURL),
		Admin:      registryv1connect.NewAdminServiceClient(httpClient, cfg.APIURL),
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
