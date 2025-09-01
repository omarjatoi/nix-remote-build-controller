package main

import (
	"fmt"
	"os"

	"github.com/spf13/cobra"
)

var version = "dev"

var rootCmd = &cobra.Command{
	Use:   "proxy",
	Short: "SSH proxy server for Nix remote builders",
	Long:  "An SSH proxy that routes Nix build requests to dynamic Kubernetes builder pods",
	Run: func(cmd *cobra.Command, args []string) {
		fmt.Println("Starting Nix remote builder SSH proxy...")
	},
}

var versionCmd = &cobra.Command{
	Use:   "version",
	Short: "Print the version number",
	Run: func(cmd *cobra.Command, args []string) {
		fmt.Printf("nix-remote-build-proxy version %s\n", version)
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