{ lib, rustPlatform, src }:

rustPlatform.buildRustPackage {
  pname = "aster-completion";
  version = (builtins.fromTOML (builtins.readFile "${src}/Cargo.toml")).package.version;
  inherit src;

  # Fetch crates by their upstream lockfile checksums, without cargo install.
  cargoLock.lockFile = "${src}/Cargo.lock";
  doCheck = true;

  meta = {
    description = "Natural shell completion";
    homepage = "https://github.com/yawnbo/aster";
    license = lib.licenses.mit;
    mainProgram = "aster";
    platforms = lib.platforms.unix;
  };
}
