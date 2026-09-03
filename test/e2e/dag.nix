# Synthetic, guaranteed-uncached dependency DAG used by the end-to-end test.
#
#        top
#       /   \
#      a     b        (a and b are independent; top depends on both)
#
# `a` and `b` each sleep for `sleepSeconds`, so if the remote builder runs them
# concurrently the whole build takes ~sleepSeconds; if it serialises them it
# takes ~2x. `marker` is baked into each derivation so the output paths are novel
# and cannot be served from any binary cache: every run really builds.
{
  nixpkgs,
  system ? "aarch64-linux",
  sleepSeconds ? 30,
  marker ? "0",
}:
let
  pkgs = import nixpkgs { inherit system; };

  slow =
    name:
    pkgs.runCommand name { inherit marker; } ''
      echo "@@ ${name} START $(date -u +%Y-%m-%dT%H:%M:%SZ) marker=${marker}"
      sleep ${toString sleepSeconds}
      echo "@@ ${name} END   $(date -u +%Y-%m-%dT%H:%M:%SZ)"
      mkdir -p "$out"
      echo "${name}:${marker}" > "$out/name"
    '';
in
rec {
  a = slow "dep-a";
  b = slow "dep-b";

  top = pkgs.runCommand "top" { inherit marker; } ''
    echo "@@ top START $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    mkdir -p "$out"
    cat ${a}/name ${b}/name > "$out/combined"
    echo "@@ top END   $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  '';

  # A derivation that always fails, to exercise failure propagation and cleanup.
  failing = pkgs.runCommand "failing" { inherit marker; } ''
    echo "@@ failing START $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "this build is designed to fail" >&2
    exit 7
  '';
}
