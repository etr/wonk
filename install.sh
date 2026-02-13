#!/bin/sh
# install.sh - Download and install the wonk binary for your platform.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/etr/wonk/main/install.sh | sh
#
# Options (via environment variables):
#   WONK_VERSION   - Version to install (default: latest)
#   WONK_INSTALL   - Installation directory (default: /usr/local/bin)
#   GITHUB_REPO    - GitHub repository (default: etr/wonk)
#
# Example:
#   WONK_VERSION=0.2.0 WONK_INSTALL=$HOME/.local/bin curl -fsSL ... | sh

set -eu

BINARY_NAME="wonk"
GITHUB_REPO="${GITHUB_REPO:-etr/wonk}"
INSTALL_DIR="${WONK_INSTALL:-/usr/local/bin}"

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
    CYGWIN*|MINGW*|MSYS*) echo "windows" ;;
    *) err "Unsupported operating system: $(uname -s)" ;;
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
    windows-x86_64) echo "x86_64-pc-windows-msvc" ;;
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

main() {
  local os arch target version ext="" download_url tmp_dir

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

  if [ "$os" = "windows" ]; then
    ext=".exe"
  fi

  local artifact="${BINARY_NAME}-${version}-${target}${ext}"
  download_url="https://github.com/${GITHUB_REPO}/releases/download/v${version}/${artifact}"

  tmp_dir=$(mktemp -d)
  trap 'rm -rf "$tmp_dir"' EXIT

  info "Downloading ${download_url}"
  download "$download_url" "${tmp_dir}/${BINARY_NAME}${ext}"

  # Install
  if [ ! -d "$INSTALL_DIR" ]; then
    info "Creating install directory: ${INSTALL_DIR}"
    mkdir -p "$INSTALL_DIR"
  fi

  local dest="${INSTALL_DIR}/${BINARY_NAME}${ext}"

  if [ -w "$INSTALL_DIR" ]; then
    mv "${tmp_dir}/${BINARY_NAME}${ext}" "$dest"
  else
    info "Elevated permissions required to install to ${INSTALL_DIR}"
    sudo mv "${tmp_dir}/${BINARY_NAME}${ext}" "$dest"
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

main
