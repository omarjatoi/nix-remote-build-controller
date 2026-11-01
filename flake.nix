{
  description = "Kubernetes controller for dynamically scaling Nix remote builders";

  inputs = {
    nixpkgs.url = "https://flakehub.com/f/NixOS/nixpkgs/0.1";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "https://flakehub.com/f/ipetkov/crane/*";
    # Builder pods run Determinate Nix, which exports OpenTelemetry traces. It
    # keeps its own nixpkgs, so that its builds stay in the binary cache.
    determinate-nix.url = "https://flakehub.com/f/DeterminateSystems/nix-src/*";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      crane,
      determinate-nix,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        lib = pkgs.lib;
        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
        version = cargoToml.workspace.package.version;

        # Only the Rust sources affect the build; manifests and docs do not.
        rustSrc = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./clippy.toml
            ./crates
          ];
        };

        # The chart's CRD is a symbolic link to deploy/crd.yaml.
        chartSrc = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./charts
            ./deploy/crd.yaml
          ];
        };

        craneLib = crane.mkLib pkgs;

        crateArgs = {
          pname = "nix-remote-build-controller";
          inherit version;
          src = rustSrc;
          strictDeps = true;
        };

        # Dependencies are their own derivations, so a change to the workspace's
        # code only rebuilds its crates. Tests and lints use the dev profile,
        # which has debug assertions, and so need their own build.
        releaseArtifacts = craneLib.buildDepsOnly crateArgs;
        devArgs = crateArgs // {
          CARGO_PROFILE = "dev";
        };
        devArtifacts = craneLib.buildDepsOnly devArgs;

        # Build one of the workspace's programs (`controller` or `proxy`).
        buildRustApp =
          name:
          craneLib.buildPackage (
            crateArgs
            // {
              pname = name;
              cargoArtifacts = releaseArtifacts;
              cargoExtraArgs = "--locked --package ${name}";
              # The test suite runs once, in `checks`, rather than per binary.
              doCheck = false;
              meta.mainProgram = name;
            }
          );

        # Build a container image for a given app
        buildImage =
          imgPkgs: name: app:
          imgPkgs.dockerTools.buildImage {
            name = "ghcr.io/omarjatoi/nix-remote-build-controller/${name}";
            tag = "latest";
            copyToRoot = pkgs.buildEnv {
              name = name;
              paths = [ app ];
              pathsToLink = [ "/bin" ];
            };
            config = {
              Entrypoint = [ "${app}/bin/${name}" ];
              # For exporting telemetry to an HTTPS endpoint.
              Env = [ "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt" ];
            };
          };
      in
      {
        packages = {
          # Native binaries for local development
          controller = buildRustApp "controller";
          proxy = buildRustApp "proxy";

          # Container images (uses current system's pkgs - works on Linux runners)
          controller-image = buildImage pkgs "controller" self.packages.${system}.controller;
          proxy-image = buildImage pkgs "proxy" self.packages.${system}.proxy;

          # Entrypoint script for builder container - runs setup at container start
          builder-entrypoint = pkgs.writeShellScriptBin "entrypoint" ''
            set -euo pipefail
            umask 022

            # Create necessary directories
            mkdir -p /etc/ssh /var/empty /home/nixbld/.ssh /home/nixbld/.cache/nix /tmp /run/sshd

            # Set up system users
            echo 'root:x:0:0:root:/root:/bin/sh' > /etc/passwd
            echo 'sshd:x:999:999:SSH Daemon:/var/empty:/bin/false' >> /etc/passwd
            echo 'nixbld:x:1000:1000:Nix Build User:/home/nixbld:/bin/sh' >> /etc/passwd
            echo 'root:x:0:' > /etc/group
            echo 'sshd:x:999:' >> /etc/group
            echo 'nixbld:x:1000:nixbld' >> /etc/group

            # Give nixbld ownership of the nix store (single-user mode).
            mkdir -p /nix/var/nix/db /nix/var/nix/gcroots /nix/var/nix/profiles /nix/var/nix/temproots
            chmod 1775 /nix/store
            chown nixbld:nixbld /nix/store
            chown -R nixbld:nixbld /nix/var
            # chown home without -R to avoid the read-only secret mount at .ssh/authorized_keys
            chown nixbld:nixbld /home/nixbld
            chown -R nixbld:nixbld /home/nixbld/.cache

            # Host key: prefer one mounted read-only by the controller (so the proxy
            # can verify it and clients see a stable key across pods); otherwise
            # generate an ephemeral one. A read-only mount cannot be used directly by
            # sshd, so copy it to a writable path.
            HOST_KEY=/etc/ssh/ssh_host_ed25519_key
            if [ -s "$HOST_KEY" ]; then
              cp "$HOST_KEY" /tmp/ssh_host_ed25519_key
              chmod 600 /tmp/ssh_host_ed25519_key
              ${pkgs.openssh}/bin/ssh-keygen -y -f /tmp/ssh_host_ed25519_key > /tmp/ssh_host_ed25519_key.pub
              HOST_KEY=/tmp/ssh_host_ed25519_key
              echo "builder: using mounted host key ($(${pkgs.openssh}/bin/ssh-keygen -lf /tmp/ssh_host_ed25519_key.pub))"
            else
              HOST_KEY=/tmp/ssh_host_ed25519_key
              ${pkgs.openssh}/bin/ssh-keygen -t ed25519 -f "$HOST_KEY" -N ""
              echo "builder: generated ephemeral host key"
            fi

            # Copy authorized_keys from mounted secret (read-only) to a writable location
            if [ -s /home/nixbld/.ssh/authorized_keys ]; then
              cp /home/nixbld/.ssh/authorized_keys /tmp/authorized_keys
              chmod 600 /tmp/authorized_keys
              chown nixbld:nixbld /tmp/authorized_keys
            else
              echo "builder: FATAL: no authorized_keys mounted at /home/nixbld/.ssh/authorized_keys" >&2
              exit 1
            fi

            # Set up SSH config
            cat > /etc/ssh/sshd_config <<SSHD_CONFIG
            HostKey $HOST_KEY
            AuthorizedKeysFile /tmp/authorized_keys
            PasswordAuthentication no
            PubkeyAuthentication yes
            PermitRootLogin no
            AllowUsers nixbld
            AcceptEnv PATH TRACEPARENT
            ClientAliveInterval 30
            ClientAliveCountMax 4
            StrictModes no
            SSHD_CONFIG

            # Start SSHD in the foreground; -e logs to stderr so kubectl logs works.
            exec ${pkgs.openssh}/bin/sshd -D -e -f /etc/ssh/sshd_config
          '';

          # Base system files for the builder container
          builder-etc = pkgs.runCommand "builder-etc" { } ''
            mkdir -p $out/etc
            echo "root:x:0:0:root:/root:/bin/sh" > $out/etc/passwd
            echo "sshd:x:999:999:SSH Daemon:/var/empty:/bin/false" >> $out/etc/passwd
            echo "nixbld:x:1000:1000:Nix Build User:/home/nixbld:/bin/sh" >> $out/etc/passwd
            echo "root:x:0:" > $out/etc/group
            echo "sshd:x:999:" >> $out/etc/group
            echo "nixbld:x:1000:" >> $out/etc/group
            mkdir -p $out/home/nixbld/.ssh $out/tmp $out/var/empty
            # Where Nix looks for CA certificates when NIX_SSL_CERT_FILE is
            # unset, as it is in SSH sessions: sshd does not pass on the image's
            # environment.
            mkdir -p $out/etc/ssl/certs
            ln -s ${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt $out/etc/ssl/certs/ca-certificates.crt
          '';

          builder-image = pkgs.dockerTools.buildImage {
            name = "ghcr.io/omarjatoi/nix-remote-build-controller/builder";
            tag = "latest";
            includeNixDB = true;
            copyToRoot = pkgs.buildEnv {
              name = "builder-root";
              paths = [
                determinate-nix.packages.${system}.nix-cli
                pkgs.openssh
                pkgs.coreutils
                pkgs.bashInteractive
                self.packages.${system}.builder-entrypoint
                self.packages.${system}.builder-etc
              ];
              pathsToLink = [
                "/bin"
                "/etc"
                "/share"
                "/root"
                "/home"
                "/tmp"
                "/var"
              ];
            };
            config = {
              Entrypoint = [ "${self.packages.${system}.builder-entrypoint}/bin/entrypoint" ];
              Env = [ "PATH=/bin" ];
              ExposedPorts = {
                "22/tcp" = { };
              };
            };
          };

          default = self.packages.${system}.controller;
        };

        # `nix flake check` runs formatting, lints and the full test suite.
        checks = {
          fmt = craneLib.cargoFmt { inherit (crateArgs) pname version src; };
          clippy = craneLib.cargoClippy (
            devArgs
            // {
              cargoArtifacts = devArtifacts;
              cargoClippyExtraArgs = "--all-targets -- -D warnings";
            }
          );
          tests = craneLib.cargoTest (
            devArgs
            // {
              cargoArtifacts = devArtifacts;
              # The OTLP exporter needs a CA bundle, which the images have too.
              preBuild = "export SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
            }
          );
          # Linting renders the templates, with both sets of values.
          helm-chart =
            pkgs.runCommand "helm-chart"
              {
                nativeBuildInputs = [ pkgs.kubernetes-helm ];
              }
              ''
                export HOME=$TMPDIR
                chart=${chartSrc}/charts/nix-remote-build-controller
                helm lint --strict "$chart"
                helm lint --strict "$chart" --values "$chart/ci/all-values.yaml"
                touch $out
              '';
        };

        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            nixfmt
            # End-to-end and manifest tooling.
            kind
            kubectl
            kustomize
            kubeconform
            kubernetes-helm
            shellcheck
            jq
          ];
        };

        apps = {
          controller = flake-utils.lib.mkApp {
            drv = self.packages.${system}.controller;
          };
          proxy = flake-utils.lib.mkApp {
            drv = self.packages.${system}.proxy;
          };
          default = self.apps.${system}.controller;
        };
      }
    );
}
