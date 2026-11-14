#!/bin/sh
# Builds and runs the host simulation of the bridge firmware: a simulated
# radio clocks bit by bit against the real decoder, session and protocol code.
# No hardware needed.
#
#   test/run-host-sim.sh                      on a made-up image
#   test/run-host-sim.sh image.bin edited.bin on two real exports
set -e
here=$(cd "$(dirname "$0")" && pwd)
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
clang++ -std=c++17 -Wall -Wextra -fsanitize=address,undefined \
  "$here/host_sim.cpp" -o "$out/host_sim"
"$out/host_sim" "$@"
