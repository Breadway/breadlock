//! Watches the per-output palette / theme directories and notifies the event
//! loop when pywal regenerates them.
//!
//! `state::AppState` caches the resolved [`breadlock_ui::theme::Palette`] per
//! output name (`output_palettes`). Without this watcher that cache would
//! never refresh; with it, a wallpaper/palette change invalidates the cache
//! and triggers one redraw — instead of the old behaviour of re-reading and
//! re-parsing `palettes/<output>.json` from disk on every rendered frame
//! (60fps during animations, plus the 1s clock tick, times N monitors).
//!
//! Mirrors [`crate::status`] / [`crate::auth`]: a `notify` watcher on its own
//! thread posts through a `calloop::channel` so the main loop just gets an
//! ordinary event. If the watcher can't be armed (missing dir it can't
//! create, inotify exhausted, …) [`register`] returns `None` and the caller
//! falls back to the uncached per-frame read, so correctness never regresses.

use notify::{EventKind, RecursiveMode, Watcher};
use smithay_client_toolkit::reexports::calloop::channel;
use smithay_client_toolkit::reexports::calloop::LoopHandle;
use std::path::Path;

/// Keeps the `notify` watcher (and its background thread) alive. Dropping it
/// stops the watch.
pub struct ThemeWatch {
    _watcher: notify::RecommendedWatcher,
}

/// Arm the watcher. `on_change` runs on the event loop whenever a palette or
/// theme file under the runtime `bread` dir changes. Returns `None` if no
/// directory could be watched.
pub fn register<Data: 'static>(
    loop_handle: &LoopHandle<'static, Data>,
    mut on_change: impl FnMut(&mut Data) + 'static,
) -> Option<ThemeWatch> {
    let (tx, rx) = channel::channel::<()>();
    loop_handle
        .insert_source(rx, move |event, _, data| {
            if let channel::Event::Msg(()) = event {
                on_change(data);
            }
        })
        .ok()?;

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            if is_palette_change(&event) {
                // A closed receiver just means the locker is exiting.
                let _ = tx.send(());
            }
        }
    })
    .ok()?;

    // `palettes/` and `themes/` share a parent (`$XDG_RUNTIME_DIR/bread`);
    // watch both non-recursively. `bread-theme generate-output` writes them
    // at login, but create them here too so the watch arms even if breadlock
    // starts first.
    let dirs = [
        breadlock_ui::theme::palettes_dir(),
        breadlock_ui::theme::themes_dir(),
    ];
    let mut armed = false;
    for dir in &dirs {
        let _ = std::fs::create_dir_all(dir);
        if watcher.watch(dir, RecursiveMode::NonRecursive).is_ok() {
            armed = true;
        } else {
            tracing::warn!(dir = %dir.display(), "breadlock theme watch: could not watch directory");
        }
    }
    if !armed {
        return None;
    }
    Some(ThemeWatch { _watcher: watcher })
}

/// True when an event touches a generated palette (`*.json`) or theme
/// (`*.css`) file — not the `.<name>.tmp.<pid>` scratch files that
/// `bread-theme`'s atomic writes create and immediately rename away.
fn is_palette_change(event: &notify::Event) -> bool {
    if !matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any
    ) {
        return false;
    }
    event.paths.iter().any(|p| {
        matches!(
            Path::new(p).extension().and_then(|e| e.to_str()),
            Some("json") | Some("css")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};
    use std::path::PathBuf;

    fn event(kind: EventKind, path: &str) -> notify::Event {
        notify::Event {
            kind,
            paths: vec![PathBuf::from(path)],
            attrs: Default::default(),
        }
    }

    #[test]
    fn accepts_palette_and_theme_writes() {
        assert!(is_palette_change(&event(
            EventKind::Create(CreateKind::File),
            "/run/user/1000/bread/palettes/DP-1.json",
        )));
        assert!(is_palette_change(&event(
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            "/run/user/1000/bread/themes/DP-1.css",
        )));
        assert!(is_palette_change(&event(
            EventKind::Remove(RemoveKind::File),
            "/run/user/1000/bread/palettes/HDMI-A-1.json",
        )));
    }

    #[test]
    fn ignores_atomic_write_scratch_files() {
        // `bread-theme` writes `.<name>.tmp.<pid>` then renames it into place;
        // the rename to the real name is what we react to, not the tmp churn.
        assert!(!is_palette_change(&event(
            EventKind::Create(CreateKind::File),
            "/run/user/1000/bread/palettes/.DP-1.json.tmp.4242",
        )));
    }

    #[test]
    fn ignores_access_events() {
        assert!(!is_palette_change(&event(
            EventKind::Access(notify::event::AccessKind::Read),
            "/run/user/1000/bread/palettes/DP-1.json",
        )));
    }
}
