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
            fromImage = pkgs.dockerTools.pullImage {
              imageName = "nixos/nix";
              imageDigest = "sha256:0e6ade350a4d86d76dd4046a654ccbbb58d14fe93b6e3deef42c1d0fd9db3849";
              sha256 = "sha256-zdGBgjbw+Z8iP5hu5oCkehO6L/VFlWmUiGsB4Y2z6i0=";
            };
            config = {
              Cmd = [ "sh" "-c" "nix-env -iA nixpkgs.openssh && adduser -D nixbld && ssh-keygen -A && exec sshd -D" ];
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
