// Package steamlogin runs the interactive Steam login flow directly in
// magpiectl. It handles QR login, username/password login, Steam Guard
// challenges, and refresh-token reuse without shelling out to the old
// helper binary.
package steamlogin

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"time"

	steam "github.com/0xAozora/go-steam"
	"github.com/0xAozora/go-steam/protocol"
	"github.com/0xAozora/go-steam/protocol/protobuf"
	"github.com/0xAozora/go-steam/protocol/steamlang"
	"github.com/mdp/qrterminal/v3"
	"google.golang.org/protobuf/proto"
)

// Result is the negotiated Steam identity and refresh token.
type Result struct {
	SteamUser    string
	RefreshToken string
}

// Options mirrors the interactive auth modes supported by gosteam.
type Options struct {
	Username  string
	Password  string
	Anonymous bool
	UseQR     bool

	Guard func(kind GuardKind) (string, error)

	// OnChallengeURL receives the QR challenge URL. If nil, the URL is
	// rendered as a terminal QR code.
	OnChallengeURL func(url string)
}

// GuardKind identifies which Steam Guard code is required.
type GuardKind int

const (
	GuardUnknown GuardKind = iota
	GuardEmailCode
	GuardDeviceCode
)

func (k GuardKind) String() string {
	switch k {
	case GuardEmailCode:
		return "email code"
	case GuardDeviceCode:
		return "mobile authenticator code"
	default:
		return "guard code"
	}
}

// Negotiate runs the default QR-based login flow.
func Negotiate(ctx context.Context) (*Result, error) {
	return Login(ctx, Options{UseQR: true})
}

// Login connects to Steam and runs the requested interactive auth flow.
func Login(ctx context.Context, opts Options) (*Result, error) {
	store, err := openStore()
	if err != nil {
		return nil, err
	}

	sc := steam.NewClient()
	if _, err := sc.Connect(); err != nil {
		return nil, fmt.Errorf("connect: %w", err)
	}

	var refreshToken, guardData string
	if opts.Username != "" {
		if token, ok := store.Token(opts.Username); ok {
			refreshToken = token.RefreshToken
			guardData = token.GuardData
		}
	}

	for {
		select {
		case <-ctx.Done():
			sc.Disconnect()
			return nil, ctx.Err()

		case ev := <-sc.Events():
			switch e := ev.(type) {
			case *steam.ConnectedEvent:
				if err := startLogin(sc, opts, refreshToken, guardData); err != nil {
					sc.Disconnect()
					return nil, err
				}

			case *steam.LoggedOnEvent:
				if e.Result != steamlang.EResult_OK {
					sc.Disconnect()
					return nil, fmt.Errorf("logon failed: %v", e.Result)
				}
				return finishLogin(sc, store, opts)

			case *steam.LogOnFailedEvent:
				sc.Disconnect()
				return nil, fmt.Errorf("logon failed: %v", e.Result)

			case steam.FatalErrorEvent:
				return nil, fmt.Errorf("steam connection error: %w", error(e))
			}
		}
	}
}

func openStore() (*tokenStore, error) {
	dir, err := os.UserConfigDir()
	if err != nil {
		return nil, err
	}
	return openTokenStore(filepath.Join(dir, "magpiectl", "steam"))
}

func startLogin(sc *steam.Client, opts Options, refreshToken, guardData string) error {
	switch {
	case opts.Anonymous:
		details := &steam.LogOnDetails{Anonymous: true}
		sc.Auth.Details = details
		return sc.Auth.LogOn(details)

	case opts.UseQR:
		challenge := opts.OnChallengeURL
		if challenge == nil {
			challenge = printChallengeURL
		}
		return logOnQR(sc, &steam.LogOnDetails{}, challenge)

	case refreshToken != "":
		details := &steam.LogOnDetails{
			Username:               opts.Username,
			RefreshToken:           refreshToken,
			GuardData:              guardData,
			ShouldRememberPassword: true,
		}
		sc.Auth.Details = details
		return sc.Auth.LogOn(details)

	default:
		if opts.Username == "" || opts.Password == "" {
			return errors.New("username and password required (or use Anonymous/UseQR/a stored token)")
		}
		if opts.Guard != nil {
			sc.Auth.Authenticator = guardAuth{fn: opts.Guard}
		}
		details := &steam.LogOnDetails{
			Username:               opts.Username,
			Password:               opts.Password,
			ShouldRememberPassword: true,
		}
		return sc.Auth.LogOnCredentials(details)
	}
}

func finishLogin(sc *steam.Client, store *tokenStore, opts Options) (*Result, error) {
	defer sc.Disconnect()

	details := sc.Auth.Details
	if details == nil {
		return nil, errors.New("steam login succeeded without auth details")
	}

	user := details.Username
	if user == "" {
		user = opts.Username
	}
	if store != nil && user != "" && details.RefreshToken != "" {
		_ = store.SaveToken(user, Token{RefreshToken: details.RefreshToken, GuardData: details.GuardData})
	}
	return &Result{SteamUser: user, RefreshToken: details.RefreshToken}, nil
}

func printChallengeURL(url string) {
	fmt.Fprintln(os.Stderr, "Scan this QR code with the Steam mobile app:")
	qrterminal.GenerateHalfBlock(url, qrterminal.L, os.Stderr)
}

type guardAuth struct {
	fn func(GuardKind) (string, error)
}

func (g guardAuth) GetCode(kind protobuf.EAuthSessionGuardType, update func(string, protobuf.EAuthSessionGuardType) error) error {
	code, err := g.fn(toGuardKind(kind))
	if err != nil {
		return err
	}
	return update(code, kind)
}

func toGuardKind(kind protobuf.EAuthSessionGuardType) GuardKind {
	switch kind {
	case protobuf.EAuthSessionGuardType_k_EAuthSessionGuardType_EmailCode:
		return GuardEmailCode
	case protobuf.EAuthSessionGuardType_k_EAuthSessionGuardType_DeviceCode:
		return GuardDeviceCode
	default:
		return GuardUnknown
	}
}

type qrLogin struct {
	client      *steam.Client
	details     *steam.LogOnDetails
	onChallenge func(string)

	clientID  uint64
	requestID []byte
	interval  float32
	pollStop  chan struct{}
}

func logOnQR(sc *steam.Client, details *steam.LogOnDetails, onChallenge func(string)) error {
	q := &qrLogin{client: sc, details: details, onChallenge: onChallenge}
	q.details.Username = ""
	sc.Auth.Details = details

	hello := &protobuf.CMsgClientHello{ProtocolVersion: proto.Uint32(steamlang.MsgClientLogon_CurrentProtocol)}
	if err := sc.Send(protocol.NewClientMsgProtobuf(steamlang.EMsg_ClientHello, hello)); err != nil {
		return err
	}

	return q.beginQRSession()
}

func (q *qrLogin) beginQRSession() error {
	platformType := protobuf.EAuthTokenPlatformType_k_EAuthTokenPlatformType_SteamClient.Enum()
	deviceFriendlyName := "DESKTOP-HELLO"
	req := &protobuf.CAuthentication_BeginAuthSessionViaQR_Request{
		DeviceFriendlyName: &deviceFriendlyName,
		PlatformType:       platformType,
		WebsiteId:          proto.String("Client"),
		DeviceDetails: &protobuf.CAuthentication_DeviceDetails{
			DeviceFriendlyName: &deviceFriendlyName,
			PlatformType:       platformType,
			OsType:             proto.Int32(16),
		},
	}
	msg := protocol.NewClientMsgProtobuf(steamlang.EMsg_ServiceMethodCallFromClientNonAuthed, req)
	jobname := "Authentication.BeginAuthSessionViaQR#1"
	msg.Header.Proto.TargetJobName = &jobname
	jobID := q.client.GetNextJobId()
	msg.SetSourceJobId(jobID)

	q.client.JobMutex.Lock()
	q.client.JobHandlers[uint64(jobID)] = q.handleQRSession
	q.client.JobMutex.Unlock()

	return q.client.Send(msg)
}

func (q *qrLogin) handleQRSession(packet *protocol.Packet) error {
	body := new(protobuf.CAuthentication_BeginAuthSessionViaQR_Response)
	_ = packet.ReadProtoMsg(body)

	q.clientID = body.GetClientId()
	q.requestID = body.RequestId
	q.interval = body.GetInterval()

	if q.onChallenge != nil && body.ChallengeUrl != nil {
		q.onChallenge(*body.ChallengeUrl)
	}

	go q.qrPollLoop()
	return nil
}

func (q *qrLogin) qrPollLoop() {
	interval := time.Duration(q.interval * float32(time.Second))
	if interval <= 0 {
		interval = 2 * time.Second
	}
	q.pollStop = make(chan struct{})
	for {
		select {
		case <-q.pollStop:
			return
		case <-time.After(interval):
			if err := q.sendQRPoll(); err != nil {
				q.client.Fatalf("qr poll: %v", err)
				return
			}
		}
	}
}

func (q *qrLogin) sendQRPoll() error {
	req := &protobuf.CAuthentication_PollAuthSessionStatus_Request{
		ClientId:  proto.Uint64(q.clientID),
		RequestId: q.requestID,
	}
	msg := protocol.NewClientMsgProtobuf(steamlang.EMsg_ServiceMethodCallFromClientNonAuthed, req)
	jobname := "Authentication.PollAuthSessionStatus#1"
	msg.Header.Proto.TargetJobName = &jobname
	jobID := q.client.GetNextJobId()
	msg.SetSourceJobId(jobID)

	q.client.JobMutex.Lock()
	q.client.JobHandlers[uint64(jobID)] = q.handleQRPoll
	q.client.JobMutex.Unlock()

	return q.client.Send(msg)
}

func (q *qrLogin) handleQRPoll(packet *protocol.Packet) error {
	body := new(protobuf.CAuthentication_PollAuthSessionStatus_Response)
	_ = packet.ReadProtoMsg(body)

	if body.NewClientId != nil {
		q.clientID = *body.NewClientId
	}
	if body.NewChallengeUrl != nil && q.onChallenge != nil {
		q.onChallenge(*body.NewChallengeUrl)
	}
	if body.RefreshToken == nil {
		return nil
	}

	if q.pollStop != nil {
		close(q.pollStop)
	}

	q.details.AccessToken = body.GetAccessToken()
	q.details.RefreshToken = *body.RefreshToken
	if body.NewGuardData != nil {
		q.details.GuardData = *body.NewGuardData
	}
	if body.AccountName != nil {
		q.details.Username = *body.AccountName
	}

	return q.client.Auth.LogOn(q.details)
}
