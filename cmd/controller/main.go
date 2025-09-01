package main

import (
	"fmt"
	"os"

	"github.com/rs/zerolog/log"
	"github.com/spf13/cobra"
)

var version = "dev"

var rootCmd = &cobra.Command{
	Use:   "controller",
	Short: "Kubernetes controller for Nix remote builders",
	Long:  "A Kubernetes controller that manages dynamic Nix remote builder pods",
	Run: func(cmd *cobra.Command, args []string) {
		log.Info().Msg("Starting Nix remote builder controller")
	},
}

var versionCmd = &cobra.Command{
	Use:   "version",
	Short: "Print the version number",
	Run: func(cmd *cobra.Command, args []string) {
		fmt.Printf("nix-remote-build-controller version %s\n", version)
	},
}

func init() {
	rootCmd.AddCommand(versionCmd)
}

func main() {
	if err := rootCmd.Execute(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
