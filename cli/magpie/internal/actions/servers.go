// Package actions is the shared layer between the TUI and the direct
// `magpie <resource> <verb>` subcommands -- every action here is called
// by both, so there's exactly one implementation of "what CreateServer
// actually does" rather than the TUI and CLI drifting apart over time.
package actions

import (
	"context"
	"fmt"
	"strings"

	"connectrpc.com/connect"
	controllerv1 "github.com/skua-international/magpie/generated/go/controller/v1"

	"github.com/skua-international/magpie/cli/internal/client"
)

type CreateServerParams struct {
	Name         string
	Port         uint32
	ModSourceIDs []string
	ConfigMap    string
	// A metrics endpoint this server's own game process/extension
	// exposes, if any -- purely a Prometheus scrape hint (see
	// ArmaServerSpec.metrics's own doc); MetricsPath defaults to
	// "/metrics" server-side when left empty. Ignored if MetricsPort is 0.
	MetricsPort uint32
	MetricsPath string
}

func ListServers(ctx context.Context, c *client.Clients) ([]*controllerv1.ServerInfo, error) {
	resp, err := c.Servers.ListServers(ctx, connect.NewRequest(&controllerv1.ListServersRequest{}))
	if err != nil {
		return nil, err
	}
	return resp.Msg.Servers, nil
}

func GetServer(ctx context.Context, c *client.Clients, id string) (*controllerv1.ServerInfo, error) {
	resp, err := c.Servers.GetServer(ctx, connect.NewRequest(&controllerv1.GetServerRequest{Id: id}))
	if err != nil {
		return nil, err
	}
	return resp.Msg, nil
}

// SanitizeServerName lowercases name (the common, harmless case -- "Ops"
// becomes "ops" with zero surprise) and then validates what's left
// against the exact same Kubernetes DNS-1123 label rule server-api's own
// validate_k8s_name enforces (services/server-api/src/service.rs) --
// `name` becomes the ArmaServer's own `metadata.name` verbatim, and this
// is the one place both the TUI wizard and the plain `servers create`
// command funnel through, so catching a bad name here means neither
// caller ever has to walk an entire wizard (or round-trip to the server)
// just to find out. Callers that also derive something from the name
// (e.g. a per-server config-override ConfigMap's default name) should
// call this first and use the sanitized result, not the raw input --
// see cmd/servers.go's serversCreateCmd and tui/create_server.go's
// createStepName for the two current examples.
func SanitizeServerName(name string) (string, error) {
	sanitized := strings.ToLower(name)
	if sanitized == "" {
		return sanitized, fmt.Errorf("name must not be empty")
	}
	if len(sanitized) > 63 {
		return sanitized, fmt.Errorf("name must be 63 characters or fewer")
	}
	isLabelChar := func(r rune) bool {
		return (r >= 'a' && r <= 'z') || (r >= '0' && r <= '9') || r == '-'
	}
	for _, r := range sanitized {
		if !isLabelChar(r) {
			return sanitized, fmt.Errorf("name must contain only lowercase letters, digits, and '-' (got %q)", name)
		}
	}
	isAlnum := func(r rune) bool { return (r >= 'a' && r <= 'z') || (r >= '0' && r <= '9') }
	first := rune(sanitized[0])
	last := rune(sanitized[len(sanitized)-1])
	if !isAlnum(first) || !isAlnum(last) {
		return sanitized, fmt.Errorf("name must start and end with a letter or digit")
	}
	return sanitized, nil
}

func CreateServer(ctx context.Context, c *client.Clients, p CreateServerParams) (*controllerv1.ServerInfo, error) {
	name, err := SanitizeServerName(p.Name)
	if err != nil {
		return nil, fmt.Errorf("invalid server name %q: %w", p.Name, err)
	}
	req := &controllerv1.CreateServerRequest{
		Name:         name,
		Port:         p.Port,
		ModSourceIds: p.ModSourceIDs,
	}
	if p.ConfigMap != "" {
		req.ConfigMap = &p.ConfigMap
	}
	if p.MetricsPort != 0 {
		req.MetricsPort = &p.MetricsPort
		if p.MetricsPath != "" {
			req.MetricsPath = &p.MetricsPath
		}
	}
	resp, err := c.Servers.CreateServer(ctx, connect.NewRequest(req))
	if err != nil {
		return nil, err
	}
	return resp.Msg, nil
}

func DeleteServer(ctx context.Context, c *client.Clients, id string) error {
	_, err := c.Servers.DeleteServer(ctx, connect.NewRequest(&controllerv1.DeleteServerRequest{Id: id}))
	return err
}

func StartServer(ctx context.Context, c *client.Clients, id string) (*controllerv1.ServerInfo, error) {
	resp, err := c.Servers.StartServer(ctx, connect.NewRequest(&controllerv1.StartServerRequest{Id: id}))
	if err != nil {
		return nil, err
	}
	return resp.Msg, nil
}

func StopServer(ctx context.Context, c *client.Clients, id string) (*controllerv1.ServerInfo, error) {
	resp, err := c.Servers.StopServer(ctx, connect.NewRequest(&controllerv1.StopServerRequest{Id: id}))
	if err != nil {
		return nil, err
	}
	return resp.Msg, nil
}

func UpdateServer(ctx context.Context, c *client.Clients, id string) (*controllerv1.ServerInfo, error) {
	resp, err := c.Servers.UpdateServer(ctx, connect.NewRequest(&controllerv1.UpdateServerRequest{Id: id}))
	if err != nil {
		return nil, err
	}
	return resp.Msg, nil
}
