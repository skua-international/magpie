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
