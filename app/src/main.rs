mod gh;
mod icons;
mod model;
mod notifications;
mod sitter;
mod store;
mod theme;
mod ui;

use gpui_kit::*;
use sitter::{Registry, RepoLocks};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

fn alert(message: &str) {
    let _ = std::process::Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "on run argv",
            "-e",
            "display alert (item 1 of argv) as warning",
            "-e",
            "end run",
            message,
        ])
        .status();
}

fn main() {
    if let Err(error) = run() {
        alert(&format!("baby-sitter could not start: {error:#}"));
    }
}

fn run() -> anyhow::Result<()> {
    let data = store::data_dir()?;
    let Some(_instance_lock) = store::lock_instance(&data)? else {
        alert("baby-sitter is already running");
        return Ok(());
    };
    let config = store::load(&data)?;
    let registry: Registry = Arc::new(Mutex::new(BTreeMap::new()));
    let locks: RepoLocks = Arc::new(Mutex::new(BTreeMap::new()));
    let signal_registry = registry.clone();
    let mut signals =
        signal_hook::iterator::Signals::new([libc::SIGINT, libc::SIGTERM, libc::SIGHUP])?;
    std::thread::spawn(move || {
        if signals.forever().next().is_some() {
            sitter::terminate_all(&signal_registry);
            std::process::exit(0);
        }
    });
    let env = gh::ChildEnv::new();
    application()
        .with_assets(icons::FilledAssets)
        .run(move |cx| {
            init(cx);
            theme::install(cx);
            set_dock_icon();
            let quit_registry = registry.clone();
            cx.on_app_quit(move |_| {
                sitter::terminate_all(&quit_registry);
                async {}
            })
            .detach();
            cx.on_window_closed(|cx, _| cx.quit()).detach();
            let bounds = Bounds::centered(None, size(px(1080.0), px(760.0)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(760.0), px(480.0))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Baby Sitter".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(16.0), px(18.0))),
                }),
                ..Default::default()
            };
            open_window(options, cx, move |window, cx| {
                cx.new(|cx| ui::AppView::new(data, config, env, registry, locks, window, cx))
            })
            .expect("open baby-sitter window");
            notifications::init();
            cx.activate(true);
        });
    Ok(())
}

/// Sets the Dock and app switcher icon when the binary runs outside the app bundle,
/// such as from `cargo run`. The bundle gets the same icon from its Info.plist.
#[cfg(target_os = "macos")]
fn set_dock_icon() {
    use objc2::{AllocAnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let data = NSData::with_bytes(include_bytes!("../assets/icon.svg"));
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        return;
    };
    // SAFETY: called on the main thread with a valid image.
    unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
}
#[cfg(not(target_os = "macos"))]
fn set_dock_icon() {}
#[cfg(test)]
pub(crate) mod test_support {
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    /// Temporary directory path no other test uses, even within the same nanosecond.
    pub(crate) fn unique_test_dir(prefix: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }
}
