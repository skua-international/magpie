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

// RefreshSteamAuthResult mirrors sync-daemon's own outcome shape (see its
// proto doc): either the login completed, or Steam Guard confirmation is
// still needed and the caller should retry with GuardCode set.
type RefreshSteamAuthResult struct {
	NeedsGuard bool
	GuardType  string
}

func RefreshSteamAuth(ctx context.Context, c *client.Clients, username, password, guardCode string) (RefreshSteamAuthResult, error) {
	req := &registryv1.RefreshSteamAuthRequest{Username: username, Password: password}
	if guardCode != "" {
		req.GuardCode = &guardCode
	}
	resp, err := c.Admin.RefreshSteamAuth(ctx, connect.NewRequest(req))
	if err != nil {
		return RefreshSteamAuthResult{}, err
	}
	return RefreshSteamAuthResult{NeedsGuard: resp.Msg.NeedsGuard, GuardType: resp.Msg.GuardType}, nil
}
