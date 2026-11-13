# fm

A keyboard-first file manager for Wayland, written in Rust with [GPUI](https://gpui.rs).
Part of a niri desktop rice; five themes (Ash, Ink, Midnight, Moss, Dusk).

- list and grid views, tabs, live filter, recursive search, editable path bar with autocomplete
- preview pane and Quick Look (`space`): syntax-highlighted text, rendered Markdown, images, archives
- copy / move / link jobs with progress, pause, conflict dialog and undo; trash; inline rename
- drag & drop onto folders, path crumbs, sidebar places and tags; rubber-band selection
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
packaging/install.sh      # release build + launcher entry + icon into ~/.local
packaging/integrate.sh    # optional: default folder handler, file dialogs, FileManager1, niri Super+E
packaging/integrate.sh --undo
```

Key bindings: press `?` in the app. Code layout and conventions: [ARCHITECTURE.md](ARCHITECTURE.md).
