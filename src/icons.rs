//! Stroke icons (24 grid, 1.6 stroke, round caps) from `assets/icons/*.svg`.
//! Tinted with the element's text colour: `icon(Icon::Folder, 14.).text_color(t.accent)`.

use gpui::{Svg, prelude::*, px, svg};

macro_rules! icons {
    ($($variant:ident => $file:literal),* $(,)?) => {
        #[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
        pub enum Icon { $($variant),* }
        impl Icon {
            pub fn path(self) -> &'static str {
                match self { $(Icon::$variant => concat!("icons/", $file, ".svg")),* }
            }
        }
    };
}

icons! {
    File => "file", Folder => "folder", Sliders => "sliders", Image => "image", Code => "code",
    Pin => "pin", Grid => "grid", Clipboard => "clipboard", Trash => "trash",
    ChevronRight => "chevron_right", ChevronLeft => "chevron_left", ChevronDown => "chevron_down",
    ChevronUp => "chevron_up", Search => "search", Minus => "minus", Plus => "plus", List => "list",
    Pause => "pause", Play => "play", Music => "music", Eject => "eject", Eye => "eye",
    Fullscreen => "fullscreen", Close => "close", Globe => "globe", Terminal => "terminal",
    Home => "home", Disk => "disk", Archive => "archive", Link => "link", Download => "download",
    Tag => "tag", Copy => "copy", Move => "move", Pencil => "pencil", Info => "info",
    Check => "check", Video => "video", Refresh => "refresh", GitBranch => "git_branch",
    Lock => "lock",
}

/// An icon element of `size` px. Colour it with `.text_color(..)`; it does not inherit.
pub fn icon(icon: Icon, size: f32) -> Svg {
    svg().path(icon.path()).size(px(size)).flex_none()
}
