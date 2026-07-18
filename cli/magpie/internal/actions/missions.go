package actions

import (
	"context"
	"os"
	"path/filepath"

	"connectrpc.com/connect"
	registryv1 "github.com/skua-international/magpie/generated/go/registry/v1"

	"github.com/skua-international/magpie/cli/internal/client"
)

func ListMissions(ctx context.Context, c *client.Clients) ([]*registryv1.MissionInfo, error) {
	resp, err := c.Missions.ListMissions(ctx, connect.NewRequest(&registryv1.ListMissionsRequest{}))
	if err != nil {
		return nil, err
	}
	return resp.Msg.Missions, nil
}

func GetMission(ctx context.Context, c *client.Clients, id string) (*registryv1.MissionInfo, error) {
	resp, err := c.Missions.GetMission(ctx, connect.NewRequest(&registryv1.GetMissionRequest{Id: id}))
	if err != nil {
		return nil, err
	}
	return resp.Msg, nil
}

// UploadMission reads pboPath from disk and uploads it under its own
// filename. If overwriteID is non-empty, replaces that mission's content
// in place instead of creating a new one (see UploadMissionRequest's own
// proto doc).
func UploadMission(ctx context.Context, c *client.Clients, pboPath, overwriteID string) (*registryv1.MissionInfo, error) {
	data, err := os.ReadFile(pboPath)
	if err != nil {
		return nil, err
	}
	req := &registryv1.UploadMissionRequest{Name: filepath.Base(pboPath), PboContent: data}
	if overwriteID != "" {
		req.Id = &overwriteID
	}
	resp, err := c.Missions.UploadMission(ctx, connect.NewRequest(req))
	if err != nil {
		return nil, err
	}
	return resp.Msg, nil
}

func DeleteMission(ctx context.Context, c *client.Clients, id string) error {
	_, err := c.Missions.DeleteMission(ctx, connect.NewRequest(&registryv1.DeleteMissionRequest{Id: id}))
	return err
}
