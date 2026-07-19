package steam

import (
	"archive/zip"
	"bytes"
	"crypto/aes"
	"encoding/binary"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"sync"

	"github.com/0xAozora/go-steam/cryptoutil"
	"github.com/0xAozora/go-steam/protocol"
	"github.com/0xAozora/go-steam/protocol/protobuf"
	"github.com/0xAozora/go-steam/protocol/protobuf/unified"
	"github.com/0xAozora/go-steam/protocol/steamlang"
	"google.golang.org/protobuf/proto"

	"github.com/itchio/lzma"
)

const (
	PROTOBUF_PAYLOAD_MAGIC       = 0x71F617D0
	PROTOBUF_METADATA_MAGIC      = 0x1F4812BE
	PROTOBUF_SIGNATURE_MAGIC     = 0x1B81B817
	PROTOBUF_ENDOFMANIFEST_MAGIC = 0x32C415AB
	STEAM3_MANIFEST_MAGIC        = 0x16349781
)

type Manifest struct {
	Payload protobuf.ContentManifestPayload
	Meta    protobuf.ContentManifestMetadata
}

func (c *Client) GetCDNAuthToken(appID, depotID uint32, host string) string {
	return ""
}

func (c *Client) GetManifestRequestCode(appID, depotID uint32, manifestID uint64, appBranch, passwordHash *string) (uint64, error) {

	req := &unified.CContentServerDirectory_GetManifestRequestCode_Request{
		AppId:              &appID,
		DepotId:            &depotID,
		ManifestId:         &manifestID,
		AppBranch:          appBranch,
		BranchPasswordHash: passwordHash,
	}

	msg := protocol.NewClientMsgProtobuf(steamlang.EMsg_ServiceMethodCallFromClient, req)
	jobname := "ContentServerDirectory.GetManifestRequestCode#1"
	msg.Header.Proto.TargetJobName = &jobname
	jobID := c.GetNextJobId()
	msg.SetSourceJobId(jobID)

	ch := make(chan uint64)

	c.JobMutex.Lock()
	c.JobHandlers[uint64(jobID)] = func(packet *protocol.Packet) error {
		body := new(unified.CContentServerDirectory_GetManifestRequestCode_Response)
		_ = packet.ReadProtoMsg(body)
		ch <- *body.ManifestRequestCode
		return nil
	}
	c.JobMutex.Unlock()

	//msg.SetTargetJobId(protocol.JobId(18446744073709551615))

	err := c.Send(msg)
	if err != nil {
		c.JobMutex.Lock()
		delete(c.JobHandlers, uint64(jobID))
		c.JobMutex.Unlock()
		return 0, err
	}
	return <-ch, nil
}

func (c *Client) GetManifest(appID, depotID uint32, manifestID uint64, appBranch, passwordHash *string) (*Manifest, error) {

	reqCode, err := c.GetManifestRequestCode(appID, depotID, manifestID, appBranch, passwordHash)
	if err != nil {
		return nil, err
	}

	u, _ := url.ParseRequestURI(fmt.Sprintf("https://%s/depot/%d/manifest/%d/5/%d", "cache9-fra1.steamcontent.com", depotID, manifestID, reqCode))
	reader, err := download(u)
	if err != nil {
		return nil, err
	}

	b, _ := io.ReadAll(reader)
	hdr := binary.LittleEndian.Uint32(b[4:])
	if hdr == 0x504b0304 {
		fmt.Println("Is zip")
	}

	r := bytes.NewReader(b)
	z, _ := zip.NewReader(r, int64(len(b)))

	f := z.File[0]
	reader, _ = f.Open()
	b, _ = io.ReadAll(reader) // Read everything at once cause zip.Reader is broken.

	var manifest Manifest

	var l uint32
outer:
	for len(b) > 8 {
		switch binary.LittleEndian.Uint32(b) {

		case PROTOBUF_PAYLOAD_MAGIC:
			l = binary.LittleEndian.Uint32(b[4:])
			if err = proto.Unmarshal(b[8:8+l], &manifest.Payload); err != nil {
				return nil, err
			}
			b = b[8+l:]

		case PROTOBUF_METADATA_MAGIC:
			l = binary.LittleEndian.Uint32(b[4:])
			if err = proto.Unmarshal(b[8:8+l], &manifest.Meta); err != nil {
				return nil, err
			}
			b = b[8+l:]

		case PROTOBUF_SIGNATURE_MAGIC:
			// Skip Signature
			l = binary.LittleEndian.Uint32(b[4:])
			b = b[8+l:]

		case PROTOBUF_ENDOFMANIFEST_MAGIC:
			break outer
		case STEAM3_MANIFEST_MAGIC:
			return nil, errors.New("Not implemented")
		default:
			return nil, errors.New("Unknown magic value")
		}

	}

	return &manifest, nil
}

func (c *Client) GetDepotDecryptionKey(appID, depotID uint32) ([]byte, error) {

	req := &protobuf.CMsgClientGetDepotDecryptionKey{
		AppId:   &appID,
		DepotId: &depotID,
	}

	msg := protocol.NewClientMsgProtobuf(steamlang.EMsg_ClientGetDepotDecryptionKey, req)
	jobID := c.GetNextJobId()
	msg.SetSourceJobId(jobID)

	ch := make(chan []byte)

	c.JobMutex.Lock()
	c.JobHandlers[uint64(jobID)] = func(packet *protocol.Packet) error {
		body := new(protobuf.CMsgClientGetDepotDecryptionKeyResponse)
		_ = packet.ReadProtoMsg(body)
		ch <- body.DepotEncryptionKey
		return nil
	}
	c.JobMutex.Unlock()

	err := c.Send(msg)
	if err != nil {
		c.JobMutex.Lock()
		delete(c.JobHandlers, uint64(jobID))
		c.JobMutex.Unlock()
		return nil, err
	}
	return <-ch, nil
}

func (c *Client) DownloadFile(appID, depotID uint32, key []byte, fileManifest *protobuf.ContentManifestPayload_FileMapping) ([]byte, error) {

	cipher, _ := aes.NewCipher(key)

	var err error
	var wg sync.WaitGroup
	ch := make(chan struct{}, 4)
	b := make([]byte, *fileManifest.Size)

	for _, cc := range fileManifest.Chunks {

		chunk := cc // For closure, Go 1.22 don't work

		ch <- struct{}{}
		wg.Add(1)

		go func() {

			u, _ := url.ParseRequestURI(fmt.Sprintf("https://%s/depot/%d/chunk/%s%s", "cache9-fra1.steamcontent.com", depotID, hex.EncodeToString(chunk.Sha), ""))

			r, _ := download(u)
			c, _ := io.ReadAll(r)

			d, e := cryptoutil.SymmetricDecrypt(cipher, c)
			if e != nil {
				err = e
			} else {
				v, e := decompressVZip(d)
				if e != nil {
					err = e // Cant be bothered to retry / cancel
				}

				copy(b[*chunk.Offset:], v)
			}

			wg.Done()
			<-ch
		}()
	}

	wg.Wait()
	return b, err
}

func download(u *url.URL) (io.Reader, error) {

	req, err := http.NewRequest(http.MethodGet, u.String(), nil)
	if err != nil {
		return nil, err
	}

	req.Header = map[string][]string{
		"Host":            {u.Host},
		"Accept":          {"text/html,*/*;q=0.9"},
		"Accept-Encoding": {"gzip,identity,*;q=0"},
		"Accept-Charset":  {"ISO-8859-1,utf-8,*;q=0.7"},
		"User-Agent":      {"Valve/Steam HTTP Client 1.0"},
	}

	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, err
	}

	return resp.Body, nil
}

/*
	VZip

+------------------------------------------------------------------------+
|      Hdr (2B)      | Ver (1B) |          Timestamp / CRC (4B)          |
+------------------------------------------------------------------------+
|                    Props (5B)                    ||      Data ...      |
+------------------------------------------------------------------------+
| ...                           ||         Decompressed CRC (4B)         |
+------------------------------------------------------------------------+
|         Decompressed Size (4B)         |  VZip Footer (2B)  |          |
+------------------------------------------------------------------------+
*/
func decompressVZip(vz []byte) ([]byte, error) {

	if vz[2] != 'a' {
		return nil, fmt.Errorf("Version \"%db\" not supported", vz[2])
	} else if footer := binary.LittleEndian.Uint16(vz[len(vz)-2:]); footer != 30330 {
		return nil, fmt.Errorf("Unknown Footer \"%d\"", footer)
	}

	l := len(vz)
	b := make([]byte, l-5) // len(Data) + 8 byte

	copy(b, vz[7:12])         // Props
	copy(b[5:], vz[l-6:l-2])  // Decompressed Size
	copy(b[13:], vz[12:l-10]) // Data

	r := lzma.NewReader(bytes.NewReader(b))

	decompressedData, err := io.ReadAll(r)
	if err != nil {
		return nil, err
	}

	return decompressedData, nil
}
