# ashd-fm

A keyboard-first file manager for Wayland, written in Rust with [GPUI](https://gpui.rs).
Part of a niri desktop rice; five themes (Ash, Ink, Midnight, Moss, Dusk).

- list and grid views, tabs, live filter, recursive search, editable path bar with autocomplete
- preview pane and Quick Look (`space`): syntax-highlighted text, rendered Markdown, images, archives
- copy / move / link jobs with progress, pause, conflict dialog and undo; trash; inline rename
- extract archives in place (`e` or the context menu): zip, tar.*, single `.gz/.xz/.zst/.bz2`,
  and 7z / rar when `7z`, `bsdtar` or `unrar` is installed
- open a terminal in the current folder (`t` / `F4`, or "Open terminal …" in the menus):
  `$TERMINAL`, else the first of foot, kitty, alacritty, wezterm, ghostty, konsole, gnome-terminal
- drag & drop onto folders, path crumbs, sidebar places and tags; rubber-band selection
- drag files past the window edge to hand them to other apps (browser, chat, editor, terminal);
  Wayland only, via the patched gpui in `vendor/gpui`
- git status column, devices with capacity, bookmarks, tags
- file-chooser mode for other apps (via xdg-desktop-portal-termfilechooser) and
  `org.freedesktop.FileManager1` ("Show in folder")

## Build and run

NixOS / Nix: everything goes through `shell.nix`.

```sh
nix-shell --run 'cargo run -- [--theme ash|ink|midnight|moss|dusk] [path]'
```

## Install

```sh
packaging/install.sh      # release build + launcher entry + icon into ~/.local (removes an old "fm" install)
packaging/integrate.sh    # optional: default folder handler, file dialogs, FileManager1, niri Super+E
packaging/integrate.sh --undo
```

Key bindings: press `?` in the app. Code layout and conventions: [ARCHITECTURE.md](ARCHITECTURE.md).

## License

MIT, see [LICENSE](LICENSE). `vendor/xattr` keeps its own MIT/Apache-2.0 licence.
