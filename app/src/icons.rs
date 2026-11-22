//! Filled icons. GPUI Kit ships the outline Lucide set, and both this app and
//! gpui-component request icons by their Lucide path. This source answers those
//! paths with Phosphor fill icons and GitHub's Octicons mark (both MIT, licenses
//! in `assets/icons/`) and falls back to the Lucide catalog for anything not listed.

use gpui_kit::*;
use std::borrow::Cow;

macro_rules! filled {
    ($($lucide:literal => $file:literal,)*) => {
        const FILLED: &[(&str, &[u8])] = &[
            $((
                concat!("icons/", $lucide, ".svg"),
                include_bytes!(concat!("../assets/icons/", $file)),
            ),)*
        ];
    };
}

filled! {
    "book-marked" => "book-bookmark-fill.svg",
    "building-2" => "buildings-fill.svg",
    "check" => "check-circle-fill.svg",
    "chevron-down" => "caret-down-fill.svg",
    "chevron-left" => "caret-left-fill.svg",
    "chevron-right" => "caret-right-fill.svg",
    "close" => "x-circle-fill.svg",
    "folder-git-2" => "folder-simple-fill.svg",
    "folder-open" => "folder-open-fill.svg",
    "git-branch" => "git-branch-fill.svg",
    "git-merge" => "git-merge-fill.svg",
    "git-pull-request-arrow" => "git-pull-request-fill.svg",
    "git-pull-request-draft" => "pencil-simple-fill.svg",
    "github" => "mark-github-24.svg",
    "inbox" => "tray-fill.svg",
    "list-checks" => "list-checks-fill.svg",
    "message-square" => "chat-centered-text-fill.svg",
    "minus" => "minus-circle-fill.svg",
    "play" => "play-fill.svg",
    "plus" => "plus-circle-fill.svg",
    "refresh-cw" => "arrows-clockwise-fill.svg",
    "settings" => "gear-six-fill.svg",
    "square" => "stop-fill.svg",
    "timer" => "timer-fill.svg",
    "x" => "x-circle-fill.svg",
}

pub struct FilledAssets;

impl AssetSource for FilledAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match FILLED.iter().find(|(lucide, _)| *lucide == path) {
            Some((_, svg)) => Ok(Some(Cow::Borrowed(svg))),
            None => assets::AllAssets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        assets::AllAssets.list(path)
    }
}

#[cfg(test)]
mod tests {
    use super::FILLED;
    use gpui_kit::{AssetSource, assets};

    /// Each overridden path must exist in the Lucide catalog; a typo would leave
    /// that icon in outline with no error.
    #[test]
    fn every_filled_path_replaces_a_catalog_icon() {
        for (path, _) in FILLED {
            assert!(
                matches!(assets::AllAssets.load(path), Ok(Some(_))),
                "{path} is not a Lucide catalog path"
            );
        }
    }
}
