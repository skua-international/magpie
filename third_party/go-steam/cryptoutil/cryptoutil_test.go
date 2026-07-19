package cryptoutil

import (
	"crypto/aes"
	"testing"
)

func TestCrypt(t *testing.T) {
	src := []byte("Hello World!")
	key := []byte("hunter2         ") // key size of 16 bytes required
	ciph, err := aes.NewCipher(key)
	if err != nil {
		t.Fatal(err)
	}
	encrypted := SymmetricEncrypt(ciph, nil, src)
	if len(encrypted)%aes.BlockSize != 0 {
		t.Fatalf("Encrypted text is not a multiple of the AES block size (got %v)", len(encrypted))
	}
	decrypted, _ := SymmetricDecrypt(ciph, encrypted)
	if len([]byte("Hello World!")) != len(decrypted) {
		t.Fatalf("src length (%v) does not match decrypted length (%v)!", len([]byte("Hello World!")), len(decrypted))
	}
}
