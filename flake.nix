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

    # El cliente se enlaza DINAMICAMENTE (glibc). Motivo: winit 0.30
    # carga libxkbcommon y libxcb en runtime via `dlopen`, que un binario
    # musl estatico no soporta (panico XKBNotFound). Los binarios dynamicos
    # de NixOS llevan los RPATH/closure en el store, no se instala nada a mano.
    # Los end-users de OTRAS distros necesitarián esas libs de sistema,
    # pero sqlcipher/openssl NO: rusqlite usa `bundled-sqlcipher-vendored-openssl`
    # (ver Cargo.toml), ambos compilados desde fuente dentro del binario.
    clientLibs = with pkgs; [
      vulkan-loader wayland wayland-protocols libxkbcommon
      alsa-lib libudev-zero glib
      libx11 libxcursor libxrandr libxi libxcb
      openssl
    ];

  in {
    # ── Development shell ───────────────────────────────────────────
    # Shell ligero de desarrollo: NO incluye LLVM (pesado) ni cargo-llvm-cov.
    devShells."x86_64-linux".default = pkgs.mkShell {
      buildInputs = clientLibs ++ (with pkgs; [
        cargo rustc rustfmt clippy rust-analyzer
      ]);

      LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (with pkgs; [ wayland libxkbcommon vulkan-loader libx11 libxcursor libxrandr libxi libxcb ]);
      shellHook = ''
	      export LD_LIBRARY_PATH=${pkgs.wayland}/lib:$LD_LIBRARY_PATH
	  '';
      nativeBuildInputs = [pkgs.pkg-config pkgs.perl];

      env.RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
    };

    # ── Coverage shell (solo para el pipeline de SonarQube) ─────────
    # Igual que el default + cargo-llvm-cov y los llvm-tools que emparejan con
    # la rustc del flake. Se entra con `nix develop .#coverage`: no engorda el
    # devShell de uso diario. Uso:
    #   nix develop .#coverage -c cargo llvm-cov --lcov --output-path target/lcov.info
    devShells."x86_64-linux".coverage = pkgs.mkShell {
      buildInputs = clientLibs ++ (with pkgs; [
        cargo rustc rustfmt clippy rust-analyzer cargo-llvm-cov
        # llvm-tools del mismo toolchain que la rustc del flake. Es buildInput
        # (no solo env) para que `nix develop .#coverage` lo descargue.
        pkgs.rustc.llvmPackages.llvm
      ]);

      LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (with pkgs; [ wayland libxkbcommon vulkan-loader libx11 libxcursor libxrandr libxi libxcb ]);
      shellHook = ''
	      export LD_LIBRARY_PATH=${pkgs.wayland}/lib:$LD_LIBRARY_PATH
	  '';
      nativeBuildInputs = [pkgs.pkg-config pkgs.perl];

      env.RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
      env.LLVM_COV = "${pkgs.rustc.llvmPackages.llvm}/bin/llvm-cov";
      env.LLVM_PROFDATA = "${pkgs.rustc.llvmPackages.llvm}/bin/llvm-profdata";
    };

    # ── Gateway binary (headless, musl estatico) ────────────────────
    packages."x86_64-linux".ponged-gateway = muslRust.buildRustPackage {
      pname = "ponged-gateway";
      version = "0.1.0";
      src = ./.;
      cargoLock.lockFile = ./Cargo.lock;
      nativeBuildInputs = [static.pkg-config pkgs.perl];
      buildInputs = [static.wayland static.libudev-zero static.openssl];
      RUSTFLAGS = staticFlags;
      cargoBuildFlags = ["--bin" "ponged-gateway"];
    };

    # ── Game client binary (Bevy + Vulkan, glibc dinámico) ──────────
    packages."x86_64-linux".ponged-cliente = pkgs.rustPlatform.buildRustPackage {
      pname = "ponged-cliente";
      version = "0.1.1";
      src = ./.;
      cargoLock.lockFile = ./Cargo.lock;
      nativeBuildInputs = [pkgs.pkg-config pkgs.perl];
      buildInputs = clientLibs;
      cargoBuildFlags = ["--bin" "ponged-cliente"];
    };

    # ── Docker image (gateway only, minimal) ────────────────────────
    packages."x86_64-linux".dockerImage = pkgs.dockerTools.buildLayeredImage {
      name = "ponged-gateway";
      tag = "latest";
      contents = [self.packages."x86_64-linux".ponged-gateway];
      config = {
        Cmd = ["ponged-gateway" "--listen" "/ip4/0.0.0.0/tcp/4001"];
        ExposedPorts = {"4001/tcp" = {};};
      };
    };

    # ── systemd unit (works en CUALQUIER distro con systemd) ────────
    # Genera /etc/systemd/system/ponged-gateway.service a partir del
    # binario estatico. La IP publica se lee de /etc/ponged-gateway.env
    # (PONG_PUBLIC=...), asi no hay que regenerar nada si cambia la EIP.
    packages."x86_64-linux".systemdUnit = pkgs.runCommand "ponged-gateway-systemd-unit" {} ''
      mkdir -p $out
      cat > $out/ponged-gateway.service <<'EOF'
      [Unit]
      Description=Pong P2P Matchmaking Gateway
      After=network.target

      [Service]
      EnvironmentFile=/etc/ponged-gateway.env
      ExecStart=/usr/local/bin/ponged-gateway --listen /ip4/0.0.0.0/tcp/4001 --public ''${PONG_PUBLIC}
      WorkingDirectory=/var/lib/ponged-gateway
      StateDirectory=ponged-gateway
      Restart=on-failure
      RestartSec=5

      [Install]
      WantedBy=multi-user.target
      EOF
    '';

    # Scrip que copia binario + unit y activa el servicio (distro-agnostico).
    packages."x86_64-linux".installSystemd = pkgs.writeShellScriptBin "install-ponged-gateway" ''
      set -euo pipefail
      BIN=${self.packages."x86_64-linux".ponged-gateway}/bin/ponged-gateway
      UNIT=${self.packages."x86_64-linux".systemdUnit}/ponged-gateway.service

      install -m 0755 "$BIN" /usr/local/bin/ponged-gateway
      install -m 0644 "$UNIT" /etc/systemd/system/ponged-gateway.service

      if [ ! -e /etc/ponged-gateway.env ]; then
        echo "PONG_PUBLIC=<EIP_ELASTICA>" > /etc/ponged-gateway.env
        echo "OJO: edita /etc/ponged-gateway.env con la IP publica real."
      fi

      systemctl daemon-reload
      systemctl enable --now ponged-gateway
    '';

    # ── NixOS module (systemd service) ──────────────────────────────
    nixosModules.ponged-gateway = { config, lib, pkgs, ... }: let
      cfg = config.services.ponged-gateway;
    in {
      options.services.ponged-gateway = {
        enable = lib.mkEnableOption "Pong P2P matchmaking gateway";
        package = lib.mkPackageOption pkgs "ponged-gateway" {};
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
        systemd.services.ponged-gateway = {
          description = "Pong P2P Matchmaking Gateway";
          after = ["network.target"];
          wantedBy = ["multi-user.target"];
          serviceConfig = {
            ExecStart = lib.concatStringsSep " " [
              "${cfg.package}/bin/ponged-gateway"
              "--listen" cfg.listen
              "--public" cfg.public
            ];
            WorkingDirectory = "/var/lib/ponged-gateway";
            StateDirectory = "ponged-gateway";
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
