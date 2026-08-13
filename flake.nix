{
  description = "A very basic flake";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
  };

  outputs = { self, nixpkgs }: let
    pkgs = nixpkgs.legacyPackages."x86_64-linux";
  in {
    devShells."x86_64-linux".default = pkgs.mkShell {
      buildInputs = with pkgs; [
        cargo rustc rustfmt clippy rust-analyzer glib wayland-protocols wayland alsa-lib libudev-zero
      ];
	
      LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (with pkgs; [ wayland ]);
      shellHook = ''
	      export LD_LIBRARY_PATH=${pkgs.wayland}/lib:$LD_LIBRARY_PATH
	'';
      nativeBuildInputs = [ pkgs.pkg-config ];

      env.RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
    };
  };
}
