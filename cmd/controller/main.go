package main

import (
	"context"
	"fmt"
	"os"
	"os/signal"
	"syscall"

	"github.com/omarjatoi/nix-remote-build-controller/pkg/apis/nixbuilder/v1alpha1"
	"github.com/omarjatoi/nix-remote-build-controller/pkg/controller"
	"github.com/rs/zerolog/log"
	"github.com/spf13/cobra"
	"k8s.io/apimachinery/pkg/runtime"
	clientgoscheme "k8s.io/client-go/kubernetes/scheme"
	ctrl "sigs.k8s.io/controller-runtime"
)

var (
	version      = "dev"
	builderImage string
	sshPort      int32
	nixConfigMap string
)

var rootCmd = &cobra.Command{
	Use:   "controller",
	Short: "Kubernetes controller for Nix remote builders",
	Long:  "A Kubernetes controller that manages dynamic Nix remote builder pods",
	Run: func(cmd *cobra.Command, args []string) {
		ctx, cancel := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
		defer cancel()

		scheme := runtime.NewScheme()
		if err := clientgoscheme.AddToScheme(scheme); err != nil {
			log.Fatal().Err(err).Msg("Failed to add client-go scheme")
		}
		if err := v1alpha1.AddToScheme(scheme); err != nil {
			log.Fatal().Err(err).Msg("Failed to add NixBuilder scheme")
		}

		k8sConfig, err := ctrl.GetConfig()
		if err != nil {
			log.Fatal().Err(err).Msg("Failed to get Kubernetes config")
		}

		mgr, err := ctrl.NewManager(k8sConfig, ctrl.Options{
			Scheme: scheme,
		})
		if err != nil {
			log.Fatal().Err(err).Msg("Failed to create controller manager")
		}

		reconciler := &controller.NixBuildRequestReconciler{
			Client:       mgr.GetClient(),
			Scheme:       mgr.GetScheme(),
			BuilderImage: builderImage,
			SSHPort:      sshPort,
			NixConfigMap: nixConfigMap,
		}

		if err := reconciler.SetupWithManager(mgr); err != nil {
			log.Fatal().Err(err).Msg("Failed to setup controller")
		}

		log.Info().
			Str("builder_image", builderImage).
			Int32("ssh_port", sshPort).
			Str("nix_config", nixConfigMap).
			Msg("Starting Nix remote builder controller")

		log.Info().Msg("Controller manager starting...")
		if err := mgr.Start(ctx); err != nil {
			if err == context.Canceled {
				log.Info().Msg("Controller manager stopped gracefully")
			} else {
				log.Fatal().Err(err).Msg("Controller manager failed")
			}
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
	rootCmd.Flags().StringVar(&builderImage, "builder-image", "nixos/nix:latest", "Builder container image")
	rootCmd.Flags().Int32Var(&sshPort, "ssh-port", 22, "SSH port in builder pods")
	rootCmd.Flags().StringVar(&nixConfigMap, "nix-config", "", "ConfigMap containing nix.conf (optional)")
	rootCmd.AddCommand(versionCmd)
}

func main() {
	if err := rootCmd.Execute(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
