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
            vendorHash = "sha256-Ua6i6574AG84UsyAIj/KL5yc0+4BVVy1eR+N98qpUkQ=";
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
            set -e

            # Create necessary directories
            mkdir -p /etc/ssh /var/empty /root/.ssh /tmp /run/sshd

            # Set up system users (including nix build users)
            echo 'root:x:0:0:root:/root:/bin/sh' > /etc/passwd
            echo 'sshd:x:999:999:SSH Daemon:/var/empty:/bin/false' >> /etc/passwd
            for i in $(seq 1 32); do
              echo "nixbld$i:x:$((30000 + i)):30000:Nix Build User $i:/var/empty:/bin/false" >> /etc/passwd
            done
            echo 'root:x:0:' > /etc/group
            echo 'sshd:x:999:' >> /etc/group
            MEMBERS=""
            for i in $(seq 1 32); do
              [ -n "$MEMBERS" ] && MEMBERS="$MEMBERS,"
              MEMBERS="''${MEMBERS}nixbld$i"
            done
            echo "nixbld:x:30000:$MEMBERS" >> /etc/group

            # Set up nix store state directories
            mkdir -p /nix/var/nix/db /nix/var/nix/gcroots /nix/var/nix/profiles /nix/var/nix/temproots /nix/var/nix/daemon-socket

            # Generate host key if needed
            if [ ! -f /etc/ssh/ssh_host_ed25519_key ]; then
              ${pkgs.openssh}/bin/ssh-keygen -t ed25519 -f /etc/ssh/ssh_host_ed25519_key -N ""
            fi

            # Copy authorized_keys from mounted secret (which is read-only)
            # to a writable location
            if [ -f /root/.ssh/authorized_keys ]; then
              cp /root/.ssh/authorized_keys /tmp/authorized_keys
              chmod 600 /tmp/authorized_keys
            fi

            # Set up SSH config - root login with restricted commands
            cat > /etc/ssh/sshd_config <<SSHD_CONFIG
            HostKey /etc/ssh/ssh_host_ed25519_key
            AuthorizedKeysFile /tmp/authorized_keys
            PasswordAuthentication no
            PermitRootLogin prohibit-password
            AllowUsers root
            StrictModes no
            SSHD_CONFIG

            # Start nix-daemon in the background
            ${pkgs.nix}/bin/nix-daemon &
            sleep 1

            # Start SSHD
            exec ${pkgs.openssh}/bin/sshd -D -e
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
            mkdir -p $out/root/.ssh $out/home/nixbld $out/tmp $out/var/empty
          '';

          builder-image = pkgs.dockerTools.buildImage {
            name = "ghcr.io/omarjatoi/nix-remote-build-controller/builder";
            tag = "latest";
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
              pathsToLink = [ "/bin" "/etc" "/share" "/root" "/home" "/tmp" "/var" ];
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
