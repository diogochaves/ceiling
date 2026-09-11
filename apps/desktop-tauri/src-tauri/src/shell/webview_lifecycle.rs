//! Recover from WebView2 process failures as they happen (#410).
//!
//! [`super::webview_health`] and [`super::window_recovery`] catch a dead
//! webview *lazily*: the next time something tries to show the window, the
//! probe notices the missing WebView2 child and the window is rebuilt. That
//! covers hidden windows well, but a window that is on screen when its
//! browser process dies just sits there as a transparent frame until the
//! user happens to toggle it.
//!
//! This module closes that gap by subscribing to WebView2's own
//! `ProcessFailed` event on every window the app builds. When WebView2
//! reports a failure it cannot recover from on its own, the affected window
//! is torn down immediately and — if it was visible — rebuilt and shown
//! again through the same first-build path it came from, so the recovery
//! never paints a frame of its own (the flyout keeps its `visible(false)` →
//! frontend-reveal handshake, `main` inherits `"visible": false` from the
//! config and is shown by the replayed surface transition).
//!
//! Tearing the dead window down right away is also what lets the lazy
//! guards stay simple: a destroyed window vanishes from Tauri's label map,
//! so the next open takes the plain first-build path with no probe involved.
//! No extra "this window is dead" state has to be kept in sync.
//!
//! Nothing else in the stack surfaces `ProcessFailed` — not wry, not
//! tauri-runtime-wry, not tauri — so this is the one place the app talks to
//! WebView2's COM interfaces directly, through the `webview2-com` crate that
//! wry already depends on.
//!
//! **Threading.** WebView2 delivers `ProcessFailed` as a COM callback on the
//! UI thread, i.e. inside the very event loop that has to process a window's
//! `Destroyed` event. The handler therefore only reads what it needs from the
//! window and hands the destroy + rebuild to a background thread via
//! `window_recovery`; it never blocks, and never destroys a window inline.

use tauri::{AppHandle, WebviewWindow};

/// What the handler does about a window whose webview reported a failure.
///
/// Pure so the label → action policy is testable away from COM. `main` is
/// always rebuilt (hidden, as at startup) because it is the app's primary
/// surface and a hidden rebuild makes the next tray click instant; the two
/// windows that are built on demand are only rebuilt when the user was
/// looking at them, otherwise their next open builds them fresh anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryPlan {
    /// Rebuild `main` hidden; replay the current surface if it was showing.
    RebuildMain { replay: bool },
    /// Destroy `settings`; reopen it on its default tab if it was showing.
    RebuildSettings { reopen: bool },
    /// Destroy the flyout; reopen it (via the reveal handshake) if it was showing.
    RebuildFlyout { reopen: bool },
    /// A window this module has no rebuild recipe for — log and leave it.
    Unsupported,
}

pub(crate) fn recovery_plan(label: &str, was_visible: bool) -> RecoveryPlan {
    match label {
        super::window_recovery::MAIN_LABEL => RecoveryPlan::RebuildMain {
            replay: was_visible,
        },
        super::settings_window::SETTINGS_LABEL => RecoveryPlan::RebuildSettings {
            reopen: was_visible,
        },
        super::flyout_window::FLYOUT_LABEL => RecoveryPlan::RebuildFlyout {
            reopen: was_visible,
        },
        _ => RecoveryPlan::Unsupported,
    }
}

/// Subscribe `window` to WebView2's `ProcessFailed` event.
///
/// Call once per built window, after `build()`; a rebuilt window gets a new
/// WebView2 instance and needs its own subscription. Registration is
/// best-effort: failing to subscribe is logged and the lazy guards still
/// apply. The subscription lives as long as the WebView2 instance does, so
/// the registration token is deliberately not kept — there is nothing to
/// unsubscribe from once the webview is gone.
#[cfg(windows)]
pub(crate) fn watch(app: &AppHandle, window: &WebviewWindow) {
    let label = window.label().to_string();
    let app = app.clone();
    let result = window.with_webview(move |webview| {
        let handler_label = label.clone();
        let handler_app = app.clone();
        match win32::subscribe(&webview, move |kind| {
            on_process_failed(&handler_app, &handler_label, kind);
        }) {
            Ok(()) => tracing::debug!(
                label,
                "webview_lifecycle: subscribed to WebView2 ProcessFailed"
            ),
            Err(error) => tracing::warn!(
                %error,
                label,
                "webview_lifecycle: could not subscribe to WebView2 ProcessFailed; \
                 relying on the liveness guards"
            ),
        }
    });
    if let Err(error) = result {
        tracing::warn!(
            %error,
            label = window.label(),
            "webview_lifecycle: could not reach the platform webview to subscribe"
        );
    }
}

#[cfg(not(windows))]
pub(crate) fn watch(_app: &AppHandle, _window: &WebviewWindow) {}

/// Runs inside the COM callback on the UI thread: read, log, dispatch.
#[cfg(windows)]
fn on_process_failed(app: &AppHandle, label: &str, kind: win32::FailedKind) {
    use tauri::Manager;

    let kind_name = win32::describe(kind);
    // The warn is part of the contract: it is how an organic WebView2 death
    // — previously invisible until a window came back blank — shows up in
    // the log.
    if !win32::needs_rebuild(kind) {
        tracing::warn!(
            label,
            kind = kind_name,
            "webview_lifecycle: WebView2 reported a failed process; no rebuild needed"
        );
        return;
    }
    tracing::warn!(
        label,
        kind = kind_name,
        "webview_lifecycle: WebView2 process failed; rebuilding the window (#410)"
    );

    // The native window is still there (only its WebView2 child died), so
    // these cheap reads are safe and, on the main thread, synchronous.
    let window = app.get_webview_window(label);
    let was_visible = window
        .as_ref()
        .and_then(|window| window.is_visible().ok())
        .unwrap_or(false);
    let position = window
        .as_ref()
        .and_then(|window| window.outer_position().ok())
        .map(|position| (position.x, position.y));
    let plan = recovery_plan(label, was_visible);
    tracing::debug!(
        label,
        exists = window.is_some(),
        was_visible,
        ?position,
        ?plan,
        "webview_lifecycle: dispatching recovery"
    );

    use super::window_recovery;
    match plan {
        RecoveryPlan::RebuildMain { replay } => {
            window_recovery::recover_main_after_loss(app, replay, position);
        }
        RecoveryPlan::RebuildSettings { reopen } => {
            window_recovery::recover_settings_after_loss(app, reopen);
        }
        RecoveryPlan::RebuildFlyout { reopen } => {
            window_recovery::recover_flyout_after_loss(app, reopen);
        }
        RecoveryPlan::Unsupported => {
            tracing::warn!(
                label,
                "webview_lifecycle: no rebuild recipe for this window; leaving it as is"
            );
        }
    }
}

/// The COM-facing half: the only code that names WebView2 interfaces.
#[cfg(windows)]
mod win32 {
    use tauri::webview::PlatformWebview;
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_PROCESS_FAILED_KIND, COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED,
        COREWEBVIEW2_PROCESS_FAILED_KIND_FRAME_RENDER_PROCESS_EXITED,
        COREWEBVIEW2_PROCESS_FAILED_KIND_GPU_PROCESS_EXITED,
        COREWEBVIEW2_PROCESS_FAILED_KIND_PPAPI_BROKER_PROCESS_EXITED,
        COREWEBVIEW2_PROCESS_FAILED_KIND_PPAPI_PLUGIN_PROCESS_EXITED,
        COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED,
        COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE,
        COREWEBVIEW2_PROCESS_FAILED_KIND_SANDBOX_HELPER_PROCESS_EXITED,
        COREWEBVIEW2_PROCESS_FAILED_KIND_UNKNOWN_PROCESS_EXITED,
        COREWEBVIEW2_PROCESS_FAILED_KIND_UTILITY_PROCESS_EXITED,
    };
    use webview2_com::ProcessFailedEventHandler;

    pub(super) type FailedKind = COREWEBVIEW2_PROCESS_FAILED_KIND;

    /// Whether a failure of this kind leaves the webview unable to render
    /// again without being recreated.
    ///
    /// Only the two kinds WebView2 documents as "the WebView is unusable"
    /// trigger a rebuild. `RenderProcessUnresponsive` is a hang WebView2 may
    /// still recover from, `FrameRenderProcessExited` affects one iframe,
    /// and the helper-process kinds (GPU, utility, sandbox, plugin) are
    /// restarted by WebView2 itself. Rebuilding on any of those would trade
    /// a transient glitch for a lost window, so they are logged only.
    pub(super) fn needs_rebuild(kind: FailedKind) -> bool {
        matches!(
            kind,
            COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED
                | COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED
        )
    }

    /// Human-readable name for the log line; unknown values are printed raw
    /// so a newer WebView2 runtime can never make the warn less informative.
    pub(super) fn describe(kind: FailedKind) -> String {
        let name = match kind {
            COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED => "BrowserProcessExited",
            COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED => "RenderProcessExited",
            COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE => {
                "RenderProcessUnresponsive"
            }
            COREWEBVIEW2_PROCESS_FAILED_KIND_FRAME_RENDER_PROCESS_EXITED => {
                "FrameRenderProcessExited"
            }
            COREWEBVIEW2_PROCESS_FAILED_KIND_UTILITY_PROCESS_EXITED => "UtilityProcessExited",
            COREWEBVIEW2_PROCESS_FAILED_KIND_SANDBOX_HELPER_PROCESS_EXITED => {
                "SandboxHelperProcessExited"
            }
            COREWEBVIEW2_PROCESS_FAILED_KIND_GPU_PROCESS_EXITED => "GpuProcessExited",
            COREWEBVIEW2_PROCESS_FAILED_KIND_PPAPI_PLUGIN_PROCESS_EXITED => {
                "PpapiPluginProcessExited"
            }
            COREWEBVIEW2_PROCESS_FAILED_KIND_PPAPI_BROKER_PROCESS_EXITED => {
                "PpapiBrokerProcessExited"
            }
            COREWEBVIEW2_PROCESS_FAILED_KIND_UNKNOWN_PROCESS_EXITED => "UnknownProcessExited",
            other => return format!("Unknown({})", other.0),
        };
        name.to_string()
    }

    /// Register `on_failed` for `ProcessFailed` on the webview's core.
    ///
    /// Must run on the UI thread (`WebviewWindow::with_webview` guarantees
    /// that). The handler is invoked by WebView2 on that same thread; it
    /// receives only the failure kind so nothing COM-flavoured leaks out.
    pub(super) fn subscribe(
        webview: &PlatformWebview,
        mut on_failed: impl FnMut(FailedKind) + 'static,
    ) -> Result<(), String> {
        // SAFETY: plain COM calls on interfaces Tauri handed us for this
        // webview, made on the thread that owns them. `add_ProcessFailed`
        // AddRefs the handler, so dropping our reference afterwards is fine.
        unsafe {
            let core = webview
                .controller()
                .CoreWebView2()
                .map_err(|error| error.to_string())?;
            let handler = ProcessFailedEventHandler::create(Box::new(move |_sender, args| {
                let Some(args) = args else {
                    return Ok(());
                };
                let mut kind = FailedKind::default();
                args.ProcessFailedKind(&mut kind)?;
                on_failed(kind);
                Ok(())
            }));
            let mut token = 0i64;
            core.add_ProcessFailed(&handler, &mut token)
                .map_err(|error| error.to_string())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_dead_browser_or_render_process_means_rebuild() {
            assert!(needs_rebuild(
                COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED
            ));
            assert!(needs_rebuild(
                COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED
            ));
        }

        #[test]
        fn recoverable_and_helper_failures_are_log_only() {
            for kind in [
                COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE,
                COREWEBVIEW2_PROCESS_FAILED_KIND_FRAME_RENDER_PROCESS_EXITED,
                COREWEBVIEW2_PROCESS_FAILED_KIND_GPU_PROCESS_EXITED,
                COREWEBVIEW2_PROCESS_FAILED_KIND_UTILITY_PROCESS_EXITED,
                COREWEBVIEW2_PROCESS_FAILED_KIND_SANDBOX_HELPER_PROCESS_EXITED,
                COREWEBVIEW2_PROCESS_FAILED_KIND_PPAPI_PLUGIN_PROCESS_EXITED,
                COREWEBVIEW2_PROCESS_FAILED_KIND_PPAPI_BROKER_PROCESS_EXITED,
                COREWEBVIEW2_PROCESS_FAILED_KIND_UNKNOWN_PROCESS_EXITED,
            ] {
                assert!(!needs_rebuild(kind), "{}", describe(kind));
            }
        }

        #[test]
        fn a_kind_this_build_does_not_know_is_never_acted_on() {
            // A newer runtime may add kinds; an unknown value must neither
            // rebuild nor lose its numeric value in the log.
            let future = COREWEBVIEW2_PROCESS_FAILED_KIND(99);
            assert!(!needs_rebuild(future));
            assert_eq!(describe(future), "Unknown(99)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_is_always_rebuilt_and_replayed_only_when_it_was_showing() {
        assert_eq!(
            recovery_plan("main", true),
            RecoveryPlan::RebuildMain { replay: true }
        );
        assert_eq!(
            recovery_plan("main", false),
            RecoveryPlan::RebuildMain { replay: false }
        );
    }

    #[test]
    fn on_demand_windows_are_reopened_only_when_they_were_showing() {
        assert_eq!(
            recovery_plan("settings", true),
            RecoveryPlan::RebuildSettings { reopen: true }
        );
        assert_eq!(
            recovery_plan("settings", false),
            RecoveryPlan::RebuildSettings { reopen: false }
        );
        assert_eq!(
            recovery_plan("flyout", true),
            RecoveryPlan::RebuildFlyout { reopen: true }
        );
        assert_eq!(
            recovery_plan("flyout", false),
            RecoveryPlan::RebuildFlyout { reopen: false }
        );
    }

    #[test]
    fn windows_without_a_rebuild_recipe_are_left_alone() {
        // The floatbar has its own lifecycle; a typo'd label must never
        // be routed into somebody else's rebuild.
        assert_eq!(recovery_plan("floatbar", true), RecoveryPlan::Unsupported);
        assert_eq!(recovery_plan("Main", true), RecoveryPlan::Unsupported);
    }
}
