package proxy

import (
	"bufio"
	"bytes"
	"fmt"
	"os"
	"strings"

	"golang.org/x/crypto/ssh"
)

// PermissionExtensionFingerprint is the key under which the authenticated
// client's public key fingerprint is stored in ssh.Permissions.Extensions.
const PermissionExtensionFingerprint = "nix.io/pubkey-fingerprint"

// ClientAuthorizer authenticates Nix clients against an authorized_keys list.
type ClientAuthorizer struct {
	// keys maps the wire-format public key to its authorized_keys comment.
	keys map[string]string
}

// LoadAuthorizedKeys reads an OpenSSH authorized_keys file.
func LoadAuthorizedKeys(path string) (*ClientAuthorizer, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("read authorized keys: %w", err)
	}
	return ParseAuthorizedKeys(data)
}

// ParseAuthorizedKeys parses authorized_keys content. Blank lines and comments
// are ignored; any unparsable line is an error so that a typo does not silently
// lock out (or fail to lock out) a client.
func ParseAuthorizedKeys(data []byte) (*ClientAuthorizer, error) {
	a := &ClientAuthorizer{keys: make(map[string]string)}
	scanner := bufio.NewScanner(bytes.NewReader(data))
	lineNo := 0
	for scanner.Scan() {
		lineNo++
		line := strings.TrimSpace(scanner.Text())
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		key, comment, _, _, err := ssh.ParseAuthorizedKey([]byte(line))
		if err != nil {
			return nil, fmt.Errorf("authorized keys line %d: %w", lineNo, err)
		}
		a.keys[string(key.Marshal())] = comment
	}
	if err := scanner.Err(); err != nil {
		return nil, err
	}
	if len(a.keys) == 0 {
		return nil, fmt.Errorf("authorized keys contain no keys")
	}
	return a, nil
}

// Len returns the number of authorized keys.
func (a *ClientAuthorizer) Len() int { return len(a.keys) }

// Callback implements ssh.ServerConfig.PublicKeyCallback.
func (a *ClientAuthorizer) Callback(conn ssh.ConnMetadata, key ssh.PublicKey) (*ssh.Permissions, error) {
	if _, ok := a.keys[string(key.Marshal())]; !ok {
		return nil, fmt.Errorf("public key %s for user %q is not authorized", ssh.FingerprintSHA256(key), conn.User())
	}
	return &ssh.Permissions{
		Extensions: map[string]string{
			PermissionExtensionFingerprint: ssh.FingerprintSHA256(key),
		},
	}, nil
}
