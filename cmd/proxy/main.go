package main

import (
	"context"
	"fmt"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/rs/zerolog"
	"github.com/rs/zerolog/log"
	"github.com/spf13/cobra"

	"github.com/omarjatoi/nix-remote-build-controller/pkg/proxy"
)

var version = "dev"

var (
	port                     int
	healthPort               int
	hostKeyPath              string
	clientAuthorizedKeysPath string
	namespace                string
	remoteUser               string
	remotePort               int32
	sshKeySecret             string
	podReadyTimeout          time.Duration
	buildTimeout             time.Duration
	shutdownTimeout          time.Duration
	keepaliveInterval        time.Duration
	maxSessions              int
	ownerPodName             string
	ownerPodUID              string
	logLevel                 string
)

var rootCmd = &cobra.Command{
	Use:   "proxy",
	Short: "SSH proxy server for Nix remote builders",
	Long: `An SSH proxy that routes Nix build sessions to dynamically provisioned
Kubernetes builder pods. Every SSH session channel becomes an independent
NixBuildRequest and builder pod, so concurrent Nix builds run in parallel.`,
	RunE: func(cmd *cobra.Command, args []string) error {
		level, err := zerolog.ParseLevel(logLevel)
		if err != nil {
			return fmt.Errorf("invalid --log-level %q: %w", logLevel, err)
		}
		zerolog.SetGlobalLevel(level)

		ctx, cancel := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
		defer cancel()

		cfg := proxy.Config{
			ListenAddr:        fmt.Sprintf(":%d", port),
			HealthAddr:        fmt.Sprintf(":%d", healthPort),
			Namespace:         namespace,
			RemoteUser:        remoteUser,
			RemotePort:        remotePort,
			PodReadyTimeout:   podReadyTimeout,
			BuildTimeout:      buildTimeout,
			ShutdownTimeout:   shutdownTimeout,
			KeepaliveInterval: keepaliveInterval,
			MaxSessions:       maxSessions,
			OwnerPodName:      ownerPodName,
			OwnerPodUID:       ownerPodUID,
		}

		sshProxy, err := proxy.NewFromCluster(ctx, cfg, hostKeyPath, sshKeySecret, clientAuthorizedKeysPath)
		if err != nil {
			return fmt.Errorf("create SSH proxy: %w", err)
		}

		log.Info().Str("version", version).Int("port", port).Msg("Starting Nix remote builder SSH proxy")
		if err := sshProxy.Start(ctx); err != nil {
			return fmt.Errorf("SSH proxy failed: %w", err)
		}
		log.Info().Msg("SSH proxy stopped")
		return nil
	},
}

var versionCmd = &cobra.Command{
	Use:   "version",
	Short: "Print the version number",
	Run: func(cmd *cobra.Command, args []string) {
		fmt.Printf("v%s\n", version)
	},
}

func init() {
	f := rootCmd.Flags()
	f.IntVarP(&port, "port", "p", 2222, "SSH proxy server port")
	f.IntVar(&healthPort, "health-port", 8080, "Health check server port")
	f.StringVarP(&hostKeyPath, "host-key", "k", "", "Path to the proxy's SSH host private key (default: 'host-key' entry of the SSH key secret, else an ephemeral key)")
	f.StringVar(&clientAuthorizedKeysPath, "client-authorized-keys", "", "Path to an authorized_keys file of Nix clients allowed to connect (default: 'client-authorized-keys' entry of the SSH key secret; if neither is set, authentication is disabled)")
	f.StringVarP(&namespace, "namespace", "n", "default", "Kubernetes namespace for build requests")
	f.StringVarP(&remoteUser, "remote-user", "u", "nixbld", "SSH username on builder pods")
	f.Int32VarP(&remotePort, "remote-port", "r", 22, "SSH port on builder pods")
	f.StringVar(&sshKeySecret, "ssh-key-secret", "nix-builder-ssh-keys", "Secret containing the SSH keypair used to authenticate to builder pods (keys 'private' and 'public')")
	f.DurationVar(&podReadyTimeout, "pod-ready-timeout", proxy.DefaultPodReadyTimeout, "How long a session waits for its builder pod to become ready")
	f.DurationVar(&buildTimeout, "build-timeout", time.Hour, "Maximum lifetime of a builder pod (NixBuildRequest.spec.timeoutSeconds); 0 uses the CRD default")
	f.DurationVar(&shutdownTimeout, "shutdown-timeout", proxy.DefaultShutdownTimeout, "How long in-flight build sessions may continue after SIGTERM before being closed")
	f.DurationVar(&keepaliveInterval, "keepalive-interval", proxy.DefaultKeepaliveInterval, "SSH keepalive interval towards clients and builders (0 disables)")
	f.IntVar(&maxSessions, "max-sessions", 0, "Maximum number of concurrent build sessions (0 = unlimited)")
	f.StringVar(&ownerPodName, "owner-pod-name", os.Getenv("PROXY_POD_NAME"), "Name of this proxy's pod; NixBuildRequests get an ownerReference to it (env PROXY_POD_NAME)")
	f.StringVar(&ownerPodUID, "owner-pod-uid", os.Getenv("PROXY_POD_UID"), "UID of this proxy's pod (env PROXY_POD_UID)")
	f.StringVar(&logLevel, "log-level", "info", "Log level (trace, debug, info, warn, error)")
	rootCmd.AddCommand(versionCmd)
}

func main() {
	if err := rootCmd.Execute(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
