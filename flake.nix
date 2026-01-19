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
      in
      {
        packages = {
          controller = pkgs.buildGoModule {
            pname = "controller";
            inherit version;
            src = ./.;
            vendorHash = "sha256-Ua6i6574AG84UsyAIj/KL5yc0+4BVVy1eR+N98qpUkQ=";
            subPackages = [ "cmd/controller" ];
            ldflags = [
              "-s"
              "-w"
              "-X main.version=${version}"
            ];
          };

          proxy = pkgs.buildGoModule {
            pname = "proxy";
            inherit version;
            src = ./.;
            vendorHash = "sha256-Ua6i6574AG84UsyAIj/KL5yc0+4BVVy1eR+N98qpUkQ=";
            subPackages = [ "cmd/proxy" ];
            ldflags = [
              "-s"
              "-w"
              "-X main.version=${version}"
            ];
          };

          controller-image = pkgs.dockerTools.buildImage {
            name = "ghcr.io/omarjatoi/nix-remote-build-controller/controller";
            tag = "latest";
            contents = [ self.packages.${system}.controller ];
            config = {
              Entrypoint = [ "${self.packages.${system}.controller}/bin/controller" ];
            };
          };

          proxy-image = pkgs.dockerTools.buildImage {
            name = "ghcr.io/omarjatoi/nix-remote-build-controller/proxy";
            tag = "latest";
            contents = [ self.packages.${system}.proxy ];
            config = {
              Entrypoint = [ "${self.packages.${system}.proxy}/bin/proxy" ];
            };
          };

          builder-image = pkgs.dockerTools.buildImage {
            name = "ghcr.io/omarjatoi/nix-remote-build-controller/builder";
            tag = "latest";
            fromImage = pkgs.dockerTools.pullImage {
              imageName = "nixos/nix";
              imageDigest = "sha256:0e6ade350a4d86d76dd4046a654ccbbb58d14fe93b6e3deef42c1d0fd9db3849";
              sha256 = "sha256-zdGBgjbw+Z8iP5hu5oCkehO6L/VFlWmUiGsB4Y2z6i0=";
            };
            contents = [
              pkgs.openssh
              pkgs.shadow
            ];
            runAsRoot = ''
              #!${pkgs.runtimeShell}
              mkdir -p /etc/ssh /var/empty
              ${pkgs.shadow}/bin/useradd -m -s /bin/sh nixbld
              mkdir -p /home/nixbld/.ssh
              chown nixbld:nixbld /home/nixbld/.ssh
              chmod 700 /home/nixbld/.ssh
              ${pkgs.openssh}/bin/ssh-keygen -t ed25519 -f /etc/ssh/ssh_host_ed25519_key -N ""

              cat > /etc/ssh/sshd_config <<SSHD_CONFIG
              HostKey /etc/ssh/ssh_host_ed25519_key
              AuthorizedKeysFile /home/nixbld/.ssh/authorized_keys
              PasswordAuthentication no
              AllowUsers nixbld
              SSHD_CONFIG

              # Create entrypoint script
              cat > /bin/entrypoint.sh <<EOF
              #!${pkgs.runtimeShell}
              # Start nix-daemon in the background
              ${pkgs.nix}/bin/nix-daemon &

              # Wait for daemon to be ready
              sleep 1

              # Start SSHD
              exec ${pkgs.openssh}/bin/sshd -D -e
              EOF
              chmod +x /bin/entrypoint.sh

              # Ensure nix binaries are in path
              ln -sf ${pkgs.nix}/bin/nix* /bin/
            '';
            config = {
              Cmd = [ "/bin/entrypoint.sh" ];
              Env = [ "PATH=/bin:/usr/bin" ];
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
            nixfmt-rfc-style
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
