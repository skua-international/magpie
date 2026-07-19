package steam

import (
	"crypto/rand"
	"crypto/rsa"
	"crypto/sha1"
	"encoding/base64"
	"errors"
	"fmt"
	"math/big"
	"strconv"
	"sync/atomic"
	"time"

	"github.com/0xAozora/go-steam/protocol"
	"github.com/0xAozora/go-steam/protocol/protobuf"
	"github.com/0xAozora/go-steam/protocol/steamlang"
	"github.com/0xAozora/go-steam/steamid"
	"google.golang.org/protobuf/proto"
)

type Auth struct {
	Client  *Client
	Details *LogOnDetails

	authSession   *protobuf.CAuthentication_BeginAuthSessionViaCredentials_Response
	Authenticator Authenticator
}

// Custom Authenticator
type Authenticator interface {
	GetCode(protobuf.EAuthSessionGuardType, func(string, protobuf.EAuthSessionGuardType) error) error
}

type SentryHash []byte

type LogOnDetails struct {
	Username string

	// If logging into an account without a login key, the account's password.
	Password string

	// If you have a Steam Guard email code, you can provide it here.
	AuthCode string

	// If you have a Steam Guard mobile two-factor authentication code, you can provide it here.
	TwoFactorCode  string
	SentryFileHash SentryHash
	LoginKey       string

	// true if you want to get a login key which can be used in lieu of
	// a password for subsequent logins. false or omitted otherwise.
	ShouldRememberPassword bool

	AccessToken  string
	RefreshToken string
	GuardData    string

	Anonymous bool
}

// Log on with the given details. You must always specify username and
// password OR username and loginkey. For the first login, don't set an authcode or a hash and you'll
//
//	receive an error (EResult_AccountLogonDenied)
//
// and Steam will send you an authcode. Then you have to login again, this time with the authcode.
// Shortly after logging in, you'll receive a MachineAuthUpdateEvent with a hash which allows
// you to login without using an authcode in the future.
//
// If you don't use Steam Guard, username and password are enough.
//
// After the event EMsg_ClientNewLoginKey is received you can use the LoginKey
// to login instead of using the password.
func (a *Auth) LogOn(details *LogOnDetails) error {

	anonymous := details.Anonymous

	if details.Username == "" && !anonymous {
		panic("Username must be set!")
	}
	if details.Password == "" && details.LoginKey == "" && details.RefreshToken == "" && !anonymous {
		panic("Password, LoginKey or RefreshToken must be set!")
	}

	logon := new(protobuf.CMsgClientLogon)

	if details.RefreshToken != "" {
		logon.AccessToken = &details.RefreshToken // Yes, RefreshToken as AccessToken
	} else if !anonymous {
		logon.Password = &details.Password
		if details.AuthCode != "" {
			logon.AuthCode = proto.String(details.AuthCode)
		}
		if details.TwoFactorCode != "" {
			logon.TwoFactorCode = proto.String(details.TwoFactorCode)
		}

		if details.LoginKey != "" {
			logon.LoginKey = proto.String(details.LoginKey)
		}
	}

	if details.SentryFileHash == nil {
		logon.EresultSentryfile = proto.Int32(9) // NotFound
	} else {
		logon.EresultSentryfile = proto.Int32(1) // OK
	}
	logon.ShaSentryfile = details.SentryFileHash

	if details.ShouldRememberPassword {
		logon.ShouldRememberPassword = proto.Bool(details.ShouldRememberPassword)
	}

	if anonymous {
		logon.AnonUserTargetAccountName = proto.String("anonymous")

		atomic.StoreUint64(&a.Client.steamId, uint64(steamid.NewIdAdv(0, 0, int32(steamlang.EUniverse_Public), int32(steamlang.EAccountType_AnonUser))))
	} else {
		logon.AccountName = &details.Username
		logon.ClientLanguage = proto.String("english")
		logon.MachineName = proto.String("DESKTOP-HELLO")
		logon.MachineId = []byte{
			0, 77, 101, 115, 115, 97, 103, 101, 79, 98, 106, 101, 99, 116, 0, 1, 66, 66, 51, 0, 52, 48, 56, 54, 49, 48, 48, 97, 54, 97, 50, 55, 102, 100, 100, 51, 49, 48, 98, 52, 50, 99, 56, 97, 102, 100, 54, 48, 51, 51, 97, 56, 51, 98, 53, 53, 49, 97, 48, 97, 0, 1, 70, 70, 50, 0, 100, 54, 49, 99, 100, 98, 97, 52, 49, 49, 55, 57, 57, 54, 97, 52, 57, 52, 52, 49, 98, 101, 49, 49, 99, 51, 98, 100, 98, 52, 101, 48, 99, 53, 51, 54, 54, 101, 99, 51, 0, 1, 51, 66, 51, 0, 99, 98, 97, 102, 56, 102, 52, 55, 56, 56, 50, 99, 102, 102, 101, 101, 52, 51, 101, 101, 49, 99, 97, 97, 99, 101, 98, 56, 97, 56, 101, 102, 99, 98, 53, 55, 54, 53, 53, 50, 0, 8, 8,
		}

		atomic.StoreUint64(&a.Client.steamId, uint64(steamid.NewIdAdv(0, 1, int32(steamlang.EUniverse_Public), int32(steamlang.EAccountType_Individual))))
	}

	logon.ClientOsType = proto.Uint32(16) // Windows 10
	logon.ProtocolVersion = proto.Uint32(steamlang.MsgClientLogon_CurrentProtocol)
	logon.ObfuscatedPrivateIp = &protobuf.CMsgIPAddress{Ip: &protobuf.CMsgIPAddress_V4{V4: 2047164953}}
	logon.DeprecatedObfustucatedPrivateIp = proto.Uint32(logon.ObfuscatedPrivateIp.GetV4())

	// Other
	logon.CellId = proto.Uint32(5)
	logon.ClientPackageVersion = proto.Uint32(1771)
	logon.SupportsRateLimitResponse = proto.Bool(!anonymous)
	logon.Steam2TicketRequest = proto.Bool(false)

	return a.Client.Send(protocol.NewClientMsgProtobuf(steamlang.EMsg_ClientLogon, logon))
}

func encryptPasword(pwd string, key *protobuf.CAuthentication_GetPasswordRSAPublicKey_Response) (string, error) {

	var n big.Int
	n.SetString(*key.PublickeyMod, 16)

	exp, err := strconv.ParseInt(*key.PublickeyExp, 16, 32)
	if err != nil {
		return "", err
	}

	pub := rsa.PublicKey{N: &n, E: int(exp)}
	rsaOut, err := rsa.EncryptPKCS1v15(rand.Reader, &pub, []byte(pwd))
	if err != nil {
		return "", err
	}

	return base64.StdEncoding.EncodeToString(rsaOut), nil
}

func (a *Auth) beginAuthSession(packet *protocol.Packet) error {

	body := new(protobuf.CAuthentication_GetPasswordRSAPublicKey_Response)
	_ = packet.ReadProtoMsg(body)

	crypt, _ := encryptPasword(a.Details.Password, body)

	deviceFriendlyName := "DESKTOP-HELLO"
	platformType := protobuf.EAuthTokenPlatformType_k_EAuthTokenPlatformType_SteamClient.Enum()

	deviceDetails := protobuf.CAuthentication_DeviceDetails{
		DeviceFriendlyName: &deviceFriendlyName,
		PlatformType:       platformType,
		OsType:             proto.Int32(16),
		//GamingDeviceType:   proto.Uint32(1),
	}

	req := protobuf.CAuthentication_BeginAuthSessionViaCredentials_Request{
		// DeviceFriendlyName:  &deviceFriendlyName,
		AccountName:         &a.Details.Username,
		EncryptedPassword:   &crypt,
		EncryptionTimestamp: body.Timestamp,
		// RememberLogin:       proto.Bool(true),
		// PlatformType:        platformType,
		Persistence:   protobuf.ESessionPersistence_k_ESessionPersistence_Persistent.Enum(),
		WebsiteId:     proto.String("Client"),
		DeviceDetails: &deviceDetails,
	}

	msg := protocol.NewClientMsgProtobuf(steamlang.EMsg_ServiceMethodCallFromClientNonAuthed, &req)
	jobname := "Authentication.BeginAuthSessionViaCredentials#1"
	msg.Header.Proto.TargetJobName = &jobname
	jobID := a.Client.GetNextJobId()
	msg.SetSourceJobId(jobID) //620515163111425

	a.Client.JobMutex.Lock()
	a.Client.JobHandlers[uint64(jobID)] = a.handleAuthSession
	a.Client.JobMutex.Unlock()

	return a.Client.Send(msg)
}

func (a *Auth) handleAuthSession(packet *protocol.Packet) error {

	body := new(protobuf.CAuthentication_BeginAuthSessionViaCredentials_Response)
	msg := packet.ReadProtoMsg(body)

	_ = msg

	a.authSession = body

	var codeType protobuf.EAuthSessionGuardType
	for _, confirmation := range body.AllowedConfirmations {

		switch *confirmation.ConfirmationType {

		case protobuf.EAuthSessionGuardType_k_EAuthSessionGuardType_None:
			return a.pollAuthSession()
		case protobuf.EAuthSessionGuardType_k_EAuthSessionGuardType_EmailCode:
			codeType = protobuf.EAuthSessionGuardType_k_EAuthSessionGuardType_EmailCode
			fallthrough
		case protobuf.EAuthSessionGuardType_k_EAuthSessionGuardType_DeviceCode:
			if codeType == 0 {
				codeType = protobuf.EAuthSessionGuardType_k_EAuthSessionGuardType_DeviceCode
			}

			if a.Authenticator != nil {
				return a.Authenticator.GetCode(codeType, a.updateAuthSession)
			}

			go func() {
				var code string
				fmt.Println("Enter Code:")
				_, _ = fmt.Scanln(&code)
				a.updateAuthSession(code, codeType)
			}()

		case protobuf.EAuthSessionGuardType_k_EAuthSessionGuardType_DeviceConfirmation:
			fallthrough
		case protobuf.EAuthSessionGuardType_k_EAuthSessionGuardType_EmailConfirmation:

		case protobuf.EAuthSessionGuardType_k_EAuthSessionGuardType_LegacyMachineAuth:

		case protobuf.EAuthSessionGuardType_k_EAuthSessionGuardType_MachineToken:

		case protobuf.EAuthSessionGuardType_k_EAuthSessionGuardType_Unknown:

		}

	}

	return nil

}

func (a *Auth) updateAuthSession(code string, codeType protobuf.EAuthSessionGuardType) error {

	req := protobuf.CAuthentication_UpdateAuthSessionWithSteamGuardCode_Request{
		ClientId: a.authSession.ClientId,
		Steamid:  a.authSession.Steamid,
		Code:     &code,
		CodeType: &codeType,
	}

	msg := protocol.NewClientMsgProtobuf(steamlang.EMsg_ServiceMethodCallFromClientNonAuthed, &req)
	jobname := "Authentication.UpdateAuthSessionWithSteamGuardCode#1"
	msg.Header.Proto.TargetJobName = &jobname
	jobID := a.Client.GetNextJobId()
	msg.SetSourceJobId(jobID) //620515163111425

	a.Client.JobMutex.Lock()
	a.Client.JobHandlers[uint64(jobID)] = a.handleAuthSessionUpdate
	a.Client.JobMutex.Unlock()

	return a.Client.Send(msg)
}

func (a *Auth) handleAuthSessionUpdate(packet *protocol.Packet) error {
	body := new(protobuf.CAuthentication_UpdateAuthSessionWithSteamGuardCode_Response)
	_ = packet.ReadProtoMsg(body)

	a.pollAuthSession()
	return nil
}

func (a *Auth) pollAuthSession() error {

	req := protobuf.CAuthentication_PollAuthSessionStatus_Request{
		ClientId:  a.authSession.ClientId,
		RequestId: a.authSession.RequestId,
	}

	msg := protocol.NewClientMsgProtobuf(steamlang.EMsg_ServiceMethodCallFromClientNonAuthed, &req)
	jobname := "Authentication.PollAuthSessionStatus#1"
	msg.Header.Proto.TargetJobName = &jobname
	jobID := a.Client.GetNextJobId()
	msg.SetSourceJobId(jobID) //620515163111425

	a.Client.JobMutex.Lock()
	a.Client.JobHandlers[uint64(jobID)] = a.handlePollResponse
	a.Client.JobMutex.Unlock()

	return a.Client.Send(msg)
}

func (a *Auth) handlePollResponse(packet *protocol.Packet) error {

	body := new(protobuf.CAuthentication_PollAuthSessionStatus_Response)
	_ = packet.ReadProtoMsg(body)

	if body.RefreshToken == nil {
		return errors.New("AuthSession PollError")
	}

	a.Details.AccessToken = *body.AccessToken
	a.Details.RefreshToken = *body.RefreshToken
	if body.NewGuardData != nil {
		a.Details.GuardData = *body.NewGuardData
	}

	return a.LogOn(a.Details)
}

func (a *Auth) getRSAKey(accountName string) error {

	req := new(protobuf.CAuthentication_GetPasswordRSAPublicKey_Request)
	req.AccountName = &accountName

	msg := protocol.NewClientMsgProtobuf(steamlang.EMsg_ServiceMethodCallFromClientNonAuthed, req)
	jobname := "Authentication.GetPasswordRSAPublicKey#1"
	msg.Header.Proto.TargetJobName = &jobname
	jobID := a.Client.GetNextJobId()
	msg.SetSourceJobId(jobID) //620515163111425

	a.Client.JobMutex.Lock()
	a.Client.JobHandlers[uint64(jobID)] = a.beginAuthSession
	a.Client.JobMutex.Unlock()

	return a.Client.Send(msg)
}

func (a *Auth) LogOnCredentials(details *LogOnDetails) error {

	if details.Username == "" {
		panic("Username must be set!")
	}
	if details.Password == "" && details.LoginKey == "" {
		panic("Password or LoginKey must be set!")
	}

	atomic.StoreUint64(&a.Client.steamId, uint64(steamid.NewIdAdv(0, 1, int32(steamlang.EUniverse_Public), int32(steamlang.EAccountType_Individual))))

	hello := &protobuf.CMsgClientHello{ProtocolVersion: proto.Uint32(steamlang.MsgClientLogon_CurrentProtocol)}
	err := a.Client.Send(protocol.NewClientMsgProtobuf(steamlang.EMsg_ClientHello, hello))
	if err != nil {
		return err
	}

	a.Details = details
	return a.getRSAKey(details.Username)
}

func (a *Auth) HandlePacket(packet *protocol.Packet) {
	var e interface{}
	switch packet.EMsg {
	case steamlang.EMsg_ClientLogOnResponse:
		l, err := a.HandleLogOnResponse(packet)
		if err != nil {
			a.Client.Fatalf(err.Error())
		} else if l.Result != steamlang.EResult_OK {
			e = &LogOnFailedEvent{Result: l.Result}
		} else {
			e = l
		}
	case steamlang.EMsg_ClientNewLoginKey:
		e, _ = a.HandleLoginKey(packet)
	case steamlang.EMsg_ClientSessionToken:
	case steamlang.EMsg_ClientLoggedOff:
		e = a.HandleLoggedOff(packet)
	case steamlang.EMsg_ClientUpdateMachineAuth:
		e, _ = a.HandleUpdateMachineAuth(packet)
	case steamlang.EMsg_ClientAccountInfo:
		e = a.HandleAccountInfo(packet)
	}

	if e != nil {
		a.Client.Emit(e)
	}
}

func (a *Auth) HandleLogOnResponse(packet *protocol.Packet) (*LoggedOnEvent, error) {
	if !packet.IsProto {
		return nil, errors.New("Got non-proto logon response!")
	}

	body := new(protobuf.CMsgClientLogonResponse)
	msg := packet.ReadProtoMsg(body)

	result := steamlang.EResult(body.GetEresult())
	if result == steamlang.EResult_OK {
		atomic.StoreInt32(&a.Client.sessionId, msg.Header.Proto.GetClientSessionid())
		atomic.StoreUint64(&a.Client.steamId, msg.Header.Proto.GetSteamid())
		if a.Client.Web != nil && body.WebapiAuthenticateUserNonce != nil {
			a.Client.Web.webLoginKey = *body.WebapiAuthenticateUserNonce
		}

		if !a.Client.manual {
			go a.Client.heartbeatLoop(time.Duration(body.GetHeartbeatSeconds()))
		}

		return &LoggedOnEvent{
			Result:                    steamlang.EResult(body.GetEresult()),
			ExtendedResult:            steamlang.EResult(body.GetEresultExtended()),
			OutOfGameSecsPerHeartbeat: body.GetLegacyOutOfGameHeartbeatSeconds(),
			InGameSecsPerHeartbeat:    body.GetHeartbeatSeconds(),
			PublicIp:                  body.GetDeprecatedPublicIp(),
			ServerTime:                body.GetRtime32ServerTime(),
			AccountFlags:              steamlang.EAccountFlags(body.GetAccountFlags()),
			ClientSteamId:             steamid.SteamId(body.GetClientSuppliedSteamid()),
			EmailDomain:               body.GetEmailDomain(),
			CellId:                    body.GetCellId(),
			CellIdPingThreshold:       body.GetCellIdPingThreshold(),
			Steam2Ticket:              body.GetSteam2Ticket(),
			UsePics:                   body.GetDeprecatedUsePics(),
			WebApiUserNonce:           body.GetWebapiAuthenticateUserNonce(),
			IpCountryCode:             body.GetIpCountryCode(),
			VanityUrl:                 body.GetVanityUrl(),
			NumLoginFailuresToMigrate: body.GetCountLoginfailuresToMigrate(),
			NumDisconnectsToMigrate:   body.GetCountDisconnectsToMigrate(),
		}, nil
	} /*else if result == steamlang.EResult_Fail || result == steamlang.EResult_ServiceUnavailable || result == steamlang.EResult_TryAnotherCM {
		// some error on Steam's side, we'll get an EOF later
	} else {
		a.client.Emit(&LogOnFailedEvent{
			Result: result,
		})
		a.client.Disconnect()
	}*/

	a.Client.Disconnect()
	return &LoggedOnEvent{Result: result}, nil
}

func (a *Auth) HandleLoginKey(packet *protocol.Packet) (*LoginKeyEvent, error) {
	body := new(protobuf.CMsgClientNewLoginKey)
	packet.ReadProtoMsg(body)
	err := a.Client.Send(protocol.NewClientMsgProtobuf(steamlang.EMsg_ClientNewLoginKeyAccepted, &protobuf.CMsgClientNewLoginKeyAccepted{
		UniqueId: proto.Uint32(body.GetUniqueId()),
	}))
	if err != nil {
		return nil, err
	}

	return &LoginKeyEvent{
		UniqueId: body.GetUniqueId(),
		LoginKey: body.GetLoginKey(),
	}, nil
}

func (a *Auth) HandleLoggedOff(packet *protocol.Packet) *LoggedOffEvent {
	result := steamlang.EResult_Invalid
	var min int32
	if packet.IsProto {
		body := new(protobuf.CMsgClientLoggedOff)
		packet.ReadProtoMsg(body)
		result = steamlang.EResult(body.GetEresult())
	} else {
		body := new(steamlang.MsgClientLoggedOff)
		packet.ReadClientMsg(body)
		result = body.Result
		min = body.SecMinReconnectHint
	}
	return &LoggedOffEvent{Result: result, MinReconnect: min}
}

func (a *Auth) HandleUpdateMachineAuth(packet *protocol.Packet) (*MachineAuthUpdateEvent, error) {
	body := new(protobuf.CMsgClientUpdateMachineAuth)
	packet.ReadProtoMsg(body)
	hash := sha1.New()
	hash.Write(packet.Data)
	sha := hash.Sum(nil)

	msg := protocol.NewClientMsgProtobuf(steamlang.EMsg_ClientUpdateMachineAuthResponse, &protobuf.CMsgClientUpdateMachineAuthResponse{
		ShaFile: sha,
	})
	msg.SetTargetJobId(packet.SourceJobId)
	err := a.Client.Send(msg)
	if err != nil {
		return nil, err
	}

	return &MachineAuthUpdateEvent{sha}, nil
}

func (a *Auth) HandleAccountInfo(packet *protocol.Packet) *AccountInfoEvent {
	body := new(protobuf.CMsgClientAccountInfo)
	packet.ReadProtoMsg(body)
	return &AccountInfoEvent{
		PersonaName:          body.GetPersonaName(),
		Country:              body.GetIpCountry(),
		CountAuthedComputers: body.GetCountAuthedComputers(),
		AccountFlags:         steamlang.EAccountFlags(body.GetAccountFlags()),
		FacebookId:           body.GetFacebookId(),
		FacebookName:         body.GetFacebookName(),
	}
}
