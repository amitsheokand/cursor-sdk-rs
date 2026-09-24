# Build cursor-seat (workspace member) with nixpkgs rustPlatform.
# OSS-safe: no private hostnames, product names, or user paths.
{
  lib,
  rustPlatform,
  makeWrapper,
  cursor-sdk-bridge,
}:

rustPlatform.buildRustPackage {
  pname = "cursor-seat";
  version = "0.1.0";

  src = lib.cleanSourceWith {
    src = ../.;
    filter =
      path: _type:
      let
        name = baseNameOf path;
      in
      name != "target"
      && name != ".git"
      && name != ".DS_Store"
      && !(lib.hasSuffix ".md" name && lib.hasPrefix "RECEIPT-" name)
      && !(lib.hasSuffix ".md" name && lib.hasPrefix "PACKET" name);
  };

  cargoLock.lockFile = ../Cargo.lock;

  # When cursor-seat adds a git dependency on toolgate, add cargoLock.outputHashes.
  cargoBuildFlags = [
    "-p"
    "cursor-seat"
  ];

  nativeBuildInputs = [
    makeWrapper
  ];

  # prost-build/protox in build.rs — no protoc required.
  doCheck = false;

  postInstall = ''
    wrapProgram $out/bin/cursor-seat \
      --set-default CURSOR_SDK_BRIDGE_BIN ${cursor-sdk-bridge}/bin/cursor-sdk-bridge
  '';

  meta = with lib; {
    description = "Cursor seat binary: typed stdin inbox and durable agent runs";
    license = licenses.mit;
    mainProgram = "cursor-seat";
    platforms = platforms.unix;
  };
}
