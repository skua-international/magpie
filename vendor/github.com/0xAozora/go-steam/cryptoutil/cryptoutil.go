package cryptoutil

import (
	"crypto/aes"
	"crypto/cipher"
	"crypto/rand"
)

// Performs an encryption using AES/CBC/PKCS7
// with a random IV prepended using AES/ECB/None.
// If dest is nil, or is not large enough, a new slice is allocated.
// so its not guaranteed that dest contains the result.
func SymmetricEncrypt(ciph cipher.Block, dest, src []byte) []byte {
	// get a random IV and ECB encrypt it
	iv := make([]byte, aes.BlockSize)
	_, err := rand.Read(iv)
	if err != nil {
		panic(err)
	}
	encryptedIv := make([]byte, aes.BlockSize)
	newECBEncrypter(ciph).CryptBlocks(encryptedIv, iv)

	// pad it, copy the IV to the first 16 bytes and encrypt the rest with CBC
	encrypted := padPKCS7WithIV(dest, src)
	copy(encrypted, encryptedIv)
	cipher.NewCBCEncrypter(ciph, iv).CryptBlocks(encrypted[aes.BlockSize:], encrypted[aes.BlockSize:])
	return encrypted
}

// Decrypts data from the reader using AES/CBC/PKCS7 with an IV
// prepended using AES/ECB/None. The src slice may not be used anymore.
func SymmetricDecrypt(ciph cipher.Block, src []byte) ([]byte, error) {
	iv := src[:aes.BlockSize]
	newECBDecrypter(ciph).CryptBlocks(iv, iv)

	data := src[aes.BlockSize:]
	cipher.NewCBCDecrypter(ciph, iv).CryptBlocks(data, data)

	return unpadPKCS7(data)
}
