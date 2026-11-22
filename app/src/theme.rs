//! Graphite: a neutral dark grey with no blue tint and a teal accent.
use gpui_kit::{
    App, Hsla,
    component::{Theme, ThemeConfig},
    rgb,
};
use std::rc::Rc;

/// Main content background.
pub fn surface() -> Hsla {
    rgb(0x26282c).into()
}
/// Sidebar and organization header rows.
pub fn panel() -> Hsla {
    rgb(0x1f2124).into()
}
/// Hovered sidebar items.
pub fn raised() -> Hsla {
    rgb(0x2c2f34).into()
}
/// Dividers and unlit meter segments.
pub fn divider() -> Hsla {
    rgb(0x393c42).into()
}
/// Primary text.
pub fn ink() -> Hsla {
    rgb(0xecece8).into()
}
/// Secondary text.
pub fn muted() -> Hsla {
    rgb(0xa3a6ad).into()
}
/// Lit meter segments, selection, start buttons, waiting babysits.
pub fn accent() -> Hsla {
    rgb(0x3fb0a3).into()
}
pub fn success() -> Hsla {
    rgb(0x7cc77a).into()
}
pub fn warning() -> Hsla {
    rgb(0xe3b25c).into()
}
pub fn danger() -> Hsla {
    rgb(0xe8716a).into()
}

const CONFIG: &str = r##"{
  "name": "Graphite",
  "mode": "dark",
  "font.family": "Avenir Next",
  "font.size": 14,
  "radius": 6,
  "radius.lg": 10,
  "shadow": false,
  "colors": {
    "background": "#26282c",
    "foreground": "#ecece8",
    "border": "#393c42",
    "input.border": "#474b52",
    "muted.background": "#2c2f34",
    "muted.foreground": "#a3a6ad",
    "accent.background": "#33363c",
    "accent.foreground": "#ecece8",
    "primary.background": "#3fb0a3",
    "primary.foreground": "#1f2124",
    "primary.hover.background": "#57c0b4",
    "primary.active.background": "#33978b",
    "secondary.background": "#2c2f34",
    "secondary.foreground": "#ecece8",
    "secondary.hover.background": "#33363c",
    "secondary.active.background": "#393c42",
    "popover.background": "#2c2f34",
    "popover.foreground": "#ecece8",
    "list.active.background": "#393c42",
    "list.active.border": "#3fb0a3",
    "list.hover.background": "#33363c",
    "ring": "#3fb0a3",
    "caret": "#3fb0a3",
    "selection.background": "#3fb0a355",
    "danger.background": "#e8716a",
    "danger.foreground": "#1f2124",
    "scrollbar.background": "#26282c00",
    "scrollbar.thumb.background": "#474b52",
    "base.yellow": "#e3b25c",
    "base.green": "#7cc77a",
    "base.red": "#e8716a"
  }
}"##;

pub fn install(cx: &mut App) {
    let config: ThemeConfig = serde_json::from_str(CONFIG).expect("theme config parses");
    Theme::update(cx, |theme| theme.apply_config(&Rc::new(config)));
}
