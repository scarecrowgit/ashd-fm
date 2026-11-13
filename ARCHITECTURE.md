# fm — architecture & working rules

Rust + [GPUI](https://gpui.rs) 0.2.2 implementation of `../SPEC.md` (the spec wins over the
prototype). Pixel/behaviour reference: `../prototype/*.html` (inline-styled HTML; grep it for
exact paddings, font sizes, colours — every colour there is a `var(--token)` = a `Theme` field).

## Build

NixOS, no global Rust: everything runs through the shell.

    nix-shell --run 'cargo build'          # must stay warning-free in files you own
    nix-shell --run 'cargo clippy'
    nix-shell --run 'cargo run -- [--theme ash|ink|midnight|moss|dusk] [path]'

`vendor/xattr` + `[patch.crates-io]` exist only to fix a libc incompatibility in a gpui
dependency — leave them alone.

## One root view, one file per region

There is a single GPUI entity, `FmApp` (`src/app.rs`). It owns all state and lays out the
window. Each region is a module exposing:

- `pub struct XxxState` — a field on `FmApp` (`app.sidebar`, `app.toolbar`, `app.files`,
  `app.preview`, `app.quicklook`, `app.overlays`, `app.ops`, `app.git`). Put whatever state you
  need there (including `Entity<TextInput>`s, scroll handles, `Task`s, `Subscription`s).
- `pub fn render(app: &mut FmApp, window, cx: &mut Context<FmApp>) -> AnyElement` (or
  `Option<…>` / `Vec<…>` for overlay layers).
- hooks and action handlers called by the shell (`on_directory_changed`, `cancel`, `act_*`).
  Their signatures are the contract: keep them, fill in the bodies.

| file | region | owner |
|---|---|---|
| `app.rs` `main.rs` `actions.rs` `theme.rs` `ui.rs` `icons.rs` `fs_model.rs` `text_input.rs` `assets.rs` `chrome.rs` | shell, tokens, shared kit | **shared — see rules** |
| `sidebar.rs` `git.rs` | sidebar §4.1, git status | sidebar agent |
| `toolbar.rs` | toolbar §4.2 | toolbar agent |
| `file_view.rs` | list §4.3, grid §4.4, states §6, rename, selection bar, drag & drop | file-view agent |
| `preview_content.rs` `preview.rs` | preview data + pane §4.5 | preview agent |
| `quicklook.rs` | Quick Look §5 | quicklook agent |
| `ops.rs` `overlays.rs` | operations, menus, dialogs, toasts, key hints §6–7 | ops agent |

### Rules for parallel work

1. Edit **only the files you own**. You may split your module into more files
   (`src/<module>/…` or `src/<module>_xyz.rs` + a `mod` line in `main.rs`).
2. Shared files: additive edits only, and only when there is no way around it (a new icon in
   `icons.rs` + `assets/icons/`, a new action + key in `actions.rs`, a `mod` line, a tiny
   helper in `ui.rs`). Never rename, re-sign or reformat what is there. List every shared
   edit in your final report. New crates in `Cargo.toml` are fine — list them too.
3. Cross-module calls: use the public `FmApp` methods (`navigate`, `set_cursor`, `set_filter`,
   `set_sort`, `reload`, `reload_and_focus`, `tab()`, `tab().targets()` …), the other module's
   documented `pub fn`s as they exist in the skeleton, or dispatch an action:
   `window.dispatch_action(Box::new(actions::Trash), cx)`. Do not reach into another module's
   state beyond its documented `pub` fields.
4. Never block the main thread on disk or subprocesses:
   `let task = cx.background_spawn(async move { … }); cx.spawn(async move |this, cx| { let r = task.await; this.update(cx, |this, cx| { …; cx.notify() }).ok(); }).detach();`
   (see `FmApp::load_active`, `preview::on_cursor_changed`).
5. Never hard-code a colour, font or radius: `let t = theme(cx).clone();` then `t.accent`,
   `t.font_mono.clone()`, `t.radius_control` … Layout constants live in `theme.rs`.
6. **Do not launch the app** (it opens a window on the user's live desktop and other agents
   are working in parallel). Verify with `cargo build` + `cargo clippy` and by reading your
   code against the spec. The integrator runs it.
7. When done: `git add -A && git commit` on your branch (end the message with
   `Co-Authored-By: Claude Fable 5.2 <noreply@anthropic.com>`), then report.

## Shared kit

- `theme.rs` — `Theme` global (5 themes), layout constants, `DUR_*`, easing fns
  (`ease_standard/enter/exit`), shadows (`t.shadow_overlay()` …), `t.scrim()`.
- `ui.rs` — `label` (mono 10.5 caps muted), `mono`, `keycap`, `icon_button`, `button`
  (`ButtonKind::{Primary,Secondary,Ghost}`), `hline`, `alpha(color, a)`.
- `icons.rs` — `icon(Icon::Folder, 14.).text_color(t.fg_muted)`; `FileType::icon()`.
- `fs_model.rs` — `Entry`, `FileType`, `read_dir`, `SortSpec`, `format_size`, `format_mtime`,
  `format_perms`, `display_path` (`~/…`), `expand_path`, `home_dir`, `trash_dir`.
- `text_input.rs` — `TextInput` entity + `TextInputEvent::{Changed,Submit,Cancel,Tab,Up,Down}`;
  usage in its module doc. It inherits font/size/colour from its parent element.
- `app.rs` — `Tab { path, entries, visible, cursor, selected, filter, load, scroll, … }`.
  Rows on screen are `tab.visible` (indices into `tab.entries`); use `tab.entry_at(row)`.

## Keyboard

All bindings are in `actions.rs`. Letter keys are bound in the context `Fm && !TextInput`, so
they are dead while a `TextInput` has focus; when your text field/overlay closes call
`app.focus_list(window)`. `esc` runs `FmApp::act_cancel`, which asks each module's
`cancel()` (overlays → quicklook → toolbar → file_view) and stops at the first `true`.
Overlay-specific keys (e.g. `s k r ↵` in the conflict dialog): add your own key context to the
overlay element + bindings scoped to it, or use `.on_key_down`.

## GPUI 0.2.2 cheat sheet

Source + examples (read them, don't guess the API):
`~/.cargo/registry/src/index.crates.io-*/gpui-0.2.2/{src,examples}` — notably
`examples/{input,uniform_list,animation,drag_drop,image_loading,opacity,shadow,scrollable,text,grid_layout}.rs`,
`src/elements/{div,uniform_list,list,img,svg,text,anchored,deferred,animation}.rs`, `src/styled.rs`.

- Views take `&mut Window` and `&mut Context<FmApp>`; listeners:
  `cx.listener(|this: &mut FmApp, ev: &ClickEvent, window, cx| …)`.
- `.on_click`, `.hover(|s| …)`-with-state, `.overflow_y_scroll()`, tooltips and `.on_drag` need
  a stateful element: `div().id("unique")` / `.id(("row", ix))`.
- Sizes are `px(12.5)`; flex via `.flex().flex_col().items_center().gap(px(8.))`;
  `.flex_1().min_w_0()` for shrinking children; `.truncate()` for ellipsis; text style via
  `.font_family(..).text_size(px(12.)).font_weight(FontWeight::MEDIUM).text_color(..)`.
- No `letter-spacing`, no `text-transform` (uppercase strings yourself — `ui::label` does),
  no CSS transitions: animate with `el.with_animation(id, Animation::new(Duration).with_easing(f), |el, delta| …)`
  (changing the id restarts it) and `.opacity()`.
- Virtualised lists: `uniform_list("id", count, cx.processor(|this, range, window, cx| -> Vec<_>))`
  `.track_scroll(handle)`. For variable heights use `list()`.
- Layers: the root is `relative`; an overlay is `div().absolute().inset_0()` (or
  `.top(..).left(..)`), painted after the content. Add `.occlude()` so mouse events don't
  reach what is underneath; `deferred(anchored().position(p).child(menu))` for popups that
  must escape clipping and stay inside the window.
- Images from disk: `img(path_buf)` (`.object_fit(ObjectFit::Cover)`), svg icons: `icons::icon`.
- Mouse: `.on_mouse_down(MouseButton::Right, cx.listener(|this, ev: &MouseDownEvent, window, cx| ev.position))`;
  clicks outside: `.on_mouse_down_out(..)`.
- Timers: `cx.background_executor().timer(Duration)` inside `cx.spawn`.
- After mutating state call `cx.notify()`.

## Packaging

`packaging/` holds the app icon (`hicolor/` theme tree, `fm.svg` source — the rice folder
glyph) and `fm.desktop`. `packaging/install.sh` builds a release binary and installs it with
the launcher entry and icon into `~/.local` (`--uninstall` removes them). The window's
`app_id` is `fm`, which is how Wayland compositors match it to `fm.desktop` and its icon.
