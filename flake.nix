{
  description = "Pong 2D P2P — juego + gateway";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
  };

  outputs = { self, nixpkgs }: let
    pkgs = nixpkgs.legacyPackages."x86_64-linux";

    # Toolchain musl (x86_64-unknown-linux-musl) para binarios Linux 100% estaticos.
    musl = pkgs.pkgsCross.musl64;
    static = musl.pkgsStatic;
    muslRust = musl.rustPlatform;
    staticFlags = "-C target-feature=+crt-static";

  in {
    # ── Development shell (unchanged) ────────────────────────────────
    devShells."x86_64-linux".default = pkgs.mkShell {
      buildInputs = with pkgs; [
       vulkan-loader cargo rustc rustfmt clippy rust-analyzer glib wayland-protocols wayland alsa-lib libudev-zero
       libxkbcommon libx11 libxcursor libxrandr libxi libxcb
      ];

      LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (with pkgs; [ wayland libxkbcommon vulkan-loader libx11 libxcursor libxrandr libxi libxcb ]);
      shellHook = ''
	      export LD_LIBRARY_PATH=${pkgs.wayland}/lib:$LD_LIBRARY_PATH
	'';
      nativeBuildInputs = [ pkgs.pkg-config ];

      env.RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
    };

    # ── Gateway binary (headless, musl estatico) ────────────────────
    packages."x86_64-linux".pong-gateway = muslRust.buildRustPackage {
      pname = "pong-gateway";
      version = "0.1.0";
      src = ./.;
      cargoLock.lockFile = ./Cargo.lock;
      nativeBuildInputs = [static.pkg-config];
      buildInputs = [static.wayland static.libudev-zero];
      RUSTFLAGS = staticFlags;
      cargoBuildFlags = ["--bin" "pong-gateway"];
    };

    # ── Game client binary (Bevy + Vulkan, musl estatico) ──────────
    packages."x86_64-linux".pong-client = (muslRust.buildRustPackage {
      pname = "pong-client";
      version = "0.1.0";
      src = ./.;
      cargoLock.lockFile = ./Cargo.lock;
      nativeBuildInputs = [static.pkg-config];
      RUSTFLAGS = staticFlags;
      cargoBuildFlags = ["--bin" "ponged"];
    }).overrideAttrs (old: {
      buildInputs = (old.buildInputs or []) ++ (with static; [
        libudev-zero
        wayland
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

    # ── systemd unit (works en CUALQUIER distro con systemd) ────────
    # Genera /etc/systemd/system/pong-gateway.service a partir del
    # binario estatico. La IP publica se lee de /etc/pong-gateway.env
    # (PONG_PUBLIC=...), asi no hay que regenerar nada si cambia la EIP.
    packages."x86_64-linux".systemdUnit = pkgs.runCommand "pong-gateway-systemd-unit" {} ''
      mkdir -p $out
      cat > $out/pong-gateway.service <<'EOF'
      [Unit]
      Description=Pong P2P Matchmaking Gateway
      After=network.target

      [Service]
      EnvironmentFile=/etc/pong-gateway.env
      ExecStart=/usr/local/bin/pong-gateway --listen /ip4/0.0.0.0/tcp/4001 --public ''${PONG_PUBLIC}
      WorkingDirectory=/var/lib/pong-gateway
      StateDirectory=pong-gateway
      Restart=on-failure
      RestartSec=5

      [Install]
      WantedBy=multi-user.target
      EOF
    '';

    # Scrip que copia binario + unit y activa el servicio (distro-agnostico).
    packages."x86_64-linux".installSystemd = pkgs.writeShellScriptBin "install-pong-gateway" ''
      set -euo pipefail
      BIN=${self.packages."x86_64-linux".pong-gateway}/bin/pong-gateway
      UNIT=${self.packages."x86_64-linux".systemdUnit}/pong-gateway.service

      install -m 0755 "$BIN" /usr/local/bin/pong-gateway
      install -m 0644 "$UNIT" /etc/systemd/system/pong-gateway.service

      if [ ! -e /etc/pong-gateway.env ]; then
        echo "PONG_PUBLIC=<EIP_ELASTICA>" > /etc/pong-gateway.env
        echo "OJO: edita /etc/pong-gateway.env con la IP publica real."
      fi

      systemctl daemon-reload
      systemctl enable --now pong-gateway
    '';

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
            WorkingDirectory = "/var/lib/pong-gateway";
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
