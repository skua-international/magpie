package steam

import (
	"bytes"

	"github.com/0xAozora/go-steam/protocol"
	"github.com/0xAozora/go-steam/protocol/gamecoordinator"
	"github.com/0xAozora/go-steam/protocol/protobuf"
	"github.com/0xAozora/go-steam/protocol/steamlang"
	"google.golang.org/protobuf/proto"
)

type GameCoordinator struct {
	Client   *Client
	handlers []GCPacketHandler
}

func NewGC(client *Client) *GameCoordinator {
	return &GameCoordinator{
		Client:   client,
		handlers: make([]GCPacketHandler, 0),
	}
}

type GCPacketHandler interface {
	HandleGCPacket(*gamecoordinator.GCPacket)
}

func (g *GameCoordinator) RegisterPacketHandler(handler GCPacketHandler) {
	g.handlers = append(g.handlers, handler)
}

func (g *GameCoordinator) HandlePacket(packet *protocol.Packet) {
	if packet.EMsg != steamlang.EMsg_ClientFromGC {
		return
	}

	msg := new(protobuf.CMsgGCClient)
	packet.ReadProtoMsg(msg)

	p, err := gamecoordinator.NewGCPacket(msg)
	if err != nil {
		g.Client.Errorf("Error reading GC message: %v", err)
		return
	}

	for _, handler := range g.handlers {
		handler.HandleGCPacket(p)
	}
}

func (g *GameCoordinator) Write(msg gamecoordinator.IGCMsg) error {
	buf := new(bytes.Buffer)
	msg.Serialize(buf)

	msgType := msg.GetMsgType()
	if msg.IsProto() {
		msgType = msgType | 0x80000000 // mask with protoMask
	}

	return g.Client.Send(protocol.NewClientMsgProtobuf(steamlang.EMsg_ClientToGC, &protobuf.CMsgGCClient{
		Msgtype: proto.Uint32(msgType),
		Appid:   proto.Uint32(msg.GetAppId()),
		Payload: buf.Bytes(),
	}))
}

// Sets you in the given games. Specify none to quit all games.
func (g *GameCoordinator) SetGamesPlayed(appIds ...uint64) error {
	games := make([]*protobuf.CMsgClientGamesPlayed_GamePlayed, 0)
	for _, appId := range appIds {
		games = append(games, &protobuf.CMsgClientGamesPlayed_GamePlayed{
			GameId: proto.Uint64(appId),
		})
	}

	return g.Client.Send(protocol.NewClientMsgProtobuf(steamlang.EMsg_ClientGamesPlayed, &protobuf.CMsgClientGamesPlayed{
		GamesPlayed: games,
	}))
}
