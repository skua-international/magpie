package actions

import (
	"context"
	"os"

	"connectrpc.com/connect"
	registryv1 "github.com/skua-international/magpie/generated/go/registry/v1"

	"github.com/skua-international/magpie/cli/internal/client"
)

func ListModSources(ctx context.Context, c *client.Clients) ([]*registryv1.ModSourceInfo, error) {
	resp, err := c.ModSources.ListModSources(ctx, connect.NewRequest(&registryv1.ListModSourcesRequest{}))
	if err != nil {
		return nil, err
	}
	return resp.Msg.Sources, nil
}

func AddModSourceSteamURL(ctx context.Context, c *client.Clients, steamURL string) (string, error) {
	req := &registryv1.AddModSourceRequest{Source: &registryv1.AddModSourceRequest_SteamUrl{SteamUrl: steamURL}}
	resp, err := c.ModSources.AddModSource(ctx, connect.NewRequest(req))
	if err != nil {
		return "", err
	}
	return resp.Msg.Id, nil
}

func AddModSourceHTMLURL(ctx context.Context, c *client.Clients, presetURL string) (string, error) {
	req := &registryv1.AddModSourceRequest{Source: &registryv1.AddModSourceRequest_HtmlUrl{HtmlUrl: presetURL}}
	resp, err := c.ModSources.AddModSource(ctx, connect.NewRequest(req))
	if err != nil {
		return "", err
	}
	return resp.Msg.Id, nil
}

// AddModSourceLocalZip uploads a local mod from a zip file already on
// disk -- uniqueID is the caller-assigned, stable reference used as the
// mod's on-disk directory name (see LocalModUpload's own proto doc).
func AddModSourceLocalZip(ctx context.Context, c *client.Clients, uniqueID, zipPath string) (string, error) {
	data, err := os.ReadFile(zipPath)
	if err != nil {
		return "", err
	}
	req := &registryv1.AddModSourceRequest{
		Source: &registryv1.AddModSourceRequest_LocalMod{
			LocalMod: &registryv1.LocalModUpload{UniqueId: uniqueID, ZipContent: data},
		},
	}
	resp, err := c.ModSources.AddModSource(ctx, connect.NewRequest(req))
	if err != nil {
		return "", err
	}
	return resp.Msg.Id, nil
}

func DeleteModSource(ctx context.Context, c *client.Clients, id string) error {
	_, err := c.ModSources.DeleteModSource(ctx, connect.NewRequest(&registryv1.DeleteModSourceRequest{Id: id}))
	return err
}

// SyncModSource forces a Steam-backed source to re-resolve and starts a
// claim job covering the full current desired state -- doesn't wait for
// that job to finish (see the RPC's own proto doc).
func SyncModSource(ctx context.Context, c *client.Clients, id string) (string, error) {
	resp, err := c.ModSources.SyncModSource(ctx, connect.NewRequest(&registryv1.SyncModSourceRequest{Id: id}))
	if err != nil {
		return "", err
	}
	return resp.Msg.JobId, nil
}

func ListSyncedMods(ctx context.Context, c *client.Clients) ([]*registryv1.SyncedMod, error) {
	resp, err := c.ModSources.ListSyncedMods(ctx, connect.NewRequest(&registryv1.ListSyncedModsRequest{}))
	if err != nil {
		return nil, err
	}
	return resp.Msg.Mods, nil
}

func GetSyncedMod(ctx context.Context, c *client.Clients, modID uint64) (*registryv1.GetSyncedModResponse, error) {
	resp, err := c.ModSources.GetSyncedMod(ctx, connect.NewRequest(&registryv1.GetSyncedModRequest{ModId: modID}))
	if err != nil {
		return nil, err
	}
	return resp.Msg, nil
}

// InvalidateMod clears a mod's verification cache only -- never deletes
// its files (see the RPC's own proto doc, and mod-sources:invalidate's
// deliberately restricted scope).
func InvalidateMod(ctx context.Context, c *client.Clients, modID uint64) error {
	_, err := c.ModSources.InvalidateMod(ctx, connect.NewRequest(&registryv1.InvalidateModRequest{ModId: modID}))
	return err
}
