{
  description = "Pong 2D P2P — juego + gateway";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
  };

  outputs = { self, nixpkgs }: let
    pkgs = nixpkgs.legacyPackages."x86_64-linux";

  in {
    # ── Development shell (unchanged) ────────────────────────────────
    devShells."x86_64-linux".default = pkgs.mkShell {
      buildInputs = with pkgs; [
       vulkan-loader cargo rustc rustfmt clippy rust-analyzer glib wayland-protocols wayland alsa-lib libudev-zero
      ];

      LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (with pkgs; [ wayland libxkbcommon vulkan-loader ]);
      shellHook = ''
	      export LD_LIBRARY_PATH=${pkgs.wayland}/lib:$LD_LIBRARY_PATH
	'';
      nativeBuildInputs = [ pkgs.pkg-config ];

      env.RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
    };

    # ── Gateway binary (headless, no Bevy deps) ─────────────────────
    packages."x86_64-linux".pong-gateway = pkgs.rustPlatform.buildRustPackage {
      pname = "pong-gateway";
      version = "0.1.0";
      src = ./.;
      cargoLock.lockFile = ./Cargo.lock;
      nativeBuildInputs = [pkgs.pkg-config];
      buildInputs = [pkgs.openssl pkgs.wayland pkgs.libudev-zero];
      cargoBuildFlags = ["--bin" "pong-gateway"];
    };

    # ── Game client binary (Bevy + Vulkan/Wayland) ──────────────────
    packages."x86_64-linux".pong-client = (pkgs.rustPlatform.buildRustPackage {
      pname = "pong-client";
      version = "0.1.0";
      src = ./.;
      cargoLock.lockFile = ./Cargo.lock;
      nativeBuildInputs = [pkgs.pkg-config];
      cargoBuildFlags = ["--bin" "Proyecto-Final"];
    }).overrideAttrs (old: {
      buildInputs = (old.buildInputs or []) ++ (with pkgs; [
        vulkan-loader wayland libxkbcommon
        libx11 libxcursor libxrandr libxi
        alsa-lib libudev-zero fontconfig freetype
      ]);
    });

    # ── Docker image (gateway only, minimal) ────────────────────────
    packages."x86_64-linux".dockerImage = pkgs.dockerTools.buildLayeredImage {
      name = "pong-gateway";
      tag = "latest";
      contents = [self.packages."x86_64-linux".pong-gateway];
      config = {
        Cmd = ["pong-gateway" "--listen" "/ip4/0.0.0.0/tcp/4001"];
        ExposedPorts = {"4001/tcp" = {};};
      };
    };

    # ── NixOS module (systemd service) ──────────────────────────────
    nixosModules.pong-gateway = { config, lib, pkgs, ... }: let
      cfg = config.services.pong-gateway;
    in {
      options.services.pong-gateway = {
        enable = lib.mkEnableOption "Pong P2P matchmaking gateway";
        package = lib.mkPackageOption pkgs "pong-gateway" {};
        listen = lib.mkOption {
          type = lib.types.str;
          default = "/ip4/0.0.0.0/tcp/4001";
          description = "Multiaddr to listen on";
        };
        public = lib.mkOption {
          type = lib.types.str;
          default = "127.0.0.1";
          description = "Public host/IP for relay addresses";
        };
        openFirewall = lib.mkEnableOption "open firewall port 4001";
      };

      config = lib.mkIf cfg.enable {
        systemd.services.pong-gateway = {
          description = "Pong P2P Matchmaking Gateway";
          after = ["network.target"];
          wantedBy = ["multi-user.target"];
          serviceConfig = {
            ExecStart = lib.concatStringsSep " " [
              "${cfg.package}/bin/pong-gateway"
              "--listen" cfg.listen
              "--public" cfg.public
            ];
            StateDirectory = "pong-gateway";
            DynamicUser = true;
            Restart = "on-failure";
            RestartSec = 5;
          };
        };
        networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall [4001];
      };
    };
  };
}
