package cryptoutil

import (
	"crypto/aes"
	"errors"
)

// Returns a new byte array padded with PKCS7 and prepended
// with empty space of the AES block size (16 bytes) for the IV.
func padPKCS7WithIV(dest, src []byte) []byte {
	missing := aes.BlockSize - (len(src) % aes.BlockSize)
	newSize := len(src) + aes.BlockSize + missing

	// Check if dest actually has the capacity to avoid allocating new slice
	if cap(dest) >= newSize {
		dest = dest[:newSize]
	} else {
		dest = make([]byte, newSize)
	}

	copy(dest[aes.BlockSize:], src)

	padding := byte(missing)
	for i := newSize - missing; i < newSize; i++ {
		dest[i] = padding
	}
	return dest
}

func unpadPKCS7(src []byte) ([]byte, error) {
	if len(src) == 0 {
		return nil, errors.New("cannot unpad empty bytes")
	}

	padLen := src[len(src)-1]
	if len(src)-int(padLen) < 0 {
		return nil, errors.New("negative pkcs7 size")
	}

	return src[:len(src)-int(padLen)], nil
}
