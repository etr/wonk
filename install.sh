#!/bin/sh
# install.sh - Download and install the wonk binary for your platform.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/etr/wonk/main/install.sh | sh
#   curl -fsSL ... | sh -s -- --install-dir ~/.local/bin
#   curl -fsSL ... | sh -s -- --install-dir ~/.local/bin --version 0.3.0
#
# Options (via CLI arguments — preferred for piped installs):
#   --install-dir  - Installation directory (default: /usr/local/bin)
#   --version      - Version to install (default: latest)
#
# Options (via environment variables):
#   WONK_INSTALL   - Installation directory (overridden by --install-dir)
#   WONK_VERSION   - Version to install (overridden by --version)
#   GITHUB_REPO    - GitHub repository (default: etr/wonk)

set -eu

BINARY_NAME="wonk"
GITHUB_REPO="${GITHUB_REPO:-etr/wonk}"

# --- Logging helpers ---

info() {
  printf '[wonk] %s\n' "$@"
}

err() {
  printf '[wonk] ERROR: %s\n' "$@" >&2
  exit 1
}

# --- Platform detection ---

detect_os() {
  case "$(uname -s)" in
    Linux*)  echo "linux" ;;
    Darwin*) echo "macos" ;;
    *) err "Unsupported operating system: $(uname -s). wonk requires a Unix-like OS (Linux or macOS)." ;;
  esac
}

detect_arch() {
  case "$(uname -m)" in
    x86_64|amd64)  echo "x86_64" ;;
    aarch64|arm64)  echo "aarch64" ;;
    *) err "Unsupported architecture: $(uname -m)" ;;
  esac
}

# Map OS + arch to Rust target triple used in release artifacts.
get_target() {
  local os="$1"
  local arch="$2"

  case "${os}-${arch}" in
    linux-x86_64)   echo "x86_64-unknown-linux-musl" ;;
    linux-aarch64)  echo "aarch64-unknown-linux-musl" ;;
    macos-x86_64)   echo "x86_64-apple-darwin" ;;
    macos-aarch64)  echo "aarch64-apple-darwin" ;;
    *) err "No prebuilt binary for ${os} ${arch}" ;;
  esac
}

# --- Version resolution ---

get_latest_version() {
  local url="https://api.github.com/repos/${GITHUB_REPO}/releases/latest"
  local tag

  if command -v curl >/dev/null 2>&1; then
    tag=$(curl -fsSL "$url" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/')
  elif command -v wget >/dev/null 2>&1; then
    tag=$(wget -qO- "$url" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/')
  else
    err "Neither curl nor wget found. Please install one of them."
  fi

  if [ -z "$tag" ]; then
    err "Could not determine the latest version. Set WONK_VERSION explicitly."
  fi

  echo "$tag"
}

# --- Download helper ---

download() {
  local url="$1"
  local dest="$2"

  if command -v curl >/dev/null 2>&1; then
    curl -fsSL -o "$dest" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$dest" "$url"
  else
    err "Neither curl nor wget found. Please install one of them."
  fi
}

# --- Main ---

TMP_DIR=""
cleanup() { [ -n "$TMP_DIR" ] && rm -rf "$TMP_DIR"; }
trap cleanup EXIT

main() {
  local os arch target version download_url

  # Default install directory (env var, then fallback)
  INSTALL_DIR="${WONK_INSTALL:-/usr/local/bin}"

  # Parse arguments (override env vars)
  while [ $# -gt 0 ]; do
    case "$1" in
      --install-dir) INSTALL_DIR="$2"; shift 2 ;;
      --version) WONK_VERSION="$2"; shift 2 ;;
      *) err "Unknown argument: $1" ;;
    esac
  done

  os=$(detect_os)
  arch=$(detect_arch)
  target=$(get_target "$os" "$arch")

  info "Detected platform: ${os} ${arch} (${target})"

  if [ -n "${WONK_VERSION:-}" ]; then
    version="$WONK_VERSION"
  else
    info "Resolving latest version..."
    version=$(get_latest_version)
  fi

  info "Installing ${BINARY_NAME} v${version}"

  local artifact="${BINARY_NAME}-${version}-${target}"
  download_url="https://github.com/${GITHUB_REPO}/releases/download/v${version}/${artifact}"

  TMP_DIR=$(mktemp -d)

  info "Downloading ${download_url}"
  download "$download_url" "${TMP_DIR}/${BINARY_NAME}"

  # Install
  if [ ! -d "$INSTALL_DIR" ]; then
    info "Creating install directory: ${INSTALL_DIR}"
    mkdir -p "$INSTALL_DIR"
  fi

  local dest="${INSTALL_DIR}/${BINARY_NAME}"

  if [ -w "$INSTALL_DIR" ]; then
    mv "${TMP_DIR}/${BINARY_NAME}" "$dest"
  else
    info "Elevated permissions required to install to ${INSTALL_DIR}"
    sudo mv "${TMP_DIR}/${BINARY_NAME}" "$dest"
  fi

  chmod +x "$dest"

  info "Installed ${BINARY_NAME} v${version} to ${dest}"

  # Verify
  if command -v "$dest" >/dev/null 2>&1; then
    info "Run '${BINARY_NAME} --help' to get started."
  else
    info "Note: ${INSTALL_DIR} may not be in your PATH."
    info "Add it with: export PATH=\"${INSTALL_DIR}:\$PATH\""
  fi
}

main "$@"
