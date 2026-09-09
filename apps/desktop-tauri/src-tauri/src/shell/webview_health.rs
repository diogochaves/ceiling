//! Liveness probe for a Tauri window's WebView2 instance.
//!
//! When the WebView2 browser process exits — cleanly or by being killed —
//! WRY does not recreate it and Tauri keeps the bare native window alive.
//! Windows that are hidden rather than closed (`flyout_window::hide`) then
//! come back as an empty, transparent frame on the next `show()`: the shadow
//! and rounded corners paint, nothing else does, and only restarting the app
//! recovers. See issue #410.
//!
//! The probe reads the native child windows instead of asking WebView2,
//! because once the browser process is gone there is no controller left to
//! ask. A healthy Tauri window on Windows parents a `WRY_WEBVIEW` container
//! that itself hosts WebView2's `Chrome_WidgetWin_0`; an organically torn
//! down window has no children at all, and a force-killed one keeps the
//! `WRY_WEBVIEW` container with nothing inside it.
//!
//! Note that `WRY_WEBVIEW` is not necessarily the *first* child: a resizable
//! window also owns a `TAURI_DRAG_RESIZE_BORDERS` child, which sorts ahead of
//! it. Walking only `GetWindow(GW_CHILD)` therefore reports a dead `main`
//! window as healthy — observed directly on 2026-09-08, with the browser
//! process killed and the first-child chain still reading
//! `[TAURI_DRAG_RESIZE_BORDERS]`. All direct children are enumerated for that
//! reason.
//!
//! The decision itself is a pure function over what the enumeration found, so
//! it can be unit-tested away from Win32.

use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager, WebviewWindow};

/// Class name WRY gives the container HWND it parents the WebView2
/// controller to. WebView2's own child classes (`Chrome_WidgetWin_*`) are
/// deliberately not matched — they are an Edge implementation detail.
const WRY_CONTAINER_CLASS: &str = "WRY_WEBVIEW";

/// Upper bound on the direct children examined, so a pathological window
/// (or a corrupted sibling chain) can never spin the probe.
const MAX_CHILDREN: usize = 32;

/// How long `destroy_and_release` waits for Tauri to release a destroyed
/// window's label before giving up on the rebuild.
const LABEL_RELEASE_TIMEOUT: Duration = Duration::from_secs(2);
const LABEL_RELEASE_POLL: Duration = Duration::from_millis(10);

/// Decide from the enumerated children whether the webview is still hosted.
///
/// `container_hosts_webview` is `None` when no `WRY_WEBVIEW` child was found
/// at all, `Some(false)` when it was found but is empty, `Some(true)` when it
/// holds the WebView2 child.
///
/// Conservative on unknown shapes: only the two shapes known to be dead
/// report `false`, so a future WRY/WebView2 that renames its window classes
/// can never push a healthy window into a destroy/rebuild loop. Hence the
/// `unwrap_or(true)`: children but no container we recognise is unknown
/// territory, and assuming the window is fine beats rebuilding it in a loop.
pub(crate) fn shape_is_alive(children: &[String], container_hosts_webview: Option<bool>) -> bool {
    if children.is_empty() {
        // Organic browser exit: WRY tears its container down too.
        return false;
    }
    // A container that hosts the WebView2 child is the healthy shape; an
    // empty one is a killed browser process.
    container_hosts_webview.unwrap_or(true)
}

/// Whether `window` still hosts a live WebView2 instance.
///
/// A window that no longer exposes a native handle at all is reported dead
/// too: Tauri keeps a destroyed window in its label map until the event loop
/// processes `Destroyed`, and in that gap the runtime silently drops every
/// message sent to it — `show()` would return `Ok` and show nothing.
pub(crate) fn webview_is_alive(window: &WebviewWindow) -> bool {
    let Some((children, container_hosts_webview)) = probe(window) else {
        tracing::warn!(
            label = window.label(),
            "webview_health: window has no native handle; treating it as gone"
        );
        return false;
    };
    let alive = shape_is_alive(&children, container_hosts_webview);
    if !alive {
        tracing::warn!(
            label = window.label(),
            ?children,
            ?container_hosts_webview,
            "webview_health: window has no live WebView2 instance"
        );
    }
    alive
}

/// Destroy `window` and block until Tauri has released its label, so the
/// caller can rebuild a window under the same label.
///
/// Tauri only unregisters the label when the event loop processes the
/// window's `Destroyed` event, which happens after `destroy()` has already
/// returned — rebuilding straight away fails with "a window with label `…`
/// already exists". A `destroy()` error is logged rather than propagated:
/// the window may already be half-destroyed (see `webview_is_alive`), in
/// which case the label is on its way out and waiting is still the right
/// move. Blocks the calling thread while it waits; like
/// `WebviewWindowBuilder::build`, it must never run on the main thread.
pub(crate) fn destroy_and_release(app: &AppHandle, window: &WebviewWindow) -> Result<(), String> {
    let label = window.label().to_string();
    if let Err(error) = window.destroy() {
        tracing::warn!(
            %error,
            label,
            "webview_health: destroy() failed; waiting for the label to be released anyway"
        );
    }

    let deadline = Instant::now() + LABEL_RELEASE_TIMEOUT;
    while app.get_webview_window(&label).is_some() {
        if Instant::now() >= deadline {
            return Err(format!(
                "window `{label}` did not release its label after destroy"
            ));
        }
        std::thread::sleep(LABEL_RELEASE_POLL);
    }
    Ok(())
}

/// Class names of every direct child of the window, plus whether the
/// `WRY_WEBVIEW` container among them hosts a webview child of its own.
/// `None` when the window exposes no Win32 handle.
#[cfg(windows)]
fn probe(window: &WebviewWindow) -> Option<(Vec<String>, Option<bool>)> {
    use raw_window_handle::HasWindowHandle;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GET_WINDOW_CMD, GW_CHILD, GW_HWNDNEXT, GetClassNameW, GetWindow,
    };

    fn class_of(hwnd: HWND) -> String {
        let mut buffer = [0u16; 256];
        let len = unsafe { GetClassNameW(hwnd, &mut buffer) };
        String::from_utf16_lossy(&buffer[..len.max(0) as usize])
    }

    // `GetWindow` reports "no such window" as an error, which is the normal
    // end of a chain rather than a failure.
    fn related(hwnd: HWND, cmd: GET_WINDOW_CMD) -> Option<HWND> {
        let next = unsafe { GetWindow(hwnd, cmd) }.ok()?;
        (!next.is_invalid()).then_some(next)
    }

    let handle = window.window_handle().ok()?;
    let raw_window_handle::RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return None;
    };
    let root = HWND(win32.hwnd.get() as *mut std::ffi::c_void);

    let mut children = Vec::new();
    let mut container_hosts_webview = None;
    let mut child = related(root, GW_CHILD);
    while let Some(hwnd) = child {
        if children.len() >= MAX_CHILDREN {
            break;
        }
        let class = class_of(hwnd);
        if class == WRY_CONTAINER_CLASS {
            // Found the container: it is live only if WebView2 has parented
            // its own window inside it.
            container_hosts_webview = Some(related(hwnd, GW_CHILD).is_some());
        }
        children.push(class);
        child = related(hwnd, GW_HWNDNEXT);
    }
    Some((children, container_hosts_webview))
}

#[cfg(not(windows))]
fn probe(_window: &WebviewWindow) -> Option<(Vec<String>, Option<bool>)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classes(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn healthy_window_hosts_a_container_with_a_webview2_child() {
        assert!(shape_is_alive(&classes(&["WRY_WEBVIEW"]), Some(true)));
    }

    #[test]
    fn organic_browser_exit_leaves_no_children_at_all() {
        // Observed in the wild (#410): WRY tears its container down too.
        assert!(!shape_is_alive(&classes(&[]), None));
    }

    #[test]
    fn killed_browser_process_leaves_an_empty_container() {
        // The forced repro (Stop-Process on msedgewebview2.exe) orphans the
        // WRY_WEBVIEW container, so a "has any child" check would wrongly
        // report this window as alive.
        assert!(!shape_is_alive(&classes(&["WRY_WEBVIEW"]), Some(false)));
    }

    #[test]
    fn a_resizable_window_is_judged_by_its_container_not_its_first_child() {
        // Regression test for the probe walking only `GW_CHILD`: a resizable
        // window parents `TAURI_DRAG_RESIZE_BORDERS` ahead of `WRY_WEBVIEW`,
        // so the first child says nothing about the webview. Observed on the
        // `main` window, 2026-09-08.
        let resizable = classes(&["TAURI_DRAG_RESIZE_BORDERS", "WRY_WEBVIEW"]);
        assert!(shape_is_alive(&resizable, Some(true)));
        assert!(!shape_is_alive(&resizable, Some(false)));
    }

    #[test]
    fn unknown_window_shapes_are_treated_as_alive() {
        // Never rebuild on a shape we do not recognise — a false "dead" would
        // be a destroy/rebuild loop, which is worse than the original bug.
        assert!(shape_is_alive(&classes(&["SomeOtherContainer"]), None));
        assert!(shape_is_alive(
            &classes(&["TAURI_DRAG_RESIZE_BORDERS"]),
            None
        ));
    }
}
