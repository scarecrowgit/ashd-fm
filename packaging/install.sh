#!/usr/bin/env bash
# Install ashd-fm for the current user: release binary, launcher entry and icon.
#   packaging/install.sh            build and install into ~/.local
#   packaging/install.sh --uninstall
# Wayland compositors pick the window/taskbar icon from the .desktop file whose name
# matches the window's app_id ("ashd-fm"), so the icon only shows once this is installed.
# An install made under the app's old name ("fm") is removed along the way.
set -euo pipefail
cd "$(dirname "$0")/.."
prefix="${PREFIX:-$HOME/.local}"
icons="$prefix/share/icons/hicolor"

remove_named() { # $1 = binary / desktop / icon name
  rm -f "$prefix/bin/$1" "$prefix/bin/.$1-bin" "$prefix/share/applications/$1.desktop"
  find "$icons" \( -name "$1.png" -o -name "$1.svg" -o -name "$1-symbolic.svg" \) -path '*/apps/*' -delete 2>/dev/null || true
}

# The old "fm" install, recognised by its wrapper pointing at .fm-bin.
remove_legacy() {
  if [ -f "$prefix/bin/fm" ] && grep -q '/\.fm-bin"' "$prefix/bin/fm" 2>/dev/null; then
    remove_named fm
    echo "removed the old fm install from $prefix"
  fi
}

if [ "${1:-}" = "--uninstall" ]; then
  remove_named ashd-fm
  remove_legacy
  echo "removed ashd-fm from $prefix"
  exit 0
fi

nix-shell --run 'cargo build --release'
install -Dm755 target/release/ashd-fm "$prefix/bin/.ashd-fm-bin"
# The binary needs the libraries from shell.nix at runtime; launch it through a wrapper.
libs=$(nix-shell --run 'echo $LD_LIBRARY_PATH')
cat > "$prefix/bin/ashd-fm" <<WRAP
#!/bin/sh
export LD_LIBRARY_PATH="$libs\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}"
exec "$prefix/bin/.ashd-fm-bin" "\$@"
WRAP
chmod 755 "$prefix/bin/ashd-fm"
remove_legacy

mkdir -p "$icons"
cp -r packaging/hicolor/. "$icons/"
sed "s|^Exec=ashd-fm|Exec=$prefix/bin/ashd-fm|" packaging/ashd-fm.desktop | install -Dm644 /dev/stdin "$prefix/share/applications/ashd-fm.desktop"
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -f "$icons" >/dev/null 2>&1 || true
command -v update-desktop-database >/dev/null && update-desktop-database "$prefix/share/applications" >/dev/null 2>&1 || true
echo "installed: $prefix/bin/ashd-fm, launcher entry and icon"
