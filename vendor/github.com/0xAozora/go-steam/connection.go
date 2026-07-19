package steam

import (
	"crypto/aes"
	"crypto/cipher"
	"encoding/binary"
	"fmt"
	"io"
	"net"
	"sync"

	"github.com/0xAozora/go-steam/cryptoutil"
	"github.com/0xAozora/go-steam/protocol"
	"golang.org/x/net/proxy"
)

// This interface exists to allow for different connection types (TCP, UDP, WebSocket??), however only TCP is implemented right now.
type connection interface {
	Read() (*protocol.Packet, error)
	Write([]byte) error
	Close() error
	SetEncryptionKey([]byte)
	IsEncrypted() bool
}

const tcpConnectionMagic uint32 = 0x31305456 // "VT01"

type tcpConnection struct {
	conn        net.Conn
	ciph        cipher.Block
	cipherMutex sync.RWMutex
}

func dialTCP(laddr, raddr *net.TCPAddr, proxy proxy.Dialer) (*tcpConnection, error) {

	var conn net.Conn
	var err error
	if proxy != nil {
		conn, err = proxy.Dial("tcp", raddr.String())
	} else {
		conn, err = net.DialTCP("tcp", laddr, raddr)
	}

	if err != nil {
		return nil, err
	}

	return &tcpConnection{
		conn: conn,
	}, nil
}

func (c *tcpConnection) Read() (*protocol.Packet, error) {

	// All packets begin with a packet length and a magic value
	var b []byte = make([]byte, 8)
	n, err := c.conn.Read(b)
	if err != nil {
		return nil, err
	}
	if n != 8 {
		return nil, fmt.Errorf("Expected 8 bytes, got %d", n)
	}

	// Check magic value first to validate the connection
	packetMagic := binary.LittleEndian.Uint32(b[4:])
	if packetMagic != tcpConnectionMagic {
		return nil, fmt.Errorf("Invalid connection magic! Expected %d, got %d!", tcpConnectionMagic, packetMagic)
	}

	packetLen := binary.LittleEndian.Uint32(b)
	buf := make([]byte, packetLen)
	_, err = io.ReadFull(c.conn, buf)
	if err == io.ErrUnexpectedEOF {
		return nil, io.EOF
	}
	if err != nil {
		return nil, err
	}

	// Packets after ChannelEncryptResult are encrypted
	c.cipherMutex.RLock()
	if c.ciph != nil {
		buf, err = cryptoutil.SymmetricDecrypt(c.ciph, buf)
	}
	c.cipherMutex.RUnlock()

	if err != nil {
		return nil, err
	}

	return protocol.NewPacket(buf)
}

// Writes a message. This may only be used by one goroutine at a time.
// Reuses message slice if possible, otherwise allocates a new one.
func (c *tcpConnection) Write(message []byte) error {

	// Allocate a new slice for the whole payload to avoid packet splitting
	// and to avoid copying the message multiple times when encrypting.
	size := len(message)
	var payload []byte

	c.cipherMutex.RLock()
	if c.ciph != nil {
		// Payload contains the length of the message, the magic value and the message itself.
		// Message is padded to the next AES block size + AES block size for the IV.
		// So we need to allocate a new slice larger enough.
		missing := aes.BlockSize - (size % aes.BlockSize)
		size += aes.BlockSize + missing
		payload = checkReuseSlice(message, size+8)
		_ = cryptoutil.SymmetricEncrypt(c.ciph, payload[8:], message)
	} else {
		// Payload contains the length of the message, the magic value and the message itself.
		payload = checkReuseSlice(message, size+8)
		copy(payload[8:], message)
	}
	c.cipherMutex.RUnlock()

	binary.LittleEndian.PutUint32(payload, uint32(size))
	binary.LittleEndian.PutUint32(payload[4:], tcpConnectionMagic)

	_, err := c.conn.Write(payload)
	return err
}

func (c *tcpConnection) Close() error {
	return c.conn.Close()
}

func (c *tcpConnection) SetEncryptionKey(key []byte) {
	c.cipherMutex.Lock()
	defer c.cipherMutex.Unlock()
	if key == nil {
		c.ciph = nil
		return
	}
	if len(key) != 32 {
		panic("Connection AES key is not 32 bytes long!")
	}

	var err error
	c.ciph, err = aes.NewCipher(key)
	if err != nil {
		panic(err)
	}
}

func (c *tcpConnection) IsEncrypted() bool {
	c.cipherMutex.RLock()
	defer c.cipherMutex.RUnlock()
	return c.ciph != nil
}

// checkReuseSlice checks if the slice has enough capacity to hold the new size.
// If it does, it returns the slice with the new size.
// If it doesn't, it allocates a new slice with the new size.
func checkReuseSlice(s []byte, size int) []byte {
	if cap(s) >= size {
		return s[:size]
	}
	return make([]byte, size)
}
