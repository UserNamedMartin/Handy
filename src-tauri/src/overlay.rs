use crate::input;
use crate::settings;
use crate::settings::{OverlayPosition, OverlayStyle};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize};

#[cfg(not(target_os = "macos"))]
use log::debug;

#[cfg(not(target_os = "macos"))]
use tauri::WebviewWindowBuilder;

#[cfg(target_os = "macos")]
use tauri::WebviewUrl;

#[cfg(target_os = "macos")]
use tauri_nspanel::{tauri_panel, CollectionBehavior, PanelBuilder, PanelLevel, StyleMask};

#[cfg(target_os = "linux")]
use crate::utils;

#[cfg(target_os = "linux")]
use gtk_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

#[cfg(target_os = "macos")]
tauri_panel! {
    panel!(RecordingOverlayPanel {
        config: {
            can_become_key_window: false,
            is_floating_panel: true
        }
    })
}

// Native overlay window sizes (logical points). One window is reused for every
// state and resized in `show_overlay_state`; each size need only be at least as
// large as the card it hosts (the `--ov-*` vars in RecordingOverlay.css). The
// card is CSS-anchored flush to the screen edge, so window height doesn't move
// where the card sits — only OVERLAY_TOP_OFFSET / OVERLAY_BOTTOM_OFFSET do. Keep
// these in sync with the CSS card geometry.
//
// Compact overlay (Minimal / transcribing / processing): the 24h pill animates
// width from 92 (--ov-rest-w) to 132 (--ov-work-w) and expands from center. The
// grow happens on a state change, which resizes the window first, so the window
// can track the resting width instead of standing at the working width all the
// time.
// This window is not just a canvas — it swallows every click inside it (there is
// no `ignore_cursor_events` anywhere), so any slack beyond the card is invisible
// dead space over whatever sits underneath. Sizing one window for the widest
// state is therefore not good enough: the pill rests at 92 and only reaches 132
// while transcribing, so a fixed 144-wide window left ~26 px of click-eating
// margin either side for the whole time you are actually speaking. The window
// now tracks the card the current state really draws — see `overlay_dimensions`.
//
// Card geometry, mirrored from the `--ov-*` custom properties in
// RecordingOverlay.css. Keep them in sync; `overlay_dimensions` should be the
// only reader.
const OVERLAY_REST_WIDTH: f64 = 92.0; // --ov-rest-w: listening pill
const OVERLAY_WORK_WIDTH: f64 = 132.0; // --ov-work-w: transcribing / processing pill
const OVERLAY_BASE_HEIGHT: f64 = 24.0; // --ov-base-h: control-row height

/// Total breathing room around the card (half per side). Purely a guard against
/// subpixel rounding shaving the pill's 14 px corner radius — the pop animation
/// scales 0.92 -> 1.0 and the dot's pulse ring stays inside the pill, so nothing
/// is ever drawn past the card box and no real slack is needed.
const OVERLAY_SLACK: f64 = 2.0;

/// Neutral size for the compact overlay: the widest state it can reach. Used
/// where no state is known yet — window creation, and as the fallback when the
/// live window size can't be read — since a fallback must never be smaller than
/// the card it has to hold.
const OVERLAY_WIDTH: f64 = OVERLAY_WORK_WIDTH + OVERLAY_SLACK;
const OVERLAY_HEIGHT: f64 = OVERLAY_BASE_HEIGHT + OVERLAY_SLACK;

// The Live panel opens to 392x118 (--ov-open-w plus the text region), so its
// window must fit the expanded form even while the pill is still small.
const OVERLAY_STREAM_WIDTH: f64 = 400.0;
const OVERLAY_STREAM_HEIGHT: f64 = 120.0;

/// Overlay window size (logical) for a given UI state.
///
/// `live_text` is [`crate::settings::AppSettings::show_live_transcript`]. With it
/// off the streaming card never opens into a panel, so reserving panel-sized
/// window would leave 400x120 of dead click area around a 92 px pill.
fn overlay_dimensions(state: &str, live_text: bool) -> (f64, f64) {
    let compact = |card_width: f64| (card_width + OVERLAY_SLACK, OVERLAY_BASE_HEIGHT + OVERLAY_SLACK);
    match state {
        // The Live panel opens into its text region on its own, driven by text
        // arriving rather than by a state change, so Rust never gets a chance to
        // grow the window first. It has to be pre-sized for the expanded form.
        "streaming" if live_text => (OVERLAY_STREAM_WIDTH, OVERLAY_STREAM_HEIGHT),
        // Listening. With live text off the streaming card renders as the
        // compact pill (see RecordingOverlay.tsx) — same shape, same width.
        "recording" | "streaming" => compact(OVERLAY_REST_WIDTH),
        // "transcribing" / "processing": the wider working pill. Reached only
        // via a state change, which resizes the window before the CSS width
        // transition runs, so the grow is never clipped.
        _ => compact(OVERLAY_WORK_WIDTH),
    }
}

static LAST_MIC_LEVEL_EMIT: AtomicU64 = AtomicU64::new(0);
const EMIT_THROTTLE_MS: u64 = 33; // ~30 FPS

#[cfg(target_os = "macos")]
const OVERLAY_TOP_OFFSET: f64 = 46.0;
#[cfg(any(target_os = "windows", target_os = "linux"))]
const OVERLAY_TOP_OFFSET: f64 = 4.0;

#[cfg(target_os = "macos")]
const OVERLAY_BOTTOM_OFFSET: f64 = 15.0;

#[cfg(any(target_os = "windows", target_os = "linux"))]
const OVERLAY_BOTTOM_OFFSET: f64 = 40.0;

/// Configures the edge and offset of a GTK layer surface. gtk-layer-shell
/// commits anchor and margin changes itself, including while the surface is
/// mapped, so changing position does not require a manual hide/show cycle.
#[cfg(target_os = "linux")]
fn configure_layer_shell_position(gtk_window: &gtk::ApplicationWindow, position: OverlayPosition) {
    let (edge, opposite_edge, margin) = match position {
        OverlayPosition::Top => (Edge::Top, Edge::Bottom, OVERLAY_TOP_OFFSET),
        OverlayPosition::Bottom => (Edge::Bottom, Edge::Top, OVERLAY_BOTTOM_OFFSET),
    };

    gtk_window.set_anchor(edge, true);
    gtk_window.set_anchor(opposite_edge, false);
    gtk_window.set_layer_shell_margin(edge, margin.round() as i32);
    gtk_window.set_layer_shell_margin(opposite_edge, 0);
}

/// Configures a GTK layer surface before it is shown.
///
/// Tauri's normal `set_size` path calls `gtk_window_resize`, but layer surfaces
/// derive their dimensions from GTK's size request. gtk-layer-shell documents
/// the `set_size_request` + `resize(1, 1)` sequence for forcing a new size.
#[cfg(target_os = "linux")]
fn configure_layer_shell_surface(
    gtk_window: &gtk::ApplicationWindow,
    position: OverlayPosition,
    width: f64,
    height: f64,
) {
    use gtk::prelude::{GtkWindowExt, WidgetExt};

    configure_layer_shell_position(gtk_window, position);

    gtk_window.set_size_request(
        width.round().max(1.0) as i32,
        height.round().max(1.0) as i32,
    );
    gtk_window.resize(1, 1);
}

/// Initializes GTK layer shell for Linux overlay window
/// Returns true if layer shell was successfully initialized, false otherwise
#[cfg(target_os = "linux")]
fn init_gtk_layer_shell(overlay_window: &tauri::webview::WebviewWindow) -> bool {
    if utils::env_flag_enabled("HANDY_NO_GTK_LAYER_SHELL") {
        debug!("Skipping GTK layer shell init (HANDY_NO_GTK_LAYER_SHELL is enabled)");
        return false;
    }

    if !gtk_layer_shell::is_supported() {
        return false;
    }

    // Try to get the GTK window from the Tauri webview
    if let Ok(gtk_window) = overlay_window.gtk_window() {
        gtk_window.init_layer_shell();
        gtk_window.set_layer(Layer::Overlay);
        gtk_window.set_keyboard_mode(KeyboardMode::None);
        gtk_window.set_exclusive_zone(0);

        let overlay_position = settings::get_settings(overlay_window.app_handle()).overlay_position;
        configure_layer_shell_surface(&gtk_window, overlay_position, OVERLAY_WIDTH, OVERLAY_HEIGHT);

        let initialized = gtk_window.is_layer_window();
        LAYER_SHELL_ACTIVE.store(initialized, Ordering::SeqCst);
        return initialized;
    }
    false
}

/// Forces a window to be topmost using Win32 API (Windows only)
/// This is more reliable than Tauri's set_always_on_top which can be overridden
#[cfg(target_os = "windows")]
fn force_overlay_topmost(overlay_window: &tauri::webview::WebviewWindow) {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
    };

    // Clone because run_on_main_thread takes 'static
    let overlay_clone = overlay_window.clone();

    // Make sure the Win32 call happens on the UI thread
    let _ = overlay_clone.clone().run_on_main_thread(move || {
        if let Ok(hwnd) = overlay_clone.hwnd() {
            unsafe {
                // Force Z-order: make this window topmost without changing size/pos or stealing focus
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
        }
    });
}

fn get_monitor_with_cursor(app_handle: &AppHandle) -> Option<tauri::Monitor> {
    if let Some(mouse_location) = input::get_cursor_position(app_handle) {
        if let Ok(monitors) = app_handle.available_monitors() {
            for monitor in monitors {
                // On Windows both the cursor (enigo -> GetCursorPos) and the
                // monitor bounds are physical pixels, so compare them directly.
                #[cfg(target_os = "windows")]
                if is_mouse_within_monitor(mouse_location, monitor.position(), monitor.size()) {
                    return Some(monitor);
                }

                // macOS/Linux: enigo returns logical coords, so scale the bounds down.
                #[cfg(not(target_os = "windows"))]
                {
                    let scale = monitor.scale_factor();
                    let pos = PhysicalPosition::new(
                        (monitor.position().x as f64 / scale) as i32,
                        (monitor.position().y as f64 / scale) as i32,
                    );
                    let size = PhysicalSize::new(
                        (monitor.size().width as f64 / scale) as u32,
                        (monitor.size().height as f64 / scale) as u32,
                    );
                    if is_mouse_within_monitor(mouse_location, &pos, &size) {
                        return Some(monitor);
                    }
                }
            }
        }
    }

    app_handle.primary_monitor().ok().flatten()
}

fn is_mouse_within_monitor(
    mouse_pos: (i32, i32),
    monitor_pos: &PhysicalPosition<i32>,
    monitor_size: &PhysicalSize<u32>,
) -> bool {
    let (mouse_x, mouse_y) = mouse_pos;
    let PhysicalPosition {
        x: monitor_x,
        y: monitor_y,
    } = *monitor_pos;
    let PhysicalSize {
        width: monitor_width,
        height: monitor_height,
    } = *monitor_size;

    mouse_x >= monitor_x
        && mouse_x < (monitor_x + monitor_width as i32)
        && mouse_y >= monitor_y
        && mouse_y < (monitor_y + monitor_height as i32)
}

/// Returns overlay position in logical coordinates (points on macOS).
///
/// The Bottom anchor uses the macOS work area (visibleFrame) so the overlay
/// tracks the Dock — above it when shown, at the screen edge when hidden.
/// This relies on tauri 2.11's work_area.position.y fix (#14655), the same
/// bug that led PR #969 to abandon work_area for full monitor bounds. Top and
/// the other platforms keep full monitor bounds plus the fixed offsets
/// (work_area is unreliable on Wayland; Windows' offset clears the taskbar).
///
/// We must use LogicalPosition (not PhysicalPosition) because Tauri/tao
/// converts PhysicalPosition using the scale factor of the monitor the window
/// is *currently* on, which is wrong when moving cross-monitor. Windows uses
/// `place_windows_overlay` instead (no single logical space across mixed DPI).

/// Whether the Dock is currently presented on screen in the active space, asked
/// directly of the window server (CGWindowList) rather than via a per-app
/// Accessibility attribute. App-agnostic — it works the same for native apps and
/// Electron apps (Claude, ChatGPT) in fullscreen, whereas `AXFullScreen` is
/// unreliable on Electron. False in a fullscreen space (the Dock isn't part of
/// that space) or when auto-hidden; true on a normal desktop with the Dock
/// showing. Used to anchor the overlay: no Dock on screen -> physical bottom;
/// Dock on screen -> above it (via work_area).
#[cfg(target_os = "macos")]
mod dock_state {
    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;
    use std::os::raw::c_void;

    type CFRef = *const c_void;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFArrayGetCount(array: CFRef) -> isize;
        fn CFArrayGetValueAtIndex(array: CFRef, idx: isize) -> CFRef;
        fn CFDictionaryGetValue(dict: CFRef, key: CFRef) -> CFRef;
        fn CFNumberGetValue(number: CFRef, the_type: i32, value: *mut c_void) -> u8;
        fn CFEqual(a: CFRef, b: CFRef) -> u8;
        fn CFRelease(cf: CFRef);
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFRef;
    }

    const ON_SCREEN_ONLY: u32 = 1; // kCGWindowListOptionOnScreenOnly
    const NULL_WINDOW: u32 = 0; // kCGNullWindowID
    const NUMBER_INT: i32 = 9; // kCFNumberIntType
    const DOCK_LAYER: i32 = 20; // kCGDockWindowLevel

    pub fn dock_is_on_screen() -> bool {
        unsafe {
            let list = CGWindowListCopyWindowInfo(ON_SCREEN_ONLY, NULL_WINDOW);
            if list.is_null() {
                return false;
            }
            // Dictionary keys are the literal strings CGWindowList uses.
            let key_layer = CFString::from_static_string("kCGWindowLayer");
            let key_owner = CFString::from_static_string("kCGWindowOwnerName");
            let dock_name = CFString::from_static_string("Dock");
            let key_layer_ref = key_layer.as_concrete_TypeRef() as CFRef;
            let key_owner_ref = key_owner.as_concrete_TypeRef() as CFRef;
            let dock_name_ref = dock_name.as_concrete_TypeRef() as CFRef;

            let count = CFArrayGetCount(list);
            let mut found = false;
            let mut i = 0isize;
            while i < count {
                let dict = CFArrayGetValueAtIndex(list, i);
                i += 1;
                if dict.is_null() {
                    continue;
                }
                // The Dock's tile bar lives at the dock window level (20); the
                // Dock process also owns the desktop wallpaper (a very different,
                // negative level), so the layer check excludes that.
                let layer_val = CFDictionaryGetValue(dict, key_layer_ref);
                if layer_val.is_null() {
                    continue;
                }
                let mut layer: i32 = 0;
                CFNumberGetValue(layer_val, NUMBER_INT, &mut layer as *mut i32 as *mut c_void);
                if layer != DOCK_LAYER {
                    continue;
                }
                let owner_val = CFDictionaryGetValue(dict, key_owner_ref);
                if !owner_val.is_null() && CFEqual(owner_val, dock_name_ref) != 0 {
                    found = true;
                    break;
                }
            }
            CFRelease(list);
            found
        }
    }
}

fn calculate_overlay_position(
    app_handle: &AppHandle,
    width: f64,
    height: f64,
) -> Option<(f64, f64)> {
    let monitor = get_monitor_with_cursor(app_handle)?;
    let scale = monitor.scale_factor();
    let monitor_x = monitor.position().x as f64 / scale;
    let monitor_y = monitor.position().y as f64 / scale;
    let monitor_width = monitor.size().width as f64 / scale;

    let settings = settings::get_settings(app_handle);

    let x = monitor_x + (monitor_width - width) / 2.0;
    let y = match settings.overlay_position {
        OverlayPosition::Top => monitor_y + OVERLAY_TOP_OFFSET,
        OverlayPosition::Bottom => {
            // work_area.position shares monitor.position's global coordinate
            // space, so no monitor offset is added.
            #[cfg(target_os = "macos")]
            let bottom = {
                if dock_state::dock_is_on_screen() {
                    // Dock is showing on this desktop: work_area already excludes
                    // it, so the pill sits just above the Dock.
                    let wa = monitor.work_area();
                    (wa.position.y as f64 + wa.size.height as f64) / scale
                } else {
                    // Fullscreen space (native OR Electron) or auto-hidden Dock:
                    // nothing reserves the bottom, so anchor to the physical edge.
                    monitor_y + monitor.size().height as f64 / scale
                }
            };
            #[cfg(not(target_os = "macos"))]
            let bottom = monitor_y + monitor.size().height as f64 / scale;

            bottom - height - OVERLAY_BOTTOM_OFFSET
        }
    };

    Some((x, y))
}

/// Current overlay window size in logical units (points), for repositioning
/// without assuming a fixed size (compact vs. streaming).
#[cfg(not(target_os = "windows"))]
fn current_overlay_logical_size(window: &tauri::webview::WebviewWindow) -> Option<(f64, f64)> {
    let size = window.inner_size().ok()?;
    let scale = window.scale_factor().ok()?;
    Some((size.width as f64 / scale, size.height as f64 / scale))
}

#[cfg(target_os = "windows")]
static WINDOWS_OVERLAY_IS_STREAMING: AtomicBool = AtomicBool::new(false);

/// Overlay rectangle in the destination monitor's physical pixels, so nothing
/// is converted through the window's previous-monitor DPI.
#[cfg(target_os = "windows")]
fn windows_overlay_bounds(
    monitor_position: PhysicalPosition<i32>,
    monitor_size: PhysicalSize<u32>,
    scale: f64,
    logical_width: f64,
    logical_height: f64,
    overlay_position: OverlayPosition,
) -> (i32, i32, i32, i32) {
    let width = (logical_width * scale).round().max(1.0) as i32;
    let height = (logical_height * scale).round().max(1.0) as i32;
    let x = (monitor_position.x as f64 + (monitor_size.width as f64 - width as f64) / 2.0).round()
        as i32;
    let y = match overlay_position {
        OverlayPosition::Top => {
            (monitor_position.y as f64 + OVERLAY_TOP_OFFSET * scale).round() as i32
        }
        OverlayPosition::Bottom => (monitor_position.y as f64 + monitor_size.height as f64
            - height as f64
            - OVERLAY_BOTTOM_OFFSET * scale)
            .round() as i32,
    };

    (x, y, width, height)
}

/// Moves and sizes the overlay in one native SetWindowPos, bypassing tao's
/// current-DPI logical conversion that mislands cross-monitor moves.
#[cfg(target_os = "windows")]
fn place_windows_overlay(
    app_handle: &AppHandle,
    overlay_window: &tauri::webview::WebviewWindow,
    logical_width: f64,
    logical_height: f64,
) -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};

    let monitor = get_monitor_with_cursor(app_handle)
        .ok_or_else(|| "failed to determine the monitor containing the cursor".to_string())?;
    let (x, y, width, height) = windows_overlay_bounds(
        *monitor.position(),
        *monitor.size(),
        monitor.scale_factor(),
        logical_width,
        logical_height,
        settings::get_settings(app_handle).overlay_position,
    );
    let hwnd = overlay_window
        .hwnd()
        .map_err(|error| format!("failed to get overlay window handle: {error}"))?;

    unsafe {
        SetWindowPos(
            hwnd,
            None,
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE | SWP_NOZORDER,
        )
        .map_err(|error| format!("failed to set overlay bounds: {error}"))?;
    }

    log::debug!(
        "windows overlay bounds: x={} y={} width={} height={} scale={}",
        x,
        y,
        width,
        height,
        monitor.scale_factor()
    );
    Ok(())
}

/// Creates the recording overlay window and keeps it hidden by default
#[cfg(not(target_os = "macos"))]
pub fn create_recording_overlay(app_handle: &AppHandle) {
    // On Linux (Wayland), monitor detection often fails, but we don't need exact coordinates
    // for Layer Shell as we use anchors. On other platforms, we require a monitor.
    #[cfg(not(target_os = "linux"))]
    {
        let position = calculate_overlay_position(app_handle, OVERLAY_WIDTH, OVERLAY_HEIGHT);
        if position.is_none() {
            debug!("Failed to determine overlay position, not creating overlay window");
            return;
        }
    }

    // Position starts unset — update_overlay_position() sets the correct
    // LogicalPosition before the overlay is shown.
    let mut builder = WebviewWindowBuilder::new(
        app_handle,
        "recording_overlay",
        tauri::WebviewUrl::App("src/overlay/index.html".into()),
    )
    .title("Recording")
    .resizable(false)
    .inner_size(OVERLAY_WIDTH, OVERLAY_HEIGHT)
    .shadow(false)
    .maximizable(false)
    .minimizable(false)
    .closable(false)
    .accept_first_mouse(true)
    .decorations(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .transparent(true)
    .focusable(false)
    .focused(false)
    .visible(false);

    if let Some(data_dir) = crate::portable::data_dir() {
        builder = builder.data_directory(data_dir.join("webview"));
    }

    #[allow(unused_variables)]
    match builder.build() {
        Ok(window) => {
            #[cfg(target_os = "linux")]
            {
                // Try to initialize GTK layer shell, ignore errors if compositor doesn't support it
                if init_gtk_layer_shell(&window) {
                    debug!("GTK layer shell initialized for overlay window");
                } else {
                    debug!("GTK layer shell not available, falling back to regular window");
                }
            }

            debug!("Recording overlay window created successfully (hidden)");
        }
        Err(e) => {
            debug!("Failed to create recording overlay window: {}", e);
        }
    }
}

/// Creates the recording overlay panel and keeps it hidden by default (macOS)
#[cfg(target_os = "macos")]
pub fn create_recording_overlay(app_handle: &AppHandle) {
    if let Some((x, y)) = calculate_overlay_position(app_handle, OVERLAY_WIDTH, OVERLAY_HEIGHT) {
        // PanelBuilder creates a Tauri window then converts it to NSPanel.
        // The window remains registered, so get_webview_window() still works.
        match PanelBuilder::<_, RecordingOverlayPanel>::new(app_handle, "recording_overlay")
            .url(WebviewUrl::App("src/overlay/index.html".into()))
            .title("Recording")
            .position(tauri::Position::Logical(tauri::LogicalPosition { x, y }))
            .level(PanelLevel::Status)
            .size(tauri::Size::Logical(tauri::LogicalSize {
                width: OVERLAY_WIDTH,
                height: OVERLAY_HEIGHT,
            }))
            .has_shadow(false)
            .transparent(true)
            .no_activate(true)
            .corner_radius(0.0)
            .style_mask(StyleMask::empty().borderless().nonactivating_panel())
            .with_window(|w| w.decorations(false).transparent(true).focusable(false))
            .collection_behavior(
                CollectionBehavior::new()
                    .can_join_all_spaces()
                    .full_screen_auxiliary(),
            )
            .build()
        {
            Ok(panel) => {
                panel.hide();
            }
            Err(e) => {
                log::error!("Failed to create recording overlay panel: {}", e);
            }
        }
    }
}

fn show_overlay_state(app_handle: &AppHandle, state: &str) {
    // Whether the overlay shows at all is governed by overlay_style; position
    // only chooses Top vs Bottom placement. Checked here (off the main thread)
    // so the common overlay-disabled case never pays for a main-thread hop.
    let settings = settings::get_settings(app_handle);
    if settings.overlay_style == OverlayStyle::None {
        return;
    }

    // The rest queries monitors and the cursor and mutates window geometry. On
    // Linux the monitor/cursor lookups hit GDK/Xlib on the process's shared X11
    // connection, which is only safe from the GTK main thread — running them on
    // a background thread corrupts the connection and hard-crashes the app
    // (issue #227). Hop to the main thread on every platform to keep the
    // geometry path uniform (a no-op cost on Windows, and it also keeps macOS's
    // NSScreen access main-thread-correct). run_on_main_thread runs the closure
    // inline when already on the main thread, so this never deadlocks.
    let handle = app_handle.clone();
    let state = state.to_string();
    let _ = app_handle.run_on_main_thread(move || show_overlay_state_on_main(&handle, &state));
}

fn show_overlay_state_on_main(app_handle: &AppHandle, state: &str) {
    // Size the overlay for this state (compact vs. streaming), then position it.
    let (width, height) = overlay_dimensions(state, settings::get_settings(app_handle).show_live_transcript);
    if let Some(overlay_window) = app_handle.get_webview_window("recording_overlay") {
        // Invalidate any delayed hide still in flight from a previous session
        // (see `hide_recording_overlay`).
        OVERLAY_SHOW_GENERATION.fetch_add(1, Ordering::SeqCst);

        #[cfg(target_os = "linux")]
        let shown_with_layer_shell = if LAYER_SHELL_ACTIVE.load(Ordering::SeqCst) {
            let position = settings::get_settings(app_handle).overlay_position;
            match overlay_window.gtk_window() {
                Ok(gtk_window) => {
                    configure_layer_shell_surface(&gtk_window, position, width, height)
                }
                Err(error) => log::error!("Failed to access GTK overlay window: {error}"),
            }
            let _ = overlay_window.show();
            true
        } else {
            false
        };
        #[cfg(not(target_os = "linux"))]
        let shown_with_layer_shell = false;

        if !shown_with_layer_shell {
            let size_started = std::time::Instant::now();
            #[cfg(not(target_os = "windows"))]
            let _ =
                overlay_window.set_size(tauri::Size::Logical(tauri::LogicalSize { width, height }));
            #[cfg(target_os = "windows")]
            WINDOWS_OVERLAY_IS_STREAMING.store(state == "streaming", Ordering::Relaxed);
            let size_elapsed = size_started.elapsed();

            let pos_started = std::time::Instant::now();
            #[cfg(not(target_os = "windows"))]
            let set_pos_elapsed =
                if let Some((x, y)) = calculate_overlay_position(app_handle, width, height) {
                    let set_pos_started = std::time::Instant::now();
                    let _ = overlay_window
                        .set_position(tauri::Position::Logical(tauri::LogicalPosition { x, y }));
                    // Appear in place — don't glide in from a previous spot.
                    #[cfg(target_os = "macos")]
                    overlay_anim_snap_to(Some((x, y)));
                    set_pos_started.elapsed()
                } else {
                    std::time::Duration::ZERO
                };
            #[cfg(target_os = "windows")]
            let set_pos_elapsed = {
                let set_pos_started = std::time::Instant::now();
                if let Err(error) =
                    place_windows_overlay(app_handle, &overlay_window, width, height)
                {
                    log::error!("Failed to place recording overlay: {error}");
                }
                set_pos_started.elapsed()
            };
            let pos_calc_elapsed = pos_started.elapsed() - set_pos_elapsed;

            let show_started = std::time::Instant::now();
            let _ = overlay_window.show();
            let show_elapsed = show_started.elapsed();

            // On Windows, aggressively re-assert "topmost" in the native Z-order after showing
            #[cfg(target_os = "windows")]
            force_overlay_topmost(&overlay_window);

            // Re-assert bounds after show(): the pre-show move crosses the DPI
            // boundary, and tao's WM_DPICHANGED reflow clobbers the first placement.
            #[cfg(target_os = "windows")]
            if let Err(error) = place_windows_overlay(app_handle, &overlay_window, width, height) {
                log::error!("Failed to re-assert recording overlay position: {error}");
            }

            log::debug!(
                "overlay '{}': set_size={:?} pos_calc={:?} set_pos={:?} show={:?}",
                state,
                size_elapsed,
                pos_calc_elapsed,
                set_pos_elapsed,
                show_elapsed
            );
        }

        // Keep tracking Dock/fullscreen changes while the overlay stays up.
        #[cfg(target_os = "macos")]
        start_overlay_reposition_loop(app_handle);

        let _ = overlay_window.emit("show-overlay", state);
    }
}

/// Notify the visible recording overlay that the input stream has delivered its
/// first sample chunk. Audio feedback uses the same backend readiness signal,
/// but this targeted event is skipped when overlays are disabled.
pub fn emit_recording_ready(app_handle: &AppHandle) {
    if !OVERLAY_ENABLED.load(Ordering::Relaxed) {
        return;
    }

    // Showing the overlay is also queued onto the main thread. Queue readiness
    // there as well so a very fast always-on stream cannot overtake show-overlay
    // and then get reset back to the arming state by the frontend.
    let handle = app_handle.clone();
    let _ = app_handle.run_on_main_thread(move || {
        let _ = handle.emit_to("recording_overlay", "recording-ready", ());
    });
}

/// Shows the recording overlay window with fade-in animation
pub fn show_recording_overlay(app_handle: &AppHandle) {
    show_overlay_state(app_handle, "recording");
}

/// Shows the larger streaming overlay that displays live transcription text
pub fn show_streaming_overlay(app_handle: &AppHandle) {
    show_overlay_state(app_handle, "streaming");
}

/// Shows the transcribing overlay window
pub fn show_transcribing_overlay(app_handle: &AppHandle) {
    show_overlay_state(app_handle, "transcribing");
}

/// Shows the processing overlay window
pub fn show_processing_overlay(app_handle: &AppHandle) {
    show_overlay_state(app_handle, "processing");
}

/// Updates the overlay window position based on current settings
pub fn update_overlay_position(app_handle: &AppHandle) {
    // Positioning queries monitors/cursor (GDK/Xlib on Linux) and moves the
    // window, so it must run on the main thread — see show_overlay_state.
    let handle = app_handle.clone();
    let _ = app_handle.run_on_main_thread(move || update_overlay_position_on_main(&handle));
}

fn update_overlay_position_on_main(app_handle: &AppHandle) {
    if let Some(overlay_window) = app_handle.get_webview_window("recording_overlay") {
        #[cfg(target_os = "linux")]
        if LAYER_SHELL_ACTIVE.load(Ordering::SeqCst) {
            let position = settings::get_settings(app_handle).overlay_position;
            match overlay_window.gtk_window() {
                Ok(gtk_window) => configure_layer_shell_position(&gtk_window, position),
                Err(error) => log::error!("Failed to access GTK overlay window: {error}"),
            }
            return;
        }

        #[cfg(target_os = "windows")]
        {
            let state = if WINDOWS_OVERLAY_IS_STREAMING.load(Ordering::Relaxed) {
                "streaming"
            } else {
                "recording"
            };
            let live_text = settings::get_settings(app_handle).show_live_transcript;
            let (width, height) = overlay_dimensions(state, live_text);
            if let Err(error) = place_windows_overlay(app_handle, &overlay_window, width, height) {
                log::error!("Failed to update recording overlay position: {error}");
            }
        }

        #[cfg(not(target_os = "windows"))]
        {
            // Use the window's current size so centering stays correct whether the
            // overlay is in compact or streaming layout.
            let (width, height) = current_overlay_logical_size(&overlay_window)
                .unwrap_or((OVERLAY_WIDTH, OVERLAY_HEIGHT));
            if let Some((x, y)) = calculate_overlay_position(app_handle, width, height) {
                let _ = overlay_window
                    .set_position(tauri::Position::Logical(tauri::LogicalPosition { x, y }));
                // Keep the ease loop in sync when position is set directly.
                #[cfg(target_os = "macos")]
                overlay_anim_snap_to(Some((x, y)));
            }
        }
    }
}

/// Generation counter bumped every time the overlay is shown. The delayed
/// `hide()` below only unmaps the window if no show happened after it was
/// scheduled, so a hide left over from a finished transcription can never
/// take down the overlay of a session that started in the meantime — e.g. a
/// press the coordinator remembered while the pipeline was busy and started
/// the instant it drained, well inside the 300 ms hide delay.
static OVERLAY_SHOW_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Live overlay repositioning: while the overlay is visible, track Dock/fullscreen
/// changes that happen mid-show (revealing the Dock in fullscreen, leaving
/// fullscreen, toggling auto-hide) and glide the pill to its new spot instead of
/// snapping. macOS only — other platforms anchor to fixed monitor bounds.
#[cfg(target_os = "macos")]
static OVERLAY_VISIBLE: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static OVERLAY_REPOSITION_GEN: AtomicU64 = AtomicU64::new(0);
/// Animated placement in logical points: `.0` = where the pill is right now,
/// `.1` = where it should be. The tick eases `.0` toward `.1`.
#[cfg(target_os = "macos")]
static OVERLAY_ANIM: std::sync::Mutex<(Option<(f64, f64)>, Option<(f64, f64)>)> =
    std::sync::Mutex::new((None, None));

/// Snap the animated placement to an exact point (on show: appear in place, no
/// slide) or clear it (on hide). Keeps the ease loop from gliding from a stale
/// position.
#[cfg(target_os = "macos")]
fn overlay_anim_snap_to(pos: Option<(f64, f64)>) {
    if let Ok(mut a) = OVERLAY_ANIM.lock() {
        *a = (pos, pos);
    }
}

#[cfg(target_os = "macos")]
fn start_overlay_reposition_loop(app_handle: &AppHandle) {
    OVERLAY_VISIBLE.store(true, Ordering::Relaxed);
    // Supersede any previous loop so only the latest show keeps running.
    let my_gen = OVERLAY_REPOSITION_GEN.fetch_add(1, Ordering::Relaxed) + 1;
    let app = app_handle.clone();
    std::thread::spawn(move || {
        let mut frame: u64 = 0;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(16)); // ~60 fps
            if !OVERLAY_VISIBLE.load(Ordering::Relaxed)
                || OVERLAY_REPOSITION_GEN.load(Ordering::Relaxed) != my_gen
            {
                break;
            }
            frame = frame.wrapping_add(1);
            // The Dock check is comparatively costly, so refresh the target only
            // ~8x/sec; ease toward it every frame for a smooth glide.
            let recompute = frame % 8 == 0;
            let app_for_main = app.clone();
            let _ = app.run_on_main_thread(move || {
                overlay_anim_tick(&app_for_main, recompute);
            });
        }
    });
}

/// One animation frame (main thread): occasionally refresh the target position,
/// then ease the window toward it and move it.
#[cfg(target_os = "macos")]
fn overlay_anim_tick(app_handle: &AppHandle, recompute: bool) {
    let Some(overlay_window) = app_handle.get_webview_window("recording_overlay") else {
        return;
    };
    let Ok(mut anim) = OVERLAY_ANIM.lock() else {
        return;
    };
    if recompute {
        let (width, height) = current_overlay_logical_size(&overlay_window)
            .unwrap_or((OVERLAY_WIDTH, OVERLAY_HEIGHT));
        if let Some(t) = calculate_overlay_position(app_handle, width, height) {
            anim.1 = Some(t);
        }
    }
    let Some(target) = anim.1 else {
        return;
    };
    let cur = anim.0.unwrap_or(target);
    // Exponential ease-out toward the target (~150–200 ms to settle at 60 fps).
    let ease = 0.30;
    let mut next = (
        cur.0 + (target.0 - cur.0) * ease,
        cur.1 + (target.1 - cur.1) * ease,
    );
    // Snap once within half a pixel so it doesn't crawl forever.
    if (target.0 - next.0).abs() < 0.5 && (target.1 - next.1).abs() < 0.5 {
        next = target;
    }
    if anim.0 != Some(next) {
        let _ = overlay_window.set_position(tauri::Position::Logical(tauri::LogicalPosition {
            x: next.0,
            y: next.1,
        }));
        anim.0 = Some(next);
    }
}

/// Hides the recording overlay window with fade-out animation
pub fn hide_recording_overlay(app_handle: &AppHandle) {
    #[cfg(target_os = "macos")]
    {
        OVERLAY_VISIBLE.store(false, Ordering::Relaxed);
        overlay_anim_snap_to(None);
    }
    // Always hide the overlay regardless of settings - if setting was changed while recording,
    // we still want to hide it properly
    if let Some(overlay_window) = app_handle.get_webview_window("recording_overlay") {
        // Snapshot before doing anything observable, so any show that lands
        // after this point invalidates the delayed hide below.
        let scheduled_at = OVERLAY_SHOW_GENERATION.load(Ordering::SeqCst);
        // Emit event to trigger fade-out animation
        let _ = overlay_window.emit("hide-overlay", ());
        // Hide the window after a short delay to allow animation to complete,
        // unless a newer session has shown the overlay again by then.
        let window_clone = overlay_window.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            if OVERLAY_SHOW_GENERATION.load(Ordering::SeqCst) != scheduled_at {
                log::debug!("Skipping stale overlay hide: a newer session is showing the overlay");
                return;
            }
            let _ = window_clone.hide();
        });
    }
}

// Cached "overlay is enabled" flag, kept in sync with overlay_style. Avoids
// reading the Tauri store on every audio callback (~24 Hz during recording).
// Defaults to false so the audio path doesn't emit until lib.rs::setup
// populates the cache from initial settings.
static OVERLAY_ENABLED: AtomicBool = AtomicBool::new(false);

/// Tracks whether gtk-layer-shell was successfully initialized (Linux only).
/// Used to skip layer-shell calls when the window is a regular fallback.
#[cfg(target_os = "linux")]
static LAYER_SHELL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Update the cached overlay-enabled flag. Called from `lib.rs` at
/// startup after settings load, and from `change_overlay_style_setting`
/// whenever the user changes whether the overlay is shown.
pub fn update_overlay_enabled_cache(enabled: bool) {
    OVERLAY_ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn emit_levels(app_handle: &AppHandle, levels: &[f32]) {
    // Skip emission when the overlay is disabled. The recording_overlay
    // window is created at boot regardless of overlay_style, so without this
    // guard a hidden overlay's WebKit subprocess still
    // processes every event. Each event drives some kind of WebKit
    // C++ allocation that accumulates without bound (mechanism not
    // directly characterized; see issue #1279 for the investigation).
    // For users with `overlay_style: none` (the Linux default) this skip
    // eliminates the upstream driver of that accumulation.
    if !OVERLAY_ENABLED.load(Ordering::Relaxed) {
        return;
    }

    // Throttle to ~30 FPS. Even with the overlay enabled, the raw audio
    // callback fires far faster than the UI needs; capping emission rate
    // cuts the per-frame `eval_script`/IPC volume that drives the wry
    // memory growth in issue #1279 (upstream tauri-apps/wry#1489).
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let last = LAST_MIC_LEVEL_EMIT.load(Ordering::Relaxed);
    if now.saturating_sub(last) < EMIT_THROTTLE_MS {
        return;
    }
    LAST_MIC_LEVEL_EMIT.store(now, Ordering::Relaxed);

    // Target only the overlay window. In Tauri 2 both `AppHandle::emit`
    // and `WebviewWindow::emit` broadcast to all webviews; Tauri's
    // listener filter then skips webviews with no registered listener
    // for the event, so the settings webview never received `mic-level`.
    // But the previous dual-call pattern still produced two `eval_script`
    // calls to the overlay per audio callback (one from each .emit()).
    // `emit_to` with the overlay's window label produces a single
    // eval_script call per callback, cutting the per-callback WebKit
    // dispatch work in half.
    let _ = app_handle.emit_to("recording_overlay", "mic-level", levels);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_hit_test_uses_half_open_physical_bounds() {
        let position = PhysicalPosition::new(-2560, -200);
        let size = PhysicalSize::new(2560, 1440);

        assert!(is_mouse_within_monitor((-2560, -200), &position, &size));
        assert!(is_mouse_within_monitor((-1, 1239), &position, &size));
        assert!(!is_mouse_within_monitor((0, 0), &position, &size));
        assert!(!is_mouse_within_monitor((-1, 1240), &position, &size));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_cursor_hit_test_does_not_scale_physical_monitor_bounds() {
        let position = PhysicalPosition::new(1920, 0);
        let size = PhysicalSize::new(3840, 2160);
        let cursor = (5000, 1000);

        assert!(is_mouse_within_monitor(cursor, &position, &size));

        // This is the old mixed-coordinate comparison. It excludes a cursor
        // that is visibly inside a secondary display running at 150%.
        let scale = 1.5;
        let logical_position = PhysicalPosition::new(
            (position.x as f64 / scale) as i32,
            (position.y as f64 / scale) as i32,
        );
        let logical_size = PhysicalSize::new(
            (size.width as f64 / scale) as u32,
            (size.height as f64 / scale) as u32,
        );
        assert!(!is_mouse_within_monitor(
            cursor,
            &logical_position,
            &logical_size
        ));
    }

    /// Every pixel of window beyond the card swallows a click, so each state's
    /// window must be its own card plus the rounding guard — nothing more.
    #[test]
    fn overlay_dimensions_track_the_card_each_state_draws() {
        let listening = (
            OVERLAY_REST_WIDTH + OVERLAY_SLACK,
            OVERLAY_BASE_HEIGHT + OVERLAY_SLACK,
        );
        let working = (
            OVERLAY_WORK_WIDTH + OVERLAY_SLACK,
            OVERLAY_BASE_HEIGHT + OVERLAY_SLACK,
        );

        assert_eq!(overlay_dimensions("recording", false), listening);
        // Live text off: the streaming card is the compact pill, so the window
        // must not reserve the panel. This is the regression that left ~26 px of
        // dead click area either side of the pill while speaking.
        assert_eq!(overlay_dimensions("streaming", false), listening);
        assert_eq!(overlay_dimensions("transcribing", false), working);
        assert_eq!(overlay_dimensions("processing", false), working);
    }

    /// The Live panel opens without a state change, so its window is the one
    /// place that must still be pre-sized for the widest form.
    #[test]
    fn overlay_dimensions_reserve_the_panel_only_for_live_text() {
        assert_eq!(
            overlay_dimensions("streaming", true),
            (OVERLAY_STREAM_WIDTH, OVERLAY_STREAM_HEIGHT)
        );
        // live_text only matters while streaming; the working pill is the same
        // either way.
        assert_eq!(
            overlay_dimensions("transcribing", true),
            overlay_dimensions("transcribing", false)
        );
    }

    /// A fallback stands in where no state is known, so it must never be
    /// narrower than the widest card it might have to hold.
    #[test]
    fn neutral_compact_size_covers_the_widest_compact_state() {
        let (w, h) = overlay_dimensions("transcribing", false);
        assert!(OVERLAY_WIDTH >= w, "{OVERLAY_WIDTH} < {w}");
        assert!(OVERLAY_HEIGHT >= h, "{OVERLAY_HEIGHT} < {h}");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_overlay_bounds_use_destination_monitor_scale() {
        let monitor_position = PhysicalPosition::new(1920, 0);
        let monitor_size = PhysicalSize::new(3840, 2160);

        assert_eq!(
            windows_overlay_bounds(
                monitor_position,
                monitor_size,
                1.5,
                256.0,
                46.0,
                OverlayPosition::Bottom,
            ),
            (3648, 2031, 384, 69)
        );
        assert_eq!(
            windows_overlay_bounds(
                monitor_position,
                monitor_size,
                1.5,
                256.0,
                46.0,
                OverlayPosition::Top,
            ),
            (3648, 6, 384, 69)
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_overlay_bounds_support_negative_monitor_origins() {
        assert_eq!(
            windows_overlay_bounds(
                PhysicalPosition::new(-2560, -200),
                PhysicalSize::new(2560, 1440),
                1.25,
                400.0,
                120.0,
                OverlayPosition::Bottom,
            ),
            (-1530, 1040, 500, 150)
        );
    }
}
