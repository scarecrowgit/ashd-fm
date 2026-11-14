//! ashd-fm — the rice file manager (GPUI). See ARCHITECTURE.md for the module map.

#![allow(dead_code)] // contracts are wider than the stubs use; remove once modules land

mod actions;
mod app;
mod assets;
mod chrome;
mod file_view;
mod fs_model;
mod git;
mod icons;
mod ops;
mod dbus_service;
mod overlays;
mod picker;
mod preview;
mod preview_content;
mod quicklook;
mod sidebar;
mod text_input;
mod theme;
mod toolbar;
mod ui;

use std::path::PathBuf;

use gpui::{App, Application, Bounds, WindowBounds, WindowOptions, prelude::*, px, size};

use crate::theme::{Theme, ThemeName};

fn main() {
    // ashd-fm [--theme ash|ink|midnight|moss|dusk] [--select] [path | file:// URI]
    // ashd-fm --pick --out FILE [--multiple | --directory | --save] [path]   (file-chooser mode)
    // ashd-fm --dbus-service                                                 (org.freedesktop.FileManager1)
    let mut theme = ThemeName::Ash;
    let mut target: Option<PathBuf> = None;
    let mut select = false;
    let (mut pick, mut out, mut kind) = (false, None, picker::PickKind::File);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--theme" => theme = args.next().and_then(|t| ThemeName::from_label(&t)).unwrap_or(theme),
            "--select" => select = true,
            "--pick" => pick = true,
            "--out" => out = args.next().map(PathBuf::from),
            "--multiple" => kind = picker::PickKind::Files,
            "--directory" => kind = picker::PickKind::Directory,
            "--save" => kind = picker::PickKind::Save,
            "--dbus-service" => {
                if let Err(err) = dbus_service::run() {
                    eprintln!("ashd-fm: D-Bus service failed: {err}");
                    std::process::exit(1);
                }
                return;
            }
            _ => target = Some(fs_model::path_from_arg(&arg)),
        }
    }
    let pick = match (pick, out) {
        (true, Some(out)) => Some((kind, out)),
        (true, None) => {
            eprintln!("ashd-fm: --pick needs --out FILE");
            std::process::exit(2);
        }
        _ => None,
    };

    // A file argument opens its folder with the cursor on the file; so does `--select`
    // for folders. In save mode a non-existing path is "folder/suggested-name".
    let target = target.map(|p| if p.is_absolute() { p } else { std::env::current_dir().unwrap_or_default().join(p) });
    let mut cursor_on: Option<PathBuf> = None;
    let mut suggested_name: Option<String> = None;
    let mut start: Option<PathBuf> = None;
    if let Some(path) = target {
        let saving = matches!(pick, Some((picker::PickKind::Save, _)));
        if path.is_dir() && !select {
            start = Some(path);
        } else if path.exists() || saving {
            if saving {
                suggested_name = path.file_name().map(|n| n.to_string_lossy().to_string());
            } else {
                cursor_on = Some(path.clone());
            }
            start = path.parent().map(PathBuf::from);
        } else {
            // Nearest existing ancestor.
            start = path.ancestors().find(|a| a.is_dir()).map(PathBuf::from);
        }
    }
    let start = start
        .filter(|p| p.is_dir())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(fs_model::home_dir);
    let pick = pick.map(|(kind, out)| picker::PickRequest { kind, out, name: suggested_name });
    let options = app::StartOptions { path: start, cursor_on, pick };

    Application::new().with_assets(assets::Assets).run(move |cx: &mut App| {
        cx.set_global(Theme::resolved(theme, cx));
        actions::bind_keys(cx);
        cx.on_action(|_: &actions::Quit, cx| cx.quit());

        let picking = options.pick.is_some();
        let (w, h) = if picking { (1100., 720.) } else { (1600., 960.) };
        let bounds = Bounds::centered(None, size(px(w), px(h)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                // A separate app id lets the compositor float the chooser (see packaging/).
                app_id: Some(if picking { "ashd-fm-picker" } else { "ashd-fm" }.into()),
                window_min_size: Some(size(px(480.), px(360.))),
                ..Default::default()
            },
            move |window, cx| cx.new(|cx| app::FmApp::new(options.clone(), window, cx)),
        )
        .unwrap();
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}
