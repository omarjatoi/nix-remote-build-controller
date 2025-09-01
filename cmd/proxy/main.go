package main

import (
	"context"
	"fmt"
	"os"
	"os/signal"
	"syscall"

	"github.com/omarjatoi/nix-remote-build-controller/pkg/proxy"
	"github.com/rs/zerolog/log"
	"github.com/spf13/cobra"
)

var version = "dev"
var port int
var hostKeyPath string
var namespace string
var remoteUser string
var remotePort int32

var rootCmd = &cobra.Command{
	Use:   "proxy",
	Short: "SSH proxy server for Nix remote builders",
	Long:  "An SSH proxy that routes Nix build requests to dynamic Kubernetes builder pods",
	Run: func(cmd *cobra.Command, args []string) {
		ctx, cancel := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
		defer cancel()

		addr := fmt.Sprintf(":%d", port)
		sshProxy, err := proxy.NewSSHProxy(addr, hostKeyPath, namespace, remoteUser, remotePort)
		if err != nil {
			log.Fatal().Err(err).Msg("Failed to create SSH proxy")
		}

		log.Info().Int("port", port).Msg("Starting Nix remote builder SSH proxy")
		if err := sshProxy.Start(ctx); err != nil && err != context.Canceled {
			log.Fatal().Err(err).Msg("Failed to start SSH proxy")
		}
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
	rootCmd.Flags().IntVarP(&port, "port", "p", 2222, "SSH proxy server port")
	rootCmd.Flags().StringVarP(&hostKeyPath, "host-key", "k", "", "Path to provided SSH host private key file")
	rootCmd.Flags().StringVarP(&namespace, "namespace", "n", "default", "Kubernetes namespace for build requests")
	rootCmd.Flags().StringVarP(&remoteUser, "remote-user", "u", "root", "SSH username for builder pods")
	rootCmd.Flags().Int32VarP(&remotePort, "remote-port", "r", 22, "SSH port on builder pods")
	rootCmd.AddCommand(versionCmd)
}

func main() {
	if err := rootCmd.Execute(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
