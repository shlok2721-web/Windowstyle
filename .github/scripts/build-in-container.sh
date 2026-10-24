#!/bin/sh
# Runs INSIDE the distro container. Invoked by .github/workflows/release.yml as:
#   docker run ... "$IMAGE" /bin/sh /src/.github/scripts/build-in-container.sh
#
# This is a real file (not a `sh -s` heredoc) on purpose: with `sh -s`, any command
# that reads stdin (rustup, dnf, zypper, pacman...) can swallow the rest of the
# script and silently truncate the build.
set -eu

: "${DISTRO_ID:?}" "${DISTRO_FAMILY:?}" "${ASSET_NAME:?}" "${CONTAINER_IMAGE:?}"
: "${CARGO_HOME:?}" "${RUSTUP_HOME:?}" "${CARGO_TARGET_DIR:?}"
RUST_TOOLCHAIN="${RUST_TOOLCHAIN:-stable}"

# Container runs as root on a checkout owned by the runner user.
git config --global --add safe.directory /src 2>/dev/null || true

case "$DISTRO_FAMILY" in
  debian)
    apt-get update
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      ca-certificates curl file git tar gzip binutils build-essential pkg-config \
      libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
      libx11-dev libx11-xcb-dev libxcb1-dev libxcb-render0-dev \
      libxcb-shape0-dev libxcb-xfixes0-dev libxcb-xkb-dev \
      libxrandr-dev libxcursor-dev libxi-dev libxinerama-dev \
      libxext-dev libxrender-dev libxfixes-dev libgl1-mesa-dev \
      libudev-dev libssl-dev
    ;;
  fedora)
    dnf -y install --allowerasing \
      ca-certificates curl file git tar gzip binutils gcc gcc-c++ make pkgconf-pkg-config \
      libxkbcommon-devel libxkbcommon-x11-devel wayland-devel \
      libX11-devel libxcb-devel libXrandr-devel \
      libXcursor-devel libXi-devel libXinerama-devel libXext-devel \
      libXrender-devel libXfixes-devel mesa-libGL-devel systemd-devel \
      openssl-devel
    ;;
  rhel)
    # Rocky/RHEL 9: several -devel packages (e.g. libxkbcommon-x11-devel) live in CRB.
    dnf -y install dnf-plugins-core
    dnf config-manager --set-enabled crb
    # --allowerasing: the base image ships curl-minimal, which conflicts with curl.
    dnf -y install --allowerasing \
      ca-certificates curl file git tar gzip binutils gcc gcc-c++ make pkgconf-pkg-config \
      libxkbcommon-devel libxkbcommon-x11-devel wayland-devel \
      libX11-devel libxcb-devel libXrandr-devel \
      libXcursor-devel libXi-devel libXinerama-devel libXext-devel \
      libXrender-devel libXfixes-devel mesa-libGL-devel systemd-devel \
      openssl-devel
    ;;
  opensuse)
    zypper --non-interactive --gpg-auto-import-keys refresh
    zypper --non-interactive install --no-recommends \
      ca-certificates curl file git tar gzip binutils gcc gcc-c++ make pkg-config \
      libxkbcommon-devel libxkbcommon-x11-devel wayland-devel \
      libX11-devel libxcb-devel libXrandr-devel libXcursor-devel \
      libXi-devel libXinerama-devel libXext-devel libXrender-devel \
      libXfixes-devel Mesa-libGL-devel libopenssl-devel libudev-devel
    ;;
  arch)
    # libxkbcommon-x11 and libxfixes are separate packages on Arch.
    pacman -Syu --noconfirm --needed \
      ca-certificates curl file git tar gzip binutils base-devel pkgconf \
      libxkbcommon libxkbcommon-x11 libx11 libxcb wayland mesa \
      libxrandr libxcursor libxi libxinerama libxext libxrender libxfixes \
      openssl systemd-libs
    ;;
  alpine)
    apk add --no-cache \
      ca-certificates curl file git tar gzip binutils build-base pkgconf linux-headers \
      libxkbcommon-dev wayland-dev libx11-dev libxcb-dev mesa-dev openssl-dev \
      libxrandr-dev libxcursor-dev libxi-dev libxinerama-dev libxext-dev \
      libxrender-dev libxfixes-dev eudev-dev \
      rust cargo
    ;;
  *)
    echo "Unsupported distribution family: $DISTRO_FAMILY" >&2
    exit 2
    ;;
esac

export PATH="$CARGO_HOME/bin:$PATH"

# Alpine (musl): use the distro Rust; rustup-init's GNU-linked binary is a bad fit there.
if [ "$DISTRO_FAMILY" != "alpine" ]; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup-init.sh
  sh /tmp/rustup-init.sh -y --profile minimal --default-toolchain "$RUST_TOOLCHAIN" </dev/null
fi

rustc --version
cargo --version

# Strip build-machine paths from the binary; add per-leg flags (e.g. target-cpu).
export RUSTFLAGS="${RUSTFLAGS_EXTRA:-} --remap-path-prefix=/src=src --remap-path-prefix=$CARGO_HOME=cargo"

# --locked: fail instead of silently re-resolving dependencies. Release-only by design.
cargo build --release --locked -p fm_app

BIN="$CARGO_TARGET_DIR/release/file-manager"
test -x "$BIN"

# Portability guard: refuse to ship a "portable" binary that needs a newer glibc than promised.
if [ -n "${MAX_GLIBC:-}" ]; then
  need="$(objdump -T "$BIN" | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/^GLIBC_//' | sort -uV | tail -n 1)"
  highest="$(printf '%s\n%s\n' "$need" "$MAX_GLIBC" | sort -V | tail -n 1)"
  echo "Highest glibc symbol required: ${need:-none} (limit: $MAX_GLIBC)"
  if [ "$highest" != "$MAX_GLIBC" ]; then
    echo "ERROR: binary needs glibc $need, newer than allowed $MAX_GLIBC" >&2
    exit 1
  fi
fi

OUT="/src/dist/$DISTRO_ID"
STAGE="$(mktemp -d)"
mkdir -p "$OUT"

install -m 0755 "$BIN" "$STAGE/file-manager"
cp /src/README.md "$STAGE/README.md"
[ -f /src/LICENSE ] && cp /src/LICENSE "$STAGE/LICENSE"

{
  printf 'Application: file-manager\n'
  printf 'Build id: %s\n' "$DISTRO_ID"
  printf 'Container image: %s\n' "$CONTAINER_IMAGE"
  printf 'Architecture: x86_64\n'
  printf 'Rust target: '
  rustc -vV | sed -n 's/^host: //p'
  printf 'Extra RUSTFLAGS: %s\n' "${RUSTFLAGS_EXTRA:-none}"
  printf 'Commit: %s\n' "${COMMIT_SHA:-unknown}"
  printf 'Release ref: %s\n' "${RELEASE_REF:-unknown}"
  printf '\n--- /etc/os-release ---\n'
  cat /etc/os-release
  printf '\n--- Rust compiler ---\n'
  rustc --version
  printf '\n--- C library ---\n'
  (ldd --version 2>&1 || true) | head -n 2
  printf '\n--- Highest glibc symbol required ---\n'
  (objdump -T "$BIN" | grep -o 'GLIBC_[0-9][0-9.]*' | sort -uV | tail -n 1) 2>/dev/null || echo 'n/a (not glibc)'
} > "$STAGE/BUILD-INFO.txt"

FILES="file-manager README.md BUILD-INFO.txt"
[ -f "$STAGE/LICENSE" ] && FILES="$FILES LICENSE"
# shellcheck disable=SC2086
tar -C "$STAGE" -czf "$OUT/${ASSET_NAME}.tar.gz" $FILES

# Container runs as root; hand ownership back so the runner can read/clean up.
if [ -n "${HOST_UID:-}" ]; then
  chown -R "$HOST_UID:${HOST_GID:-$HOST_UID}" /src/dist || true
fi

echo "Built $OUT/${ASSET_NAME}.tar.gz"
