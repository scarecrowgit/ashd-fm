//! Design tokens (tokens/tokens.json) as a GPUI global. Never hard-code hex in views:
//! read `theme(cx)` (or `cx.global::<Theme>()`) and use its fields.

use gpui::{App, BoxShadow, Global, Hsla, Pixels, SharedString, hsla, point, px, rgb};

// ── layout constants (SPEC §3) ────────────────────────────────────────────────
pub const TITLEBAR_H: f32 = 28.;
pub const TABS_H: f32 = 36.;
pub const SIDEBAR_W: f32 = 230.;
pub const SIDEBAR_RAIL_W: f32 = 44.;
pub const TOOLBAR_H: f32 = 48.;
pub const PREVIEW_W: f32 = 340.;
pub const LIST_HEADER_H: f32 = 30.;
pub const ROW_H: f32 = 30.;
pub const FOOTER_H: f32 = 30.;
pub const HINTBAR_H: f32 = 34.;
/// Below this window width the preview pane collapses.
pub const BREAK_PREVIEW: f32 = 1100.;
/// Below this window width the sidebar becomes an icon rail.
pub const BREAK_SIDEBAR: f32 = 820.;

// ── motion (SPEC §9), milliseconds ────────────────────────────────────────────
pub const DUR_INSTANT: u64 = 80;
pub const DUR_FAST: u64 = 120;
pub const DUR_FLYOUT: u64 = 160;
pub const DUR_BASE: u64 = 220;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemeName {
    Ash,
    Ink,
    Midnight,
    Moss,
    Dusk,
}

impl ThemeName {
    pub const ALL: [ThemeName; 5] = [Self::Ash, Self::Ink, Self::Midnight, Self::Moss, Self::Dusk];

    pub fn label(self) -> &'static str {
        match self {
            Self::Ash => "ash",
            Self::Ink => "ink",
            Self::Midnight => "midnight",
            Self::Moss => "moss",
            Self::Dusk => "dusk",
        }
    }

    pub fn next(self) -> Self {
        let ix = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(ix + 1) % Self::ALL.len()]
    }

    pub fn from_label(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.label() == s)
    }
}

#[derive(Clone)]
pub struct Theme {
    pub name: ThemeName,
    pub bg: Hsla,
    pub surface: Hsla,
    pub surface_2: Hsla,
    pub line: Hsla,
    pub frame: Hsla,
    pub fg: Hsla,
    pub fg_muted: Hsla,
    pub accent: Hsla,
    pub on_accent: Hsla,
    pub ok: Hsla,
    pub warn: Hsla,
    pub err: Hsla,
    pub info: Hsla,
    pub syn_keyword: Hsla,
    pub syn_string: Hsla,
    pub syn_number: Hsla,
    pub syn_comment: Hsla,
    pub font_sans: SharedString,
    pub font_mono: SharedString,
    pub radius_panel: Pixels,
    pub radius_window: Pixels,
    pub radius_control: Pixels,
}

impl Global for Theme {}

/// Shorthand for `cx.global::<Theme>()`. Clone it (`theme(cx).clone()`) when you
/// also need `cx` mutably afterwards.
pub fn theme(cx: &App) -> &Theme {
    cx.global::<Theme>()
}

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

impl Theme {
    #[allow(clippy::too_many_arguments)]
    fn build(
        name: ThemeName,
        colors: [u32; 17],
        sans: &'static str,
        mono: &'static str,
        radius: [f32; 3],
    ) -> Self {
        let [bg, surface, surface_2, line, frame, fg, fg_muted, accent, on_accent, ok, warn, err, info, sk, ss, sn, sc] =
            colors;
        Theme {
            name,
            bg: c(bg),
            surface: c(surface),
            surface_2: c(surface_2),
            line: c(line),
            frame: c(frame),
            fg: c(fg),
            fg_muted: c(fg_muted),
            accent: c(accent),
            on_accent: c(on_accent),
            ok: c(ok),
            warn: c(warn),
            err: c(err),
            info: c(info),
            syn_keyword: c(sk),
            syn_string: c(ss),
            syn_number: c(sn),
            syn_comment: c(sc),
            font_sans: sans.into(),
            font_mono: mono.into(),
            radius_panel: px(radius[0]),
            radius_window: px(radius[1]),
            radius_control: px(radius[2]),
        }
    }

    pub fn get(name: ThemeName) -> Self {
        match name {
            ThemeName::Ash => Self::build(
                name,
                [
                    0x111110, 0x161615, 0x1B1B19, 0x2A2A27, 0x2A2A27, 0xDAD7CF, 0x96938B, 0xE0A458,
                    0x161615, 0x9DBF86, 0xE6C25A, 0xE07A62, 0x8DB0D6, 0xE0A458, 0xC2BEB4, 0xDAD7CF,
                    0x8A877F,
                ],
                "Inter",
                "JetBrains Mono",
                [0., 0., 0.],
            ),
            ThemeName::Ink => Self::build(
                name,
                [
                    0x0E0F11, 0x15171A, 0x1A1D21, 0x2A2E34, 0x4A4F57, 0xE6E2D9, 0x9C998F, 0xFF5B3A,
                    0x0E0F11, 0x8FC48C, 0xE7C35E, 0xF2667A, 0x8DB4E2, 0xFF6D4F, 0xC9C5BC, 0xE6E2D9,
                    0x8C8982,
                ],
                "IBM Plex Sans",
                "IBM Plex Mono",
                [0., 0., 0.],
            ),
            ThemeName::Midnight => Self::build(
                name,
                [
                    0x0B0D12, 0x12151C, 0x171B24, 0x252B38, 0x252B38, 0xE3E6EE, 0x9098A9, 0x7088FF,
                    0x0B0D12, 0x8CCB9E, 0xE7C46A, 0xF07A7A, 0x7FC6E8, 0x8095FF, 0xB7BDCB, 0xE3E6EE,
                    0x7D8496,
                ],
                "Inter Tight",
                "Geist Mono",
                [8., 6., 4.],
            ),
            ThemeName::Moss => Self::build(
                name,
                [
                    0x0C0E0C, 0x121512, 0x171B17, 0x262C26, 0x2E352E, 0xDCE2D8, 0x8F998C, 0xA6D1A4,
                    0x0C0E0C, 0xC2D98A, 0xE3C266, 0xE0806A, 0x8EB8D2, 0xA6D1A4, 0xC3CABF, 0xDCE2D8,
                    0x7E887B,
                ],
                "Geist",
                "Geist Mono",
                [4., 3., 2.],
            ),
            ThemeName::Dusk => Self::build(
                name,
                [
                    0x0F0D13, 0x16131B, 0x1C1822, 0x2C2734, 0x2C2734, 0xE7E2EE, 0xA098AB, 0xB7A4FF,
                    0x120F18, 0x92CDA6, 0xE9C878, 0xF08A9A, 0x8FB7F0, 0xB7A4FF, 0xCBC4D6, 0xE7E2EE,
                    0x8A8296,
                ],
                "Manrope",
                "JetBrains Mono",
                [12., 8., 6.],
            ),
        }
    }

    /// The theme with its font families swapped for ones that are actually installed
    /// (e.g. `JetBrains Mono` → `JetBrainsMono Nerd Font`). Use this, not `get`, at runtime.
    pub fn resolved(name: ThemeName, cx: &App) -> Self {
        let mut theme = Self::get(name);
        let installed = cx.text_system().all_font_names();
        let pick = |wanted: &SharedString, fallbacks: &[&str]| -> SharedString {
            let has = |n: &str| installed.iter().any(|f| f.eq_ignore_ascii_case(n));
            let squashed = wanted.replace(' ', "");
            std::iter::once(wanted.to_string())
                .chain([format!("{squashed} Nerd Font"), format!("{squashed} NF"), squashed.clone()])
                .chain(fallbacks.iter().map(|s| s.to_string()))
                .find(|n| has(n))
                .map(SharedString::from)
                .unwrap_or_else(|| wanted.clone())
        };
        theme.font_mono = pick(
            &theme.font_mono,
            &["JetBrains Mono", "JetBrainsMono Nerd Font", "Geist Mono", "IBM Plex Mono", "DejaVu Sans Mono", "Noto Sans Mono", "Liberation Mono"],
        );
        theme.font_sans = pick(
            &theme.font_sans,
            &["Inter", "Inter Variable", "Geist", "IBM Plex Sans", "Noto Sans", "DejaVu Sans", "Liberation Sans", "Cantarell"],
        );
        theme
    }

    /// `--shadow-overlay`: menus, dialogs, autocomplete.
    pub fn shadow_overlay(&self) -> Vec<BoxShadow> {
        vec![BoxShadow {
            color: hsla(0., 0., 0., 0.45),
            offset: point(px(0.), px(24.)),
            blur_radius: px(60.),
            spread_radius: px(0.),
        }]
    }

    /// `--shadow-quicklook`.
    pub fn shadow_quicklook(&self) -> Vec<BoxShadow> {
        vec![BoxShadow {
            color: hsla(0., 0., 0., 0.55),
            offset: point(px(0.), px(30.)),
            blur_radius: px(80.),
            spread_radius: px(0.),
        }]
    }

    /// `--shadow-flyout`.
    pub fn shadow_flyout(&self) -> Vec<BoxShadow> {
        vec![BoxShadow {
            color: hsla(0., 0., 0., 0.35),
            offset: point(px(0.), px(18.)),
            blur_radius: px(40.),
            spread_radius: px(0.),
        }]
    }

    /// `--scrim-modal`.
    pub fn scrim(&self) -> Hsla {
        hsla(0., 0., 0., 0.55)
    }
}

/// `--ease-standard` cubic-bezier(.2,.8,.2,1)
pub fn ease_standard(t: f32) -> f32 {
    cubic_bezier(0.2, 0.8, 0.2, 1.0, t)
}
/// `--ease-enter` cubic-bezier(.16,1,.3,1)
pub fn ease_enter(t: f32) -> f32 {
    cubic_bezier(0.16, 1.0, 0.3, 1.0, t)
}
/// `--ease-exit` cubic-bezier(.5,1,.89,1)
pub fn ease_exit(t: f32) -> f32 {
    cubic_bezier(0.5, 1.0, 0.89, 1.0, t)
}

/// CSS-style cubic-bezier easing: solve x(s)=t, return y(s).
pub fn cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32, t: f32) -> f32 {
    let t = t.clamp(0., 1.);
    let bez = |a: f32, b: f32, s: f32| {
        let inv = 1. - s;
        3. * inv * inv * s * a + 3. * inv * s * s * b + s * s * s
    };
    let (mut lo, mut hi) = (0f32, 1f32);
    let mut s = t;
    for _ in 0..24 {
        let x = bez(x1, x2, s);
        if (x - t).abs() < 1e-4 {
            break;
        }
        if x < t {
            lo = s;
        } else {
            hi = s;
        }
        s = (lo + hi) / 2.;
    }
    bez(y1, y2, s)
}
