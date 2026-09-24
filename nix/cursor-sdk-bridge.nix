# Prebuilt cursor-sdk-bridge from cursor/sdk-bridge releases (sdk.v1 contract).
# The standalone binary is a Bun --compile ELF with payload appended: never
# patchelf, strip, or fixup — nix-ld on NixOS loads it like a normal dynamic
# executable from the store.
{
  lib,
  stdenv,
  fetchurl,
}:

let
  version = "1.0.31";

  assetFor =
    system:
    {
      x86_64-linux = {
        platform = "linux-x64";
        hash = "sha256-Uny+vcaq1Op9MCb0m0h54+fz1ukHwFmCQYY4ArAiyDg=";
      };
      aarch64-linux = {
        platform = "linux-arm64";
        hash = "sha256-xbPc5SugH2CxUoYfAI6NLJMcC6n795oRDd6jda5AcXs=";
      };
      aarch64-darwin = {
        platform = "darwin-arm64";
        hash = "sha256-aXg1j6cqpREQjWgq7K8plA3wcIcR+DjN3FHmBJsgMbo=";
      };
    }
    .${system} or (throw "cursor-sdk-bridge: unsupported system ${system}");

  asset = assetFor stdenv.hostPlatform.system;
  archiveName = "cursor-sdk-bridge-standalone-${asset.platform}.tar.gz";
in

stdenv.mkDerivation {
  pname = "cursor-sdk-bridge";
  inherit version;

  src = fetchurl {
    url = "https://github.com/cursor/sdk-bridge/releases/download/v${version}/${archiveName}";
    hash = asset.hash;
  };

  dontBuild = true;
  dontStrip = true;
  dontPatchELF = true;
  dontFixup = true;

  nativeBuildInputs = [ ];

  unpackPhase = "true";

  installPhase = ''
    runHook preInstall
    mkdir -p "$out/share/cursor-sdk-bridge"
    tar -xzf "$src" -C "$out/share/cursor-sdk-bridge"
    mkdir -p "$out/bin"
    ln -s "$out/share/cursor-sdk-bridge/bin/cursor-sdk-bridge" "$out/bin/cursor-sdk-bridge"
    runHook postInstall
  '';

  meta = with lib; {
    description = "cursor-sdk-bridge standalone binary (sdk.v1 Connect server)";
    homepage = "https://github.com/cursor/sdk-bridge";
    license = licenses.mit;
    sourceProvenance = with sourceTypes; [ binaryNativeCode ];
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
      "aarch64-darwin"
    ];
    mainProgram = "cursor-sdk-bridge";
  };
}
