//! Liveness guards for the `main` and `settings` windows (#410).
//!
//! [`super::webview_health`] can tell whether a window still hosts a live
//! WebView2 instance; this module decides what to do when it does not, for
//! the two windows that are shown from Rust rather than revealed by the
//! frontend.
//!
//! The recovery deliberately never runs inline. `flyout_window::open_or_focus`
//! can destroy and rebuild on the spot because it is documented as callable
//! only from an async context, but these two windows are not so lucky:
//! `tray_bridge::handle_menu_event` calls both `shell::transition_to_target`
//! and `settings_window::open_or_focus` synchronously on the menu-event
//! thread, and `commands::set_surface_mode` is a *sync* Tauri command. Both
//! therefore reach us on the main thread, where
//! `webview_health::destroy_and_release` would block the very event loop it
//! is waiting on to process `Destroyed` — a guaranteed two-second stall
//! followed by a failed rebuild.
//!
//! So a guard that finds a dead window hands the destroy + rebuild to a
//! background thread and tells its caller to abandon this attempt. The
//! rebuild re-issues the original request once the window is healthy again,
//! so the click that hit the dead window still ends up doing what the user
//! asked — just a beat later.
//!
//! The same background rebuilds serve [`super::webview_lifecycle`], which
//! learns about a dead webview from WebView2 itself (inside a COM callback
//! on the UI thread, so it is under the same must-not-block rule) rather
//! than from a probe at show time. The `*_after_loss` entry points are its
//! side of the contract.

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Manager, WebviewWindow};

use crate::surface::SurfaceMode;
use crate::surface_target::SurfaceTarget;

use super::webview_health;

pub(crate) const MAIN_LABEL: &str = "main";

/// Set while a rebuild thread is between `destroy()` and a rebuilt window,
/// so a burst of tray clicks queues one recovery rather than one per click.
static MAIN_REBUILD_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static SETTINGS_REBUILD_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static FLYOUT_REBUILD_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// What a caller should do with a window it is about to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuardAction {
    /// The window is there and its webview is live — carry on.
    Proceed,
    /// The window needs rebuilding and this caller should start it.
    Rebuild,
    /// A rebuild is already running; abandon this attempt without starting
    /// a second one.
    Waiting,
}

/// Decide what to do about a window, from facts a caller can cheaply gather.
///
/// Split out from the Win32 probe and the thread spawning so the policy is
/// testable on its own: a missing window is as recoverable as a dead one
/// (Tauri drops a window from its label map once `Destroyed` is processed,
/// so an interrupted earlier recovery leaves exactly that shape), and an
/// in-flight rebuild always wins so concurrent callers cannot stack up
/// destroy/build cycles on the same label.
pub(crate) fn guard_action(exists: bool, alive: bool, rebuild_in_flight: bool) -> GuardAction {
    if rebuild_in_flight {
        return GuardAction::Waiting;
    }
    if exists && alive {
        return GuardAction::Proceed;
    }
    GuardAction::Rebuild
}

/// The transition to replay once `main` has been rebuilt.
#[derive(Debug, Clone)]
pub(crate) struct MainRequest {
    pub mode: SurfaceMode,
    pub target: SurfaceTarget,
    pub position: Option<(i32, i32)>,
    /// Replay through `reopen_to_target` rather than `transition_to_target`.
    /// Needed whenever the surface state already reads `mode`: a plain
    /// transition to the mode the state is in resolves as a no-op, and the
    /// freshly rebuilt (hidden) window would never be shown.
    pub reopen: bool,
}

/// `main`'s webview was reported dead by WebView2 itself: rebuild it now,
/// hidden, exactly as at startup. When it was on screen, replay the current
/// surface at `position` so it comes back where the user had it.
pub(crate) fn recover_main_after_loss(app: &AppHandle, replay: bool, position: Option<(i32, i32)>) {
    let request = replay
        .then(|| {
            let state = app.try_state::<std::sync::Mutex<crate::state::AppState>>()?;
            let snapshot = {
                let guard = state.lock().unwrap_or_else(|error| error.into_inner());
                super::transition::current_surface_snapshot(&guard)
            };
            tracing::debug!(
                mode = ?snapshot.mode,
                target = ?snapshot.target,
                "window_recovery: surface to replay after the main rebuild"
            );
            (snapshot.mode != SurfaceMode::Hidden).then_some(MainRequest {
                mode: snapshot.mode,
                target: snapshot.target,
                position,
                // The state still says we are in this mode, so only a
                // forced same-mode apply will show the rebuilt window.
                reopen: true,
            })
        })
        .flatten();
    dispatch_main_rebuild(app, request);
}

/// Resolve the `main` window, or start recovering it.
///
/// `Some(window)` means the window is live and the caller may proceed with
/// its transition. `None` means the caller must abandon this attempt: a
/// rebuild is either already running or has just been dispatched, and will
/// re-issue `request` itself once `main` is healthy.
pub(crate) fn resolve_live_main(
    app: &AppHandle,
    request: Option<MainRequest>,
) -> Option<WebviewWindow> {
    let window = app.get_webview_window(MAIN_LABEL);
    let alive = window
        .as_ref()
        .map(webview_health::webview_is_alive)
        .unwrap_or(false);

    match guard_action(
        window.is_some(),
        alive,
        MAIN_REBUILD_IN_FLIGHT.load(Ordering::SeqCst),
    ) {
        GuardAction::Proceed => window,
        GuardAction::Waiting => {
            tracing::debug!("window_recovery: main rebuild already in flight; ignoring this open");
            None
        }
        GuardAction::Rebuild => {
            tracing::warn!(
                label = MAIN_LABEL,
                "window_recovery: main window is not usable; rebuilding it (#410)"
            );
            dispatch_main_rebuild(app, request);
            None
        }
    }
}

/// Destroy and rebuild `main` on a background thread, then replay `request`.
///
/// Spawns rather than blocking because the caller may be the main thread;
/// `std::thread::spawn` (not the async runtime) matches the existing
/// precedent in `transition.rs`'s startup reveal fallback and keeps the
/// blocking label wait off a tokio worker.
fn dispatch_main_rebuild(app: &AppHandle, request: Option<MainRequest>) {
    if MAIN_REBUILD_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    let _ = std::thread::spawn(move || {
        let result = rebuild_main(&app);
        MAIN_REBUILD_IN_FLIGHT.store(false, Ordering::SeqCst);

        match result {
            Ok(()) => {
                tracing::info!(label = MAIN_LABEL, "window_recovery: main window rebuilt");
                if let Some(request) = request {
                    replay_on_main_thread(&app, request);
                }
            }
            Err(error) => {
                tracing::error!(
                    %error,
                    label = MAIN_LABEL,
                    "window_recovery: could not rebuild the main window"
                );
            }
        }
    });
}

/// Replay `request` against the rebuilt `main` — on the main thread, never
/// on the rebuild thread.
///
/// A transition holds `SHELL_TRANSITION_SERIAL` while it calls window
/// getters, and the main thread's window-event handler takes that same lock
/// (`hide_to_tray_if_current` on `Focused(false)`). Run from a background
/// thread, the replay can therefore deadlock: it holds the lock and waits
/// for the main thread to answer a getter, while the main thread sits in an
/// event handler waiting for the lock. That is not hypothetical — a freshly
/// built window receives a `Focused(true)`/`Focused(false)` pair right after
/// `build()`, so the race was hit on every rebuild of a visible `main`
/// (observed 2026-09-11). On the main thread every window call takes the
/// runtime's direct path and the event handler cannot run concurrently, which
/// is exactly why the tray and menu paths run their transitions there.
///
/// The window is healthy by now, so this pass takes the normal path —
/// `resolve_live_main` returns it and no further rebuild is dispatched.
fn replay_on_main_thread(app: &AppHandle, request: MainRequest) {
    let handle = app.clone();
    let dispatched = app.run_on_main_thread(move || {
        let replay = if request.reopen {
            super::reopen_to_target
        } else {
            super::transition_to_target
        };
        if let Err(error) = replay(&handle, request.mode, request.target, request.position) {
            tracing::warn!(
                %error,
                "window_recovery: replaying the transition after rebuild failed"
            );
        }
    });
    if let Err(error) = dispatched {
        tracing::warn!(
            %error,
            "window_recovery: could not dispatch the post-rebuild replay to the main thread"
        );
    }
}

/// Rebuild `main` from the same `tauri.conf.json` entry the first build uses.
///
/// `main` is declared in the config rather than built in code, so the only
/// faithful recipe is `WebviewWindowBuilder::from_config` over that entry —
/// which also means the rebuilt window inherits `"visible": false` and the
/// rest of its declared properties, exactly as at startup. The two things
/// `setup` does to it afterwards (`force_dark_caption`, then `hide()`) are
/// repeated here so a recovered window is indistinguishable from a
/// freshly-launched one, and in particular can never flash a frame of its
/// own before the surface machinery decides to show it.
fn rebuild_main(app: &AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(MAIN_LABEL) {
        webview_health::destroy_and_release(app, &window)?;
    }

    let config = app
        .config()
        .app
        .windows
        .first()
        .cloned()
        .ok_or_else(|| "no window is declared in tauri.conf.json".to_string())?;

    let window = tauri::WebviewWindowBuilder::from_config(app, &config)
        .map_err(|error| error.to_string())?
        .build()
        .map_err(|error| error.to_string())?;
    super::webview_lifecycle::watch(app, &window);

    super::dwm::force_dark_caption(&window);
    window.hide().map_err(|error| error.to_string())?;
    Ok(())
}

/// Whether the existing `settings` window may be shown as-is.
///
/// `false` means the caller must return without showing anything: either a
/// rebuild is already running, or one has just been dispatched that will
/// re-open Settings on `tab` once the label is free.
pub(crate) fn settings_window_is_usable(
    app: &AppHandle,
    window: &WebviewWindow,
    tab: &str,
) -> bool {
    match guard_action(
        true,
        webview_health::webview_is_alive(window),
        SETTINGS_REBUILD_IN_FLIGHT.load(Ordering::SeqCst),
    ) {
        GuardAction::Proceed => true,
        GuardAction::Waiting => {
            tracing::debug!(
                "window_recovery: settings rebuild already in flight; ignoring this open"
            );
            false
        }
        GuardAction::Rebuild => {
            tracing::warn!(
                label = window.label(),
                "window_recovery: settings window is not usable; rebuilding it (#410)"
            );
            dispatch_settings_rebuild(app, Some(tab.to_string()));
            false
        }
    }
}

/// `settings` lost its webview: tear it down now. When it was on screen,
/// reopen it on the default tab — the tab it was showing died with the
/// webview, and the default is where the user lands from the tray too.
pub(crate) fn recover_settings_after_loss(app: &AppHandle, reopen: bool) {
    let tab = reopen.then(
        || match SurfaceTarget::default_for_mode(SurfaceMode::Settings) {
            SurfaceTarget::Settings { tab } => tab,
            _ => "general".to_string(),
        },
    );
    dispatch_settings_rebuild(app, tab);
}

/// Destroy the dead `settings` window on a background thread, then — when
/// `reopen_tab` is given — let `settings_window::open_or_focus` build a
/// fresh one on that tab.
///
/// Re-entering `open_or_focus` rather than duplicating its builder keeps the
/// window's size/geometry/theme recipe in one place; once the label is
/// released that call takes the first-build branch, which is precisely "the
/// same path the first build uses". Without a tab the window is simply
/// gone, and the next open builds it on demand.
fn dispatch_settings_rebuild(app: &AppHandle, reopen_tab: Option<String>) {
    if SETTINGS_REBUILD_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    let _ = std::thread::spawn(move || {
        let destroyed = match app.get_webview_window(super::settings_window::SETTINGS_LABEL) {
            Some(window) => webview_health::destroy_and_release(&app, &window),
            None => Ok(()),
        };
        SETTINGS_REBUILD_IN_FLIGHT.store(false, Ordering::SeqCst);

        let tab = match (destroyed, reopen_tab) {
            (Err(error), _) => {
                tracing::error!(
                    %error,
                    "window_recovery: could not release the settings window for rebuild"
                );
                return;
            }
            (Ok(()), None) => {
                tracing::info!(
                    label = super::settings_window::SETTINGS_LABEL,
                    "window_recovery: settings window torn down; it will be rebuilt on next open"
                );
                return;
            }
            (Ok(()), Some(tab)) => tab,
        };
        match super::settings_window::open_or_focus(&app, &tab) {
            Ok(()) => tracing::info!(
                label = super::settings_window::SETTINGS_LABEL,
                "window_recovery: settings window rebuilt"
            ),
            Err(error) => tracing::warn!(
                %error,
                "window_recovery: reopening Settings after rebuild failed"
            ),
        }
    });
}

/// The flyout lost its webview: tear it down now and, when it was on
/// screen, reopen it.
///
/// `flyout_window::open_or_focus` is the first-build path, so the reopened
/// window is built `visible(false)` and revealed by the frontend after its
/// first layout pass — the same handshake as any other open, which is what
/// keeps the recovery from flashing a blank frame. It re-anchors above the
/// tray on its own, so no position is carried over. `open_or_focus` may
/// block on `build()`, hence the background thread.
pub(crate) fn recover_flyout_after_loss(app: &AppHandle, reopen: bool) {
    if FLYOUT_REBUILD_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    let _ = std::thread::spawn(move || {
        let destroyed = match app.get_webview_window(super::flyout_window::FLYOUT_LABEL) {
            Some(window) => webview_health::destroy_and_release(&app, &window),
            None => Ok(()),
        };
        FLYOUT_REBUILD_IN_FLIGHT.store(false, Ordering::SeqCst);

        match destroyed {
            Err(error) => tracing::error!(
                %error,
                "window_recovery: could not release the flyout window for rebuild"
            ),
            Ok(()) if !reopen => tracing::info!(
                label = super::flyout_window::FLYOUT_LABEL,
                "window_recovery: flyout torn down; it will be rebuilt on next open"
            ),
            Ok(()) => match super::flyout_window::open_or_focus(&app, None) {
                Ok(()) => tracing::info!(
                    label = super::flyout_window::FLYOUT_LABEL,
                    "window_recovery: flyout rebuilt"
                ),
                Err(error) => tracing::warn!(
                    %error,
                    "window_recovery: reopening the flyout after rebuild failed"
                ),
            },
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_live_window_is_shown_as_is() {
        assert_eq!(guard_action(true, true, false), GuardAction::Proceed);
    }

    #[test]
    fn a_dead_window_is_rebuilt() {
        assert_eq!(guard_action(true, false, false), GuardAction::Rebuild);
    }

    #[test]
    fn a_missing_window_is_rebuilt_too() {
        // An earlier recovery that was interrupted after `destroy()` leaves
        // no window under the label at all; that is recoverable, not fatal.
        assert_eq!(guard_action(false, false, false), GuardAction::Rebuild);
    }

    #[test]
    fn an_in_flight_rebuild_wins_over_every_other_shape() {
        // Concurrent tray clicks must not stack destroy/build cycles on one
        // label — including the case where the half-rebuilt window already
        // probes as alive.
        assert_eq!(guard_action(true, false, true), GuardAction::Waiting);
        assert_eq!(guard_action(false, false, true), GuardAction::Waiting);
        assert_eq!(guard_action(true, true, true), GuardAction::Waiting);
    }
}
