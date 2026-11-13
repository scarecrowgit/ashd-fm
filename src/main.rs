//! fm — the rice file manager (GPUI). See ARCHITECTURE.md for the module map.

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
mod overlays;
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
    // fm [--theme ash|ink|midnight|moss|dusk] [path]
    let mut theme = ThemeName::Ash;
    let mut start: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--theme" {
            theme = args.next().and_then(|t| ThemeName::from_label(&t)).unwrap_or(theme);
        } else {
            start = Some(fs_model::expand_path(&arg));
        }
    }
    let start = start
        .and_then(|p| p.canonicalize().ok())
        .filter(|p| p.is_dir())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(fs_model::home_dir);

    Application::new().with_assets(assets::Assets).run(move |cx: &mut App| {
        cx.set_global(Theme::get(theme));
        actions::bind_keys(cx);
        cx.on_action(|_: &actions::Quit, cx| cx.quit());

        let bounds = Bounds::centered(None, size(px(1600.), px(960.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                app_id: Some("fm".into()),
                window_min_size: Some(size(px(480.), px(360.))),
                ..Default::default()
            },
            move |window, cx| cx.new(|cx| app::FmApp::new(start.clone(), window, cx)),
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
