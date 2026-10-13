{
  outputs = inputs@{
    self, nixpkgs, flake-parts,
  }: flake-parts.lib.mkFlake { inherit inputs; } {
    systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
    perSystem = { self', pkgs, lib, ... }: {
      packages.default = pkgs.rustPlatform.buildRustPackage {
        pname = "stable-npm";
        version = "0.1.0";
        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./src
          ];
        };
        cargoLock.lockFile = ./Cargo.lock;
      };

      packages.docker = pkgs.dockerTools.buildLayeredImage {
        name = "stable-npm";
        tag = "latest";
        contents = [ self'.packages.default pkgs.cacert ];
        config = {
          Entrypoint = [ "${self'.packages.default}/bin/stable-npm" ];
          Cmd = [ "--addr" "0.0.0.0:3000" "--db" "/data/stable-npm.db" ];
          Env = [ "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt" ];
          ExposedPorts."3000/tcp" = { };
          Volumes."/data" = { };
        };
      };

      devShells.default = pkgs.mkShell {
        nativeBuildInputs = with pkgs; [ rustc cargo clippy rustfmt nodejs ];
      };
    };
  };
}
