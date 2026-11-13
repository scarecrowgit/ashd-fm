#!/usr/bin/env bash
# Install fm for the current user: release binary, launcher entry and icon.
#   packaging/install.sh            build and install into ~/.local
#   packaging/install.sh --uninstall
# Wayland compositors pick the window/taskbar icon from the .desktop file whose name
# matches the window's app_id ("fm"), so the icon only shows once this is installed.
set -euo pipefail
cd "$(dirname "$0")/.."
prefix="${PREFIX:-$HOME/.local}"
icons="$prefix/share/icons/hicolor"

if [ "${1:-}" = "--uninstall" ]; then
  rm -f "$prefix/bin/fm" "$prefix/share/applications/fm.desktop"
  find "$icons" \( -name 'fm.png' -o -name 'fm.svg' -o -name 'fm-symbolic.svg' \) -path '*/apps/*' -delete 2>/dev/null || true
  echo "removed fm from $prefix"
  exit 0
fi

nix-shell --run 'cargo build --release'
install -Dm755 target/release/fm "$prefix/bin/fm"
# The binary needs the libraries from shell.nix at runtime; launch it through a wrapper.
libs=$(nix-shell --run 'echo $LD_LIBRARY_PATH')
mv "$prefix/bin/fm" "$prefix/bin/.fm-bin"
cat > "$prefix/bin/fm" <<WRAP
#!/bin/sh
export LD_LIBRARY_PATH="$libs\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}"
exec "$prefix/bin/.fm-bin" "\$@"
WRAP
chmod 755 "$prefix/bin/fm"

mkdir -p "$icons"
cp -r packaging/hicolor/. "$icons/"
sed "s|^Exec=fm|Exec=$prefix/bin/fm|" packaging/fm.desktop | install -Dm644 /dev/stdin "$prefix/share/applications/fm.desktop"
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -f "$icons" >/dev/null 2>&1 || true
command -v update-desktop-database >/dev/null && update-desktop-database "$prefix/share/applications" >/dev/null 2>&1 || true
echo "installed: $prefix/bin/fm, launcher entry and icon"
