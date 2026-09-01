//! Liveness probe for a Tauri window's WebView2 instance.
//!
//! When the WebView2 browser process exits — cleanly or by being killed —
//! WRY does not recreate it and Tauri keeps the bare native window alive.
//! Windows that are hidden rather than closed (`flyout_window::hide`) then
//! come back as an empty, transparent frame on the next `show()`: the shadow
//! and rounded corners paint, nothing else does, and only restarting the app
//! recovers. See issue #410.
//!
//! The probe reads the native child-window chain instead of asking WebView2,
//! because once the browser process is gone there is no controller left to
//! ask. A healthy Tauri window on Windows hosts
//! `WRY_WEBVIEW` → `Chrome_WidgetWin_0` → `Chrome_WidgetWin_1` → …, an
//! organically torn-down one has no children at all, and a force-killed one
//! keeps only the orphaned `WRY_WEBVIEW` container. The decision itself is a
//! pure function over the class-name chain so it can be unit-tested away
//! from Win32.

use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager, WebviewWindow};

/// Class name WRY gives the container HWND it parents the WebView2
/// controller to. WebView2's own child classes (`Chrome_WidgetWin_*`) are
/// deliberately not matched — they are an Edge implementation detail.
const WRY_CONTAINER_CLASS: &str = "WRY_WEBVIEW";

/// How many levels of first-child HWNDs to read. Two are enough to tell the
/// three known shapes apart (nothing / container only / container + child).
const PROBE_DEPTH: usize = 2;

/// How long `destroy_and_release` waits for Tauri to release a destroyed
/// window's label before giving up on the rebuild.
const LABEL_RELEASE_TIMEOUT: Duration = Duration::from_secs(2);
const LABEL_RELEASE_POLL: Duration = Duration::from_millis(10);

/// Decide from the first-child class chain whether the webview is still
/// hosted.
///
/// Conservative on unknown shapes: only the two shapes known to be dead
/// report `false`, so a future WRY/WebView2 that renames its window classes
/// can never push a healthy window into a destroy/rebuild loop.
pub(crate) fn chain_is_alive(chain: &[String]) -> bool {
    match chain {
        [] => false,
        [container] if container == WRY_CONTAINER_CLASS => false,
        _ => true,
    }
}

/// Whether `window` still hosts a live WebView2 instance.
///
/// A window that no longer exposes a native handle at all is reported dead
/// too: Tauri keeps a destroyed window in its label map until the event loop
/// processes `Destroyed`, and in that gap the runtime silently drops every
/// message sent to it — `show()` would return `Ok` and show nothing.
pub(crate) fn webview_is_alive(window: &WebviewWindow) -> bool {
    let Some(chain) = child_class_chain(window) else {
        tracing::warn!(
            label = window.label(),
            "webview_health: window has no native handle; treating it as gone"
        );
        return false;
    };
    let alive = chain_is_alive(&chain);
    if !alive {
        tracing::warn!(
            label = window.label(),
            ?chain,
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

/// Class names of the first child HWND at each level below the window, up
/// to `PROBE_DEPTH`. `None` when the window exposes no Win32 handle.
#[cfg(windows)]
fn child_class_chain(window: &WebviewWindow) -> Option<Vec<String>> {
    use raw_window_handle::HasWindowHandle;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{GW_CHILD, GetClassNameW, GetWindow};

    let handle = window.window_handle().ok()?;
    let raw_window_handle::RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return None;
    };

    let mut hwnd = HWND(win32.hwnd.get() as *mut std::ffi::c_void);
    let mut chain = Vec::with_capacity(PROBE_DEPTH);
    for _ in 0..PROBE_DEPTH {
        // `GetWindow` reports "no such child" as an error, which is the
        // normal end of the chain rather than a failure.
        let Ok(child) = (unsafe { GetWindow(hwnd, GW_CHILD) }) else {
            break;
        };
        if child.is_invalid() {
            break;
        }
        let mut buffer = [0u16; 256];
        let len = unsafe { GetClassNameW(child, &mut buffer) };
        chain.push(String::from_utf16_lossy(&buffer[..len.max(0) as usize]));
        hwnd = child;
    }
    Some(chain)
}

#[cfg(not(windows))]
fn child_class_chain(_window: &WebviewWindow) -> Option<Vec<String>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(classes: &[&str]) -> Vec<String> {
        classes.iter().map(|class| (*class).to_string()).collect()
    }

    #[test]
    fn healthy_window_hosts_the_container_and_a_webview2_child() {
        assert!(chain_is_alive(&chain(&[
            "WRY_WEBVIEW",
            "Chrome_WidgetWin_0"
        ])));
    }

    #[test]
    fn organic_browser_exit_leaves_no_children_at_all() {
        // Observed in the wild (#410): WRY tears its container down too.
        assert!(!chain_is_alive(&chain(&[])));
    }

    #[test]
    fn killed_browser_process_leaves_only_the_wry_container() {
        // The forced repro (Stop-Process on msedgewebview2.exe) orphans the
        // WRY_WEBVIEW container, so a one-level "has any child" check would
        // wrongly report this window as alive.
        assert!(!chain_is_alive(&chain(&["WRY_WEBVIEW"])));
    }

    #[test]
    fn unknown_window_shapes_are_treated_as_alive() {
        // Never rebuild on a shape we do not recognise — a false "dead" would
        // be a destroy/rebuild loop, which is worse than the original bug.
        assert!(chain_is_alive(&chain(&["SomeOtherContainer"])));
        assert!(chain_is_alive(&chain(&[
            "WRY_WEBVIEW",
            "NotChromeButSomething"
        ])));
    }
}
