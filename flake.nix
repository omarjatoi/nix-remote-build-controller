{
  description = "Kubernetes controller for dynamically scaling Nix remote builders";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        version = "0.1.0";

        # Build a Go binary for a given pkgs set
        buildGoApp =
          goPkgs: name:
          goPkgs.buildGoModule {
            pname = name;
            inherit version;
            src = ./.;
            vendorHash = "sha256-e4dLY+rLKK2VxINuLQNADEK/dCTVGvd2OtRh7aUYycY=";
            subPackages = [ "cmd/${name}" ];
            ldflags = [
              "-s"
              "-w"
              "-X main.version=${version}"
            ];
          };

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
            };
          };
      in
      {
        packages = {
          # Native binaries for local development
          controller = buildGoApp pkgs "controller";
          proxy = buildGoApp pkgs "proxy";

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
            AcceptEnv PATH
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
          '';

          builder-image = pkgs.dockerTools.buildImage {
            name = "ghcr.io/omarjatoi/nix-remote-build-controller/builder";
            tag = "latest";
            includeNixDB = true;
            copyToRoot = pkgs.buildEnv {
              name = "builder-root";
              paths = [
                pkgs.nix
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
              Env = [
                "PATH=/bin"
                "NIX_SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              ];
              ExposedPorts = {
                "22/tcp" = { };
              };
            };
          };

          default = self.packages.${system}.controller;
        };

        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            go
            golangci-lint
            nixfmt
            # End-to-end and manifest tooling.
            kind
            kubectl
            kustomize
            kubeconform
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
