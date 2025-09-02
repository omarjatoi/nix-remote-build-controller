package main

import (
	"context"
	"fmt"
	"net/http"
	"os"
	"os/signal"
	"sync/atomic"
	"syscall"
	"time"

	"github.com/omarjatoi/nix-remote-build-controller/pkg/apis/nixbuilder/v1alpha1"
	"github.com/omarjatoi/nix-remote-build-controller/pkg/controller"
	"github.com/rs/zerolog/log"
	"github.com/spf13/cobra"
	"k8s.io/apimachinery/pkg/runtime"
	clientgoscheme "k8s.io/client-go/kubernetes/scheme"
	ctrl "sigs.k8s.io/controller-runtime"
)

var (
	version         = "dev"
	builderImage    string
	remotePort      int32
	nixConfigMap    string
	healthPort      int
	shutdownTimeout time.Duration
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
			RemotePort:   remotePort,
			NixConfigMap: nixConfigMap,
		}

		if err := reconciler.SetupWithManager(mgr); err != nil {
			log.Fatal().Err(err).Msg("Failed to setup controller")
		}

		// Setup health checks
		var shuttingDown atomic.Bool
		if err := setupHealthChecks(mgr, &shuttingDown, healthPort); err != nil {
			log.Fatal().Err(err).Msg("Failed to setup health checks")
		}

		// Setup graceful shutdown handler
		go func() {
			<-ctx.Done() // Signal received

			// Mark as shutting down immediately
			shuttingDown.Store(true)
			log.Info().Dur("timeout", shutdownTimeout).Msg("Shutdown signal received, starting graceful shutdown")

			// Start resource cleanup
			cleanupCtx, cleanupCancel := context.WithTimeout(context.Background(), shutdownTimeout/2)
			defer cleanupCancel()

			if err := reconciler.GracefulShutdown(cleanupCtx); err != nil {
				log.Error().Err(err).Msg("Graceful shutdown cleanup failed")
			}

			// Give controller remaining time to finish
			time.Sleep(shutdownTimeout / 2)

			log.Fatal().Msg("Graceful shutdown timeout exceeded, forcing exit")
		}()

		log.Info().
			Str("builder_image", builderImage).
			Int32("remote_port", remotePort).
			Str("nix_config", nixConfigMap).
			Int("health_port", healthPort).
			Dur("shutdown_timeout", shutdownTimeout).
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

func setupHealthChecks(mgr ctrl.Manager, shuttingDown *atomic.Bool, port int) error {
	mux := http.NewServeMux()

	// Liveness probe - "is the process running?"
	mux.HandleFunc("/healthz", func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusOK)
		w.Write([]byte("ok"))
	})

	// Readiness probe - "can you handle new requests?"
	mux.HandleFunc("/readyz", func(w http.ResponseWriter, r *http.Request) {
		if shuttingDown.Load() {
			w.WriteHeader(http.StatusServiceUnavailable)
			w.Write([]byte("shutting down"))
			return
		}

		w.WriteHeader(http.StatusOK)
		w.Write([]byte("ready"))
	})

	server := &http.Server{
		Addr:    fmt.Sprintf(":%d", port),
		Handler: mux,
	}

	go func() {
		log.Info().Int("port", port).Msg("Health server starting")
		if err := server.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			log.Error().Err(err).Msg("Health server failed")
		}
	}()

	return nil
}

func init() {
	rootCmd.Flags().StringVar(&builderImage, "builder-image", "nixos/nix:latest", "Builder container image")
	rootCmd.Flags().Int32Var(&remotePort, "remote-port", 22, "SSH port in builder pods")
	rootCmd.Flags().StringVar(&nixConfigMap, "nix-config", "", "ConfigMap containing nix.conf (optional)")
	rootCmd.Flags().IntVar(&healthPort, "health-port", 8081, "Health check server port")
	rootCmd.Flags().DurationVar(&shutdownTimeout, "shutdown-timeout", 30*time.Second, "Graceful shutdown timeout")
	rootCmd.AddCommand(versionCmd)
}

func main() {
	if err := rootCmd.Execute(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
