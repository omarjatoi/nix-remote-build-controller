package proxy

import (
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"fmt"
	"os"

	"golang.org/x/crypto/ssh"
	corev1 "k8s.io/api/core/v1"
	"sigs.k8s.io/controller-runtime/pkg/client"
)

const (
	// SSHKeySecretPrivateKey is the key in the secret containing the private key
	// the proxy uses to authenticate to builder pods.
	SSHKeySecretPrivateKey = "private"
	// SSHKeySecretPublicKey is the key in the secret containing the matching
	// public key, mounted into builder pods as authorized_keys.
	SSHKeySecretPublicKey = "public"
	// SSHKeySecretHostKey is the key in the secret containing the proxy's SSH host key.
	SSHKeySecretHostKey = "host-key"
	// SSHKeySecretBuilderHostKey is the optional key in the secret containing a
	// host key shared by all builder pods. When present the proxy verifies
	// builder host keys instead of accepting any key.
	SSHKeySecretBuilderHostKey = "builder-host-key"
	// SSHKeySecretClientAuthorizedKeys is the optional key in the secret with
	// the authorized_keys of Nix clients allowed to use the proxy.
	SSHKeySecretClientAuthorizedKeys = "client-authorized-keys"
)

// getSecret fetches the SSH key secret.
func getSecret(ctx context.Context, c client.Client, namespace, name string) (*corev1.Secret, error) {
	var secret corev1.Secret
	if err := c.Get(ctx, client.ObjectKey{Namespace: namespace, Name: name}, &secret); err != nil {
		return nil, fmt.Errorf("get secret %s/%s: %w", namespace, name, err)
	}
	return &secret, nil
}

// signerFromSecret parses the private key stored under key in the secret.
// The second return value is false when the key is absent.
func signerFromSecret(secret *corev1.Secret, key string) (ssh.Signer, bool, error) {
	data, ok := secret.Data[key]
	if !ok || len(data) == 0 {
		return nil, false, nil
	}
	signer, err := ssh.ParsePrivateKey(data)
	if err != nil {
		return nil, true, fmt.Errorf("parse %q from secret %s: %w", key, secret.Name, err)
	}
	return signer, true, nil
}

// generateHostKey creates an ephemeral ed25519 host key. It is only used when
// no persistent host key is configured; clients will see a changed host key on
// every restart.
func generateHostKey() (ssh.Signer, error) {
	_, priv, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		return nil, err
	}
	return ssh.NewSignerFromKey(priv)
}

func loadHostKey(path string) (ssh.Signer, error) {
	keyBytes, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	return ssh.ParsePrivateKey(keyBytes)
}
