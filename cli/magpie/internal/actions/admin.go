package actions

import (
	"context"

	"connectrpc.com/connect"
	registryv1 "github.com/skua-international/magpie/generated/go/registry/v1"

	"github.com/skua-international/magpie/cli/internal/client"
)

func GetDiskUsage(ctx context.Context, c *client.Clients) (*registryv1.GetDiskUsageResponse, error) {
	resp, err := c.Admin.GetDiskUsage(ctx, connect.NewRequest(&registryv1.GetDiskUsageRequest{}))
	if err != nil {
		return nil, err
	}
	return resp.Msg, nil
}

// RefreshSteamAuth installs the cluster's Steam session from an
// already-negotiated refresh token -- see internal/steamlogin for how
// that's obtained (a client-side-only login; this RPC, like every other
// deployed service, never sees a password).
func RefreshSteamAuth(ctx context.Context, c *client.Clients, username, refreshToken string) error {
	_, err := c.Admin.RefreshSteamAuth(ctx, connect.NewRequest(&registryv1.RefreshSteamAuthRequest{
		Username:     username,
		RefreshToken: refreshToken,
	}))
	return err
}

// ExportState returns everything declarative about this cluster's Arma
// fleet -- mod source registrations, ConfigMaps, ArmaServer specs. See
// the RPC's own proto doc for exactly what's excluded (Postgres data,
// synced file content, ACL grants, live credentials) and why.
func ExportState(ctx context.Context, c *client.Clients) (*registryv1.ExportStateResponse, error) {
	resp, err := c.Admin.ExportState(ctx, connect.NewRequest(&registryv1.ExportStateRequest{}))
	if err != nil {
		return nil, err
	}
	return resp.Msg, nil
}

// ImportState re-creates whatever ExportState produced, scoped to
// exactly the sections state.go's Bundle carries (mod sources,
// ConfigMaps, servers) -- see ImportState's own proto doc for why this
// is per-item best-effort rather than transactional.
func ImportState(ctx context.Context, c *client.Clients, req *registryv1.ImportStateRequest) (*registryv1.ImportStateResponse, error) {
	resp, err := c.Admin.ImportState(ctx, connect.NewRequest(req))
	if err != nil {
		return nil, err
	}
	return resp.Msg, nil
}
