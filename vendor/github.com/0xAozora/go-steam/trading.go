package steam

import (
	"github.com/0xAozora/go-steam/protocol"
	"github.com/0xAozora/go-steam/protocol/protobuf"
	"github.com/0xAozora/go-steam/protocol/steamlang"
	"github.com/0xAozora/go-steam/steamid"
	"google.golang.org/protobuf/proto"
)

// Provides access to the Steam client's part of Steam Trading, that is bootstrapping
// the trade.
// The trade itself is not handled by the Steam client itself, but it's a part of
// the Steam website.
//
// You'll receive a TradeProposedEvent when a friend proposes a trade. You can accept it with
// the RespondRequest method. You can request a trade yourself with RequestTrade.
type Trading struct {
	Client *Client
}

type TradeRequestId uint32

func (t *Trading) HandlePacket(packet *protocol.Packet) {
	switch packet.EMsg {
	case steamlang.EMsg_EconTrading_InitiateTradeProposed:
		msg := new(protobuf.CMsgTrading_InitiateTradeRequest)
		packet.ReadProtoMsg(msg)
		t.Client.Emit(&TradeProposedEvent{
			RequestId: TradeRequestId(msg.GetTradeRequestId()),
			Other:     steamid.SteamId(msg.GetOtherSteamid()),
		})
	case steamlang.EMsg_EconTrading_InitiateTradeResult:
		msg := new(protobuf.CMsgTrading_InitiateTradeResponse)
		packet.ReadProtoMsg(msg)
		t.Client.Emit(&TradeResultEvent{
			RequestId: TradeRequestId(msg.GetTradeRequestId()),
			Response:  steamlang.EEconTradeResponse(msg.GetResponse()),
			Other:     steamid.SteamId(msg.GetOtherSteamid()),

			NumDaysSteamGuardRequired:            msg.GetSteamguardRequiredDays(),
			NumDaysNewDeviceCooldown:             msg.GetNewDeviceCooldownDays(),
			DefaultNumDaysPasswordResetProbation: msg.GetDefaultPasswordResetProbationDays(),
			NumDaysPasswordResetProbation:        msg.GetPasswordResetProbationDays(),
		})
	case steamlang.EMsg_EconTrading_StartSession:
		msg := new(protobuf.CMsgTrading_StartSession)
		packet.ReadProtoMsg(msg)
		t.Client.Emit(&TradeSessionStartEvent{
			Other: steamid.SteamId(msg.GetOtherSteamid()),
		})
	}
}

// Requests a trade. You'll receive a TradeResultEvent if the request fails or
// if the friend accepted the trade.
func (t *Trading) RequestTrade(other steamid.SteamId) error {
	return t.Client.Send(protocol.NewClientMsgProtobuf(steamlang.EMsg_EconTrading_InitiateTradeRequest, &protobuf.CMsgTrading_InitiateTradeRequest{
		OtherSteamid: proto.Uint64(uint64(other)),
	}))
}

// Responds to a TradeProposedEvent.
func (t *Trading) RespondRequest(requestId TradeRequestId, accept bool) error {
	var resp uint32
	if accept {
		resp = 0
	} else {
		resp = 1
	}

	return t.Client.Send(protocol.NewClientMsgProtobuf(steamlang.EMsg_EconTrading_InitiateTradeResponse, &protobuf.CMsgTrading_InitiateTradeResponse{
		TradeRequestId: proto.Uint32(uint32(requestId)),
		Response:       proto.Uint32(resp),
	}))
}

// This cancels a request made with RequestTrade.
func (t *Trading) CancelRequest(other steamid.SteamId) error {
	return t.Client.Send(protocol.NewClientMsgProtobuf(steamlang.EMsg_EconTrading_CancelTradeRequest, &protobuf.CMsgTrading_CancelTradeRequest{
		OtherSteamid: proto.Uint64(uint64(other)),
	}))
}
