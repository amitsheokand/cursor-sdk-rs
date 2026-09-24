# Build cursor-seat (workspace member) with nixpkgs rustPlatform.
# OSS-safe: no private hostnames, product names, or user paths.
{
  lib,
  rustPlatform,
  makeWrapper,
  git,
  cursor-sdk-bridge,
}:

rustPlatform.buildRustPackage {
  pname = "cursor-seat";
  version = "0.1.0";

  src = lib.cleanSourceWith {
    src = lib.cleanSource ../.;
    filter =
      path: _type:
      let
        name = baseNameOf path;
      in
      name != "target"
      && name != ".git"
      && name != ".DS_Store"
      && name != "result"
      && !(lib.hasPrefix "result-" name)
      && !(lib.hasSuffix ".md" name && lib.hasPrefix "RECEIPT-" name)
      && !(lib.hasSuffix ".md" name && lib.hasPrefix "PACKET" name);
  };

  cargoLock = {
    lockFile = ../Cargo.lock;
    outputHashes = {
      "toolgate-0.1.0" = "sha256-I9v/kCH/e49lKay6MjPfLtY5nwj2yCPiRiwBDn6DtNY=";
    };
  };

  cargoBuildFlags = [
    "-p"
    "cursor-seat"
  ];

  nativeBuildInputs = [
    makeWrapper
  ];

  nativeCheckInputs = [
    git
  ];

  # prost-build/protox in build.rs — no protoc required.
  doCheck = true;

  cargoTestFlags = [
    "-p"
    "cursor-seat"
  ];

  preCheck = ''
    export HOME="$TMPDIR"
    export GIT_CONFIG_NOSYSTEM=1
    ${git}/bin/git config --global user.name "nix-cursor-seat"
    ${git}/bin/git config --global user.email "nix-cursor-seat@localhost"
  '';

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
