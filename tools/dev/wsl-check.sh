#!/bin/sh
# Prints what the WSL environment has for Horizon mod development. Changes nothing.
uname -r
. /etc/os-release
echo "$PRETTY_NAME"
id -un
echo "cores: $(nproc)"
free -g | sed -n 2p
df -h "$HOME" | tail -1
for t in git cargo rustc rustup python3 gcc clang pkg-config cmake; do
    printf '%s: ' "$t"
    command -v "$t" || echo none
done
if sudo -n true 2>/dev/null; then echo sudo-nopass; else echo sudo-needs-password; fi
echo wsl-check-done
