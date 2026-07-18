// Package actions is the shared layer between the TUI and the direct
// `magpie <resource> <verb>` subcommands -- every action here is called
// by both, so there's exactly one implementation of "what CreateServer
// actually does" rather than the TUI and CLI drifting apart over time.
package actions

import (
	"context"

	"connectrpc.com/connect"
	controllerv1 "github.com/skua-international/magpie/generated/go/controller/v1"

	"github.com/skua-international/magpie/cli/internal/client"
)

type CreateServerParams struct {
	Name          string
	Port          uint32
	ModSourceIDs  []string
	ArmaConfig    string
	NetworkConfig string
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

func CreateServer(ctx context.Context, c *client.Clients, p CreateServerParams) (*controllerv1.ServerInfo, error) {
	req := &controllerv1.CreateServerRequest{
		Name:         p.Name,
		Port:         p.Port,
		ModSourceIds: p.ModSourceIDs,
	}
	if p.ArmaConfig != "" {
		req.ArmaConfig = &p.ArmaConfig
	}
	if p.NetworkConfig != "" {
		req.NetworkConfig = &p.NetworkConfig
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
