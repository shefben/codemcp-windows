#!/bin/sh
# codemcp installer.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/skymoore/codemcp/main/install.sh | sh
#
# Environment overrides:
#   CODEMCP_VERSION   Release tag to install (default: latest). e.g. v0.1.0
#   CODEMCP_BIN_DIR   Install directory (default: first writable of
#                     $HOME/.local/bin, /usr/local/bin; falls back with sudo).
#   CODEMCP_REPO      GitHub owner/repo (default: skymoore/codemcp).
#   CODEMCP_BASE_URL  Override the release download base URL (for mirrors/testing).
#                     When set, CODEMCP_VERSION must also be set (no auto-latest).
#
# This downloads a prebuilt binary from GitHub Releases, verifies its SHA-256
# checksum, and installs it onto your PATH.

set -eu

REPO="${CODEMCP_REPO:-skymoore/codemcp}"
BIN="codemcp"

info() { printf '%s\n' "$*" >&2; }
err() { printf 'error: %s\n' "$*" >&2; exit 1; }

need() { command -v "$1" >/dev/null 2>&1 || err "required command not found: $1"; }

# --- pick a downloader -------------------------------------------------------
if command -v curl >/dev/null 2>&1; then
	DL="curl -fsSL"
	DL_O="curl -fsSL -o"
elif command -v wget >/dev/null 2>&1; then
	DL="wget -qO-"
	DL_O="wget -qO"
else
	err "need curl or wget to download codemcp"
fi

# --- detect platform ---------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
	Darwin) os="darwin" ;;
	Linux) os="linux" ;;
	*) err "unsupported OS: $os (supported: macOS, Linux)" ;;
esac

case "$arch" in
	arm64 | aarch64) arch="arm64" ;;
	x86_64 | amd64) arch="x86_64" ;;
	*) err "unsupported architecture: $arch (supported: arm64, x86_64)" ;;
esac

asset="${BIN}-${os}-${arch}.tar.gz"

# --- resolve version ---------------------------------------------------------
version="${CODEMCP_VERSION:-latest}"
if [ "$version" = "latest" ]; then
	if [ -n "${CODEMCP_BASE_URL:-}" ]; then
		err "CODEMCP_VERSION must be set when CODEMCP_BASE_URL is used"
	fi
	info "resolving latest release of $REPO ..."
	# Query the GitHub API for the latest tag (no jq dependency).
	tag="$($DL "https://api.github.com/repos/$REPO/releases/latest" \
		| sed -n 's/.*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1)"
	[ -n "$tag" ] || err "could not determine latest release tag for $REPO"
	version="$tag"
fi

base="${CODEMCP_BASE_URL:-https://github.com/$REPO/releases/download/$version}"
url="$base/$asset"
sum_url="$url.sha256"

info "installing $BIN $version ($os/$arch)"

# --- download ----------------------------------------------------------------
tmp="$(mktemp -d "${TMPDIR:-/tmp}/codemcp.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

info "downloading $url"
$DL_O "$tmp/$asset" "$url" || err "download failed: $url"

# --- verify checksum (best-effort: skip if the .sha256 is missing) -----------
if $DL_O "$tmp/$asset.sha256" "$sum_url" 2>/dev/null; then
	expected="$(awk '{print $1}' "$tmp/$asset.sha256")"
	if command -v shasum >/dev/null 2>&1; then
		actual="$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')"
	elif command -v sha256sum >/dev/null 2>&1; then
		actual="$(sha256sum "$tmp/$asset" | awk '{print $1}')"
	else
		actual=""
		info "warning: no shasum/sha256sum available; skipping checksum verification"
	fi
	if [ -n "$actual" ]; then
		[ "$expected" = "$actual" ] || err "checksum mismatch (expected $expected, got $actual)"
		info "checksum ok"
	fi
else
	info "warning: no checksum published for $asset; skipping verification"
fi

# --- unpack ------------------------------------------------------------------
need tar
tar -C "$tmp" -xzf "$tmp/$asset" || err "failed to extract $asset"
extracted="$tmp/${BIN}-${os}-${arch}"
[ -f "$extracted" ] || extracted="$tmp/$BIN"
[ -f "$extracted" ] || err "binary not found inside archive"
chmod 0755 "$extracted"

# --- choose install dir ------------------------------------------------------
pick_dir() {
	if [ -n "${CODEMCP_BIN_DIR:-}" ]; then
		echo "$CODEMCP_BIN_DIR"
		return
	fi
	for d in "$HOME/.local/bin" "/usr/local/bin"; do
		if [ -d "$d" ] && [ -w "$d" ]; then echo "$d"; return; fi
	done
	# Default: create ~/.local/bin (no sudo needed).
	echo "$HOME/.local/bin"
}

bindir="$(pick_dir)"
mkdir -p "$bindir" 2>/dev/null || true

dest="$bindir/$BIN"
if [ -w "$bindir" ] || [ ! -e "$bindir" ]; then
	install -m 0755 "$extracted" "$dest" 2>/dev/null || cp "$extracted" "$dest"
	chmod 0755 "$dest"
else
	info "elevating with sudo to write $dest"
	need sudo
	sudo install -m 0755 "$extracted" "$dest"
fi

info "installed $BIN -> $dest"

# --- PATH advice -------------------------------------------------------------
case ":$PATH:" in
	*":$bindir:"*) ;;
	*)
		info ""
		info "note: $bindir is not on your PATH."
		info "add it, e.g.:"
		info "  echo 'export PATH=\"$bindir:\$PATH\"' >> ~/.profile && . ~/.profile"
		info "opencode launches codemcp by bare name, so it must be on PATH."
		;;
esac

# --- usage: show an MCP config snippet ---------------------------------------
# Use the bare name when it resolves on PATH, otherwise the absolute path so the
# snippet is copy-pasteable regardless of PATH state.
if command -v "$BIN" >/dev/null 2>&1; then
	cmd="$BIN"
else
	cmd="$dest"
fi

info ""
info "to use codemcp, point your agent harness at it. MCP config (opencode \"mcp\" object):"
info ""
info "  {"
info "    \"mcp\": {"
info "      \"codemcp\": {"
info "        \"type\": \"local\","
info "        \"command\": [\"$cmd\"],"
info "        \"environment\": {"
info "          \"CODEMCP_CONFIG\": \"$HOME/.config/codemcp/mcp.json\","
info "          \"CODEMCP_INSTANCE_LABEL\": \"lmstudio\""
info "        },"
info "        \"enabled\": true"
info "      }"
info "    }"
info "  }"

info ""
info "done. next steps:"
info "  $BIN setup opencode      # adopt your existing opencode MCP servers automatically"
info "  $BIN --help"
