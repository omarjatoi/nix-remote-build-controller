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
              Cmd = [ "${self.packages.${system}.controller}/bin/controller" ];
            };
          };

          proxy-image = pkgs.dockerTools.buildImage {
            name = "ghcr.io/omarjatoi/nix-remote-build-controller/proxy";
            tag = "latest";
            contents = [ self.packages.${system}.proxy ];
            config = {
              Cmd = [ "${self.packages.${system}.proxy}/bin/proxy" ];
            };
          };

          builder-image = pkgs.dockerTools.buildImage {
            name = "ghcr.io/omarjatoi/nix-remote-build-controller/builder";
            tag = "latest";
            contents = with pkgs; [
              nix
              openssh
              coreutils
              bash
            ];
            config = {
              Cmd = [
                "${pkgs.openssh}/bin/sshd"
                "-D"
                "-e"
              ];
              ExposedPorts = {
                "22/tcp" = { };
              };
            };
            runAsRoot = ''
              # Create nixbld user
              ${pkgs.shadow}/bin/useradd -m -s ${pkgs.bash}/bin/bash nixbld

              # Setup SSH
              mkdir -p /etc/ssh /home/nixbld/.ssh
              ${pkgs.openssh}/bin/ssh-keygen -A

              # Configure SSH daemon
              cat > /etc/ssh/sshd_config << 'EOF'
              Port 22
              PermitRootLogin no
              PasswordAuthentication no
              PubkeyAuthentication yes
              AuthorizedKeysFile .ssh/authorized_keys
              UsePAM no
              EOF

              # Setup Nix for nixbld user
              mkdir -p /nix/var/nix/profiles/per-user/nixbld
              chown nixbld:nixbld /nix/var/nix/profiles/per-user/nixbld
            '';
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
