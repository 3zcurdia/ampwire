#!/bin/sh
# Thin wrapper around the cargo-dist installer for Ampwire.
# Usage:
#   curl --proto '=https' --tlsv1.2 -LsSf https://ampwire.ezcurdia.dev/install.sh | sh
# Pin a version:
#   curl .../install.sh | AMPWIRE_VERSION=0.1.0 sh
set -eu

# Validate AMPWIRE_VERSION (optional pin).
VERSION="${AMPWIRE_VERSION:-latest}"
case "$VERSION" in
  latest) ;;
  *)
    # Allow only safe characters.
    case "$VERSION" in
      *[!0-9A-Za-z._-]*) echo "ampwire: invalid AMPWIRE_VERSION '$VERSION'" >&2; exit 2 ;;
    esac
    ;;
esac

if [ "$VERSION" = "latest" ]; then
  INSTALLER_URL="https://github.com/3zcurdia/ampwire/releases/latest/download/ampwire-installer.sh"
else
  INSTALLER_URL="https://github.com/3zcurdia/ampwire/releases/download/v${VERSION}/ampwire-installer.sh"
fi

# --help
if [ "${1:-}" = "--help" ] || [ "${1:-}" = "-h" ]; then
  cat <<EOF
ampwire installer wrapper

Variables:
  AMPWIRE_VERSION  optional version pin, e.g. 0.1.0 (validated: [0-9A-Za-z._-]).
                   Set it on sh, not on curl:
                   curl .../install.sh | AMPWIRE_VERSION=0.1.0 sh

Direct installer (fallback):
  https://github.com/3zcurdia/ampwire/releases/latest/download/ampwire-installer.sh
EOF
  exit 0
fi

# Need at least one downloader.
if ! command -v curl >/dev/null 2>&1 && ! command -v wget >/dev/null 2>&1; then
  echo "ampwire: need curl or wget" >&2
  exit 1
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

if command -v curl >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -LsSf "$INSTALLER_URL" -o "$tmp/ampwire-installer.sh"
else
  wget -q -O "$tmp/ampwire-installer.sh" "$INSTALLER_URL"
fi

# macOS sha256sum shim so cargo-dist verifies checksums.
if ! command -v sha256sum >/dev/null 2>&1; then
  if command -v shasum >/dev/null 2>&1; then
    mkdir -p "$tmp/bin"
    cat > "$tmp/bin/sha256sum" <<'EOF'
#!/bin/sh
exec shasum -a 256 "$@"
EOF
    chmod +x "$tmp/bin/sha256sum"
    PATH="$tmp/bin:$PATH"
  fi
fi

# Run installer, capturing output.
set +e
sh "$tmp/ampwire-installer.sh" >"$tmp/out" 2>"$tmp/err"
rc=$?
set -e
cat "$tmp/out"
cat "$tmp/err" >&2
if [ $rc -ne 0 ]; then
  echo "ampwire: installer failed (exit $rc)" >&2
  exit $rc
fi

# Post-check.
BIN="$HOME/.local/bin/ampwire"
if [ ! -x "$BIN" ]; then
  echo "ampwire: binary not found at $BIN" >&2
  echo "ampwire: installer output above; direct release: https://github.com/3zcurdia/ampwire/releases/latest" >&2
  exit 1
fi

set +e
ver_out=$("$BIN" --version 2>&1)
ver_rc=$?
set -e
if [ $ver_rc -ne 0 ]; then
  echo "ampwire: $BIN --version failed: $ver_out" >&2
  case "$ver_out" in
    *libasound*)
      echo "ampwire: install the ALSA runtime: libasound2/libasound2t64 (Debian/Ubuntu), alsa-lib (Fedora/Arch), alsa (openSUSE)" >&2
      ;;
    *GLIBC_*)
      echo "ampwire: your system is older than the glibc 2.35 baseline; use a newer distro or build from source" >&2
      ;;
    *)
      echo "ampwire: see the installer output above and the direct release URL" >&2
      ;;
  esac
  exit 1
fi

echo "$ver_out"

# PATH hint.
case ":$PATH:" in
  *":$HOME/.local/bin:"*) ;;
  *)
    echo "ampwire: $HOME/.local/bin is not on PATH; restart your shell or run: source \"$HOME/.local/bin/env\"" >&2
    ;;
esac

if [ "$VERSION" != "latest" ]; then
  echo "ampwire: pinned to $VERSION; upgrades are manual until the pin is removed" >&2
fi

echo "ampwire installed. Run ampwire --version to verify."
