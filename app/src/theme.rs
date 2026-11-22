//! Night watch: a baby monitor's parent unit in a dark nursery.
use gpui_kit::{
    App, Hsla,
    component::{Theme, ThemeConfig},
    rgb,
};
use std::rc::Rc;

/// Window background: the nursery at night.
pub fn night() -> Hsla {
    rgb(0x1d1e33).into()
}
/// Sidebar, the parent unit's housing.
pub fn housing() -> Hsla {
    rgb(0x17182a).into()
}
/// Dividers and unlit meter segments.
pub fn rail() -> Hsla {
    rgb(0x363956).into()
}
/// Primary text.
pub fn moon() -> Hsla {
    rgb(0xeee8d8).into()
}
/// Secondary text.
pub fn dusk() -> Hsla {
    rgb(0x9c9bba).into()
}
/// Lit meter segments, the primary action, waiting babysits.
pub fn glow() -> Hsla {
    rgb(0xf6c46a).into()
}
pub fn mint() -> Hsla {
    rgb(0x8ed7b2).into()
}
pub fn coral() -> Hsla {
    rgb(0xf28c7f).into()
}

const CONFIG: &str = r##"{
  "name": "Night watch",
  "mode": "dark",
  "font.family": "Avenir Next",
  "font.size": 14,
  "radius": 6,
  "radius.lg": 10,
  "shadow": false,
  "colors": {
    "background": "#1d1e33",
    "foreground": "#eee8d8",
    "border": "#363956",
    "input.border": "#43476a",
    "muted.background": "#25273f",
    "muted.foreground": "#9c9bba",
    "accent.background": "#2b2d48",
    "accent.foreground": "#eee8d8",
    "primary.background": "#f6c46a",
    "primary.foreground": "#1d1e33",
    "primary.hover.background": "#f9d38d",
    "primary.active.background": "#e9b355",
    "secondary.background": "#25273f",
    "secondary.foreground": "#eee8d8",
    "secondary.hover.background": "#2e3150",
    "secondary.active.background": "#363956",
    "popover.background": "#25273f",
    "popover.foreground": "#eee8d8",
    "list.active.background": "#363956",
    "list.active.border": "#f6c46a",
    "list.hover.background": "#2e3150",
    "ring": "#f6c46a",
    "caret": "#f6c46a",
    "selection.background": "#f6c46a55",
    "danger.background": "#f28c7f",
    "danger.foreground": "#1d1e33",
    "scrollbar.background": "#1d1e3300",
    "scrollbar.thumb.background": "#43476a",
    "base.yellow": "#f6c46a",
    "base.green": "#8ed7b2",
    "base.red": "#f28c7f"
  }
}"##;

pub fn install(cx: &mut App) {
    let config: ThemeConfig = serde_json::from_str(CONFIG).expect("theme config parses");
    Theme::update(cx, |theme| theme.apply_config(&Rc::new(config)));
}
