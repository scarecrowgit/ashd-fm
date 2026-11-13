#!/bin/sh
# Called by xdg-desktop-portal-termfilechooser when an application asks for a file dialog:
#   $1 multiple  $2 directory  $3 save  $4 suggested path  $5 output file  $6 debug
# Runs fm as the chooser; fm writes the chosen paths to $5 (empty = cancelled).
multiple="$1" directory="$2" save="$3" path="$4" out="$5"
[ "$6" = 1 ] && set -x
fm="$(dirname "$0")/../../bin/fm"
[ -x "$fm" ] || fm=fm

if [ "$save" = 1 ]; then mode=--save
elif [ "$directory" = 1 ]; then mode=--directory
elif [ "$multiple" = 1 ]; then mode=--multiple
else mode=
fi
# shellcheck disable=SC2086
exec "$fm" --pick $mode --out "$out" "${path:-$HOME}"
