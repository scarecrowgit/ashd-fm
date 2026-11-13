#!/usr/bin/env bash
# Make the installed fm the desktop's file manager (user-level only, nothing needs root):
#   1. default handler for folders (xdg-open, "open folder" in apps)
#   2. org.freedesktop.FileManager1 ("Show in folder" in browsers, chat apps, …)
#   3. file dialogs of portal-aware apps, via xdg-desktop-portal-termfilechooser + fm --pick
#   4. niri: Mod+E opens fm, the chooser window floats
# Run packaging/install.sh first.   packaging/integrate.sh --undo   reverts all of it.
set -euo pipefail
cd "$(dirname "$0")/.."
prefix="${PREFIX:-$HOME/.local}"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
config="${XDG_CONFIG_HOME:-$HOME/.config}"
state="${XDG_STATE_HOME:-$HOME/.local/state}/fm"
niri_extra="$config/niri/config.d/90-user-extra.kdl"
begin="// >>> fm (packaging/integrate.sh) >>>"
end="// <<< fm <<<"

strip_niri_block() {
  [ -f "$niri_extra" ] || return 0
  awk -v b="$begin" -v e="$end" '$0==b{skip=1} !skip{print} $0==e{skip=0}' "$niri_extra" > "$niri_extra.tmp"
  # drop trailing blank lines left behind
  sed -e :a -e '/^\n*$/{$d;N;ba' -e '}' "$niri_extra.tmp" > "$niri_extra"
  rm -f "$niri_extra.tmp"
}

if [ "${1:-}" = "--undo" ]; then
  rm -f "$data/dbus-1/services/org.freedesktop.FileManager1.service" \
        "$data/dbus-1/services/org.freedesktop.impl.portal.desktop.termfilechooser.service" \
        "$data/xdg-desktop-portal/portals/termfilechooser.portal" \
        "$config/xdg-desktop-portal-termfilechooser/config" \
        "$prefix/libexec/fm/portal-wrapper.sh" "$state/termfilechooser"
  if [ -f "$state/portals.conf.created" ]; then rm -f "$config/xdg-desktop-portal/portals.conf" "$state/portals.conf.created"
  elif [ -f "$state/portals.conf.bak" ]; then mv "$state/portals.conf.bak" "$config/xdg-desktop-portal/portals.conf"; fi
  if [ -f "$state/previous-directory-handler" ]; then
    prev=$(cat "$state/previous-directory-handler")
    [ -n "$prev" ] && xdg-mime default "$prev" inode/directory
  fi
  strip_niri_block
  busctl --user call org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus ReloadConfig >/dev/null 2>&1 || true
  systemctl --user restart xdg-desktop-portal.service 2>/dev/null || true
  echo "fm integration removed (log out and in to release D-Bus names already held)"
  exit 0
fi

[ -x "$prefix/bin/fm" ] || { echo "run packaging/install.sh first" >&2; exit 1; }
mkdir -p "$state" "$data/dbus-1/services" "$data/xdg-desktop-portal/portals" \
         "$config/xdg-desktop-portal" "$config/xdg-desktop-portal-termfilechooser" "$prefix/libexec/fm"

# 1. folders
[ -f "$state/previous-directory-handler" ] || xdg-mime query default inode/directory > "$state/previous-directory-handler" || true
xdg-mime default fm.desktop inode/directory

# 2. "Show in folder"
cat > "$data/dbus-1/services/org.freedesktop.FileManager1.service" <<SERVICE
[D-BUS Service]
Name=org.freedesktop.FileManager1
Exec=$prefix/bin/fm --dbus-service
SERVICE

# 3. file dialogs
install -m755 packaging/portal-wrapper.sh "$prefix/libexec/fm/portal-wrapper.sh"
nix build nixpkgs#xdg-desktop-portal-termfilechooser --out-link "$state/termfilechooser"
cat > "$data/dbus-1/services/org.freedesktop.impl.portal.desktop.termfilechooser.service" <<SERVICE
[D-BUS Service]
Name=org.freedesktop.impl.portal.desktop.termfilechooser
Exec=$state/termfilechooser/libexec/xdg-desktop-portal-termfilechooser
SERVICE
cat > "$data/xdg-desktop-portal/portals/termfilechooser.portal" <<PORTAL
[portal]
DBusName=org.freedesktop.impl.portal.desktop.termfilechooser
Interfaces=org.freedesktop.impl.portal.FileChooser;
PORTAL
cat > "$config/xdg-desktop-portal-termfilechooser/config" <<CONFIG
[filechooser]
cmd=$prefix/libexec/fm/portal-wrapper.sh
default_dir=\$HOME
create_help_file=0
open_mode=last
save_mode=last
CONFIG
portals="$config/xdg-desktop-portal/portals.conf"
if [ -f "$portals" ] && ! grep -q termfilechooser "$portals"; then
  cp "$portals" "$state/portals.conf.bak"
  if grep -q '^org.freedesktop.impl.portal.FileChooser=' "$portals"; then
    sed -i 's|^org.freedesktop.impl.portal.FileChooser=.*|org.freedesktop.impl.portal.FileChooser=termfilechooser|' "$portals"
  else
    sed -i '/^\[preferred\]/a org.freedesktop.impl.portal.FileChooser=termfilechooser' "$portals"
  fi
elif [ ! -f "$portals" ]; then
  # A user portals.conf replaces the system one, so carry its preferences over.
  system=$(ls /run/current-system/sw/share/xdg-desktop-portal/*portals.conf /etc/xdg/xdg-desktop-portal/*portals.conf 2>/dev/null | head -1 || true)
  if [ -n "$system" ]; then grep -v '^org.freedesktop.impl.portal.FileChooser=' "$system" > "$portals"
  else printf '[preferred]\ndefault=gtk;\n' > "$portals"; fi
  echo 'org.freedesktop.impl.portal.FileChooser=termfilechooser' >> "$portals"
  touch "$state/portals.conf.created"
fi

# 4. niri
if [ -f "$niri_extra" ]; then
  strip_niri_block
  cat >> "$niri_extra" <<NIRI

$begin
binds {
    // Same key spec as the stock nautilus bind in 70-binds.kdl, so this one replaces it.
    Super+E { spawn "$prefix/bin/fm"; }
}
window-rule {
    match app-id="^fm-picker\$"
    open-floating true
}
$end
NIRI
fi

# dbus-broker only learns about new service files on reload.
busctl --user call org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus ReloadConfig >/dev/null 2>&1 || true
systemctl --user restart xdg-desktop-portal.service 2>/dev/null || true
echo "fm is now the default file manager, file chooser and Mod+E target"
