package main

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/signal"
	"strings"
	"syscall"
	"time"

	"github.com/rs/zerolog"
	"github.com/rs/zerolog/log"
	"github.com/spf13/cobra"
	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/api/resource"
	"k8s.io/apimachinery/pkg/runtime"
	clientgoscheme "k8s.io/client-go/kubernetes/scheme"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/healthz"
	metricsserver "sigs.k8s.io/controller-runtime/pkg/metrics/server"

	"github.com/omarjatoi/nix-remote-build-controller/pkg/apis/nixbuilder/v1alpha1"
	"github.com/omarjatoi/nix-remote-build-controller/pkg/controller"
)

var version = "dev"

var (
	builderImage            string
	builderImagePullPolicy  string
	builderRequests         string
	builderLimits           string
	builderServiceAccount   string
	remotePort              int32
	nixConfigMap            string
	sshKeySecret            string
	namespace               string
	healthPort              int
	metricsAddr             string
	shutdownTimeout         time.Duration
	completedTTL            time.Duration
	maxConcurrentReconciles int
	leaderElect             bool
	logLevel                string
)

var rootCmd = &cobra.Command{
	Use:   "controller",
	Short: "Kubernetes controller for Nix remote builders",
	Long:  "A Kubernetes controller that turns NixBuildRequests into dedicated, short-lived Nix builder pods.",
	RunE: func(cmd *cobra.Command, args []string) error {
		level, err := zerolog.ParseLevel(logLevel)
		if err != nil {
			return fmt.Errorf("invalid --log-level %q: %w", logLevel, err)
		}
		zerolog.SetGlobalLevel(level)

		pullPolicy, err := parsePullPolicy(builderImagePullPolicy)
		if err != nil {
			return err
		}
		defaultResources, err := parseResources(builderRequests, builderLimits)
		if err != nil {
			return err
		}

		ctx, cancel := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
		defer cancel()

		scheme := runtime.NewScheme()
		if err := clientgoscheme.AddToScheme(scheme); err != nil {
			return fmt.Errorf("add client-go scheme: %w", err)
		}
		if err := v1alpha1.AddToScheme(scheme); err != nil {
			return fmt.Errorf("add NixBuilder scheme: %w", err)
		}

		k8sConfig, err := ctrl.GetConfig()
		if err != nil {
			return fmt.Errorf("get Kubernetes config: %w", err)
		}

		opts := ctrl.Options{
			Scheme:                  scheme,
			HealthProbeBindAddress:  fmt.Sprintf(":%d", healthPort),
			Metrics:                 metricsserver.Options{BindAddress: metricsAddr},
			GracefulShutdownTimeout: &shutdownTimeout,
			LeaderElection:          leaderElect,
			LeaderElectionID:        "nix-remote-build-controller.nix.io",
		}
		if namespace != "" {
			opts.LeaderElectionNamespace = namespace
		}
		mgr, err := ctrl.NewManager(k8sConfig, opts)
		if err != nil {
			return fmt.Errorf("create controller manager: %w", err)
		}

		// Check the secret up front so misconfiguration fails fast, and detect
		// whether a shared builder host key should be mounted.
		mountHostKey, err := secretHasBuilderHostKey(ctx, mgr.GetAPIReader(), namespace, sshKeySecret)
		if err != nil {
			return err
		}

		reconciler := &controller.NixBuildRequestReconciler{
			Client:                  mgr.GetClient(),
			Scheme:                  mgr.GetScheme(),
			Recorder:                mgr.GetEventRecorder("nix-remote-build-controller"),
			BuilderImage:            builderImage,
			BuilderImagePullPolicy:  pullPolicy,
			RemotePort:              remotePort,
			NixConfigMap:            nixConfigMap,
			SSHKeySecret:            sshKeySecret,
			MountBuilderHostKey:     mountHostKey,
			DefaultResources:        defaultResources,
			CompletedTTL:            completedTTL,
			MaxConcurrentReconciles: maxConcurrentReconciles,
			BuilderServiceAccount:   builderServiceAccount,
		}
		if err := reconciler.SetupWithManager(mgr); err != nil {
			return fmt.Errorf("setup controller: %w", err)
		}
		if err := mgr.AddHealthzCheck("healthz", healthz.Ping); err != nil {
			return fmt.Errorf("add healthz check: %w", err)
		}
		if err := mgr.AddReadyzCheck("readyz", healthz.Ping); err != nil {
			return fmt.Errorf("add readyz check: %w", err)
		}

		log.Info().
			Str("version", version).
			Str("builder_image", builderImage).
			Str("builder_image_pull_policy", string(pullPolicy)).
			Int32("remote_port", remotePort).
			Str("nix_config", nixConfigMap).
			Str("ssh_key_secret", sshKeySecret).
			Bool("mount_builder_host_key", mountHostKey).
			Int("max_concurrent_reconciles", maxConcurrentReconciles).
			Dur("completed_ttl", completedTTL).
			Dur("shutdown_timeout", shutdownTimeout).
			Bool("leader_election", leaderElect).
			Msg("Starting Nix remote builder controller")

		// mgr.Start blocks until ctx is cancelled and then waits up to
		// GracefulShutdownTimeout for in-flight reconciles. In-flight
		// NixBuildRequests are left untouched: their state is persisted and the
		// next controller instance resumes them.
		if err := mgr.Start(ctx); err != nil {
			return fmt.Errorf("controller manager failed: %w", err)
		}
		log.Info().Msg("Controller manager stopped")
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

func parsePullPolicy(s string) (corev1.PullPolicy, error) {
	switch corev1.PullPolicy(s) {
	case "", corev1.PullAlways, corev1.PullIfNotPresent, corev1.PullNever:
		return corev1.PullPolicy(s), nil
	default:
		return "", fmt.Errorf("invalid --builder-image-pull-policy %q (want Always, IfNotPresent, Never or empty)", s)
	}
}

// parseResources turns "cpu=1,memory=2Gi" style strings into resource requirements.
func parseResources(requests, limits string) (corev1.ResourceRequirements, error) {
	var rr corev1.ResourceRequirements
	var err error
	if rr.Requests, err = parseResourceList(requests); err != nil {
		return rr, fmt.Errorf("invalid --builder-requests: %w", err)
	}
	if rr.Limits, err = parseResourceList(limits); err != nil {
		return rr, fmt.Errorf("invalid --builder-limits: %w", err)
	}
	return rr, nil
}

func parseResourceList(s string) (corev1.ResourceList, error) {
	s = strings.TrimSpace(s)
	if s == "" {
		return nil, nil
	}
	list := corev1.ResourceList{}
	for _, kv := range strings.Split(s, ",") {
		name, value, ok := strings.Cut(strings.TrimSpace(kv), "=")
		if !ok || name == "" {
			return nil, fmt.Errorf("expected name=quantity, got %q", kv)
		}
		q, err := resource.ParseQuantity(strings.TrimSpace(value))
		if err != nil {
			return nil, fmt.Errorf("%s: %w", name, err)
		}
		list[corev1.ResourceName(strings.TrimSpace(name))] = q
	}
	return list, nil
}

func secretHasBuilderHostKey(ctx context.Context, reader client.Reader, ns, name string) (bool, error) {
	if ns == "" {
		ns = "default"
	}
	var secret corev1.Secret
	if err := reader.Get(ctx, client.ObjectKey{Namespace: ns, Name: name}, &secret); err != nil {
		return false, fmt.Errorf("read SSH key secret %s/%s: %w", ns, name, err)
	}
	if len(secret.Data[controller.AuthorizedKeysSecretKey]) == 0 {
		return false, errors.New("SSH key secret " + name + " is missing the 'public' entry (builder authorized_keys)")
	}
	has := len(secret.Data[controller.BuilderHostKeySecretKey]) > 0
	if !has {
		log.Warn().Msgf("Secret %s has no %q entry; builder pods will generate a fresh host key on every start and the proxy cannot verify them", name, controller.BuilderHostKeySecretKey)
	}
	return has, nil
}

func init() {
	f := rootCmd.Flags()
	f.StringVar(&builderImage, "builder-image", "", "Builder container image (required)")
	_ = rootCmd.MarkFlagRequired("builder-image")
	f.StringVar(&builderImagePullPolicy, "builder-image-pull-policy", "", "Image pull policy for builder pods: Always, IfNotPresent or Never (default: Kubernetes default)")
	f.StringVar(&builderRequests, "builder-requests", "", "Default resource requests for builder pods, e.g. cpu=1,memory=2Gi")
	f.StringVar(&builderLimits, "builder-limits", "", "Default resource limits for builder pods, e.g. cpu=4,memory=8Gi")
	f.StringVar(&builderServiceAccount, "builder-service-account", "", "Service account for builder pods (default: the namespace default)")
	f.Int32Var(&remotePort, "remote-port", 22, "SSH port in builder pods")
	f.StringVar(&nixConfigMap, "nix-config", "", "ConfigMap containing nix.conf, mounted at /etc/nix in builder pods (optional)")
	f.StringVar(&sshKeySecret, "ssh-key-secret", "nix-builder-ssh-keys", "Secret with the SSH keypair for builder authentication (keys 'private' and 'public'; optional 'builder-host-key')")
	f.StringVarP(&namespace, "namespace", "n", "default", "Namespace of the SSH key secret and leader election lease")
	f.IntVar(&healthPort, "health-port", 8081, "Health check server port")
	f.StringVar(&metricsAddr, "metrics-addr", "0", "Address for the Prometheus metrics endpoint (\"0\" disables it)")
	f.DurationVar(&shutdownTimeout, "shutdown-timeout", 30*time.Second, "How long to wait for in-flight reconciles on shutdown")
	f.DurationVar(&completedTTL, "completed-ttl", controller.DefaultCompletedTTL, "How long Completed/Failed NixBuildRequests are kept before the controller deletes them")
	f.IntVar(&maxConcurrentReconciles, "max-concurrent-reconciles", controller.DefaultMaxConcurrentReconciles, "Number of NixBuildRequests reconciled concurrently")
	f.BoolVar(&leaderElect, "leader-elect", false, "Enable leader election (required when running more than one replica)")
	f.StringVar(&logLevel, "log-level", "info", "Log level (trace, debug, info, warn, error)")
	rootCmd.AddCommand(versionCmd)
}

func main() {
	if err := rootCmd.Execute(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
