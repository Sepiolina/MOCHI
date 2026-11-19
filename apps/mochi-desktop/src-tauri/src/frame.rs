//! The window frame (release 0.9.10, owner request: one title bar).
//!
//! Every window is built without the operating system's title bar
//! (`decorations(false)`, with `shadow(true)` so that Windows 11 keeps its
//! shadow, one-pixel border and rounded corners) and the page draws a single
//! bar: the app icon, the window's title, the toolbar, and minimize,
//! maximize/restore and close. Resizing from the edges stays with the
//! windowing library, which gives undecorated windows resize borders on
//! Windows and Linux.
//!
//! The buttons need no `core:window:*` permission. Each is one small command
//! here that acts on **the window that called it**: Tauri hands the calling
//! webview window to the command, the page names no window, so a page cannot
//! move, close or resize any other window. Each has its own `allow-…`
//! permission in `capabilities/`, like every other app command.
//!
//! * [`window_close`] asks the window to close exactly as the operating
//!   system's close button (and Alt+F4) does: `Window::close` raises
//!   `CloseRequested` and then destroys the window, so the bookkeeping in
//!   `lib.rs` (`WindowEvent::Destroyed`: release the window's archives, jobs
//!   and passphrases, or stop the compact window's job) runs the same way.
//! * [`window_start_drag`] starts the operating system's move of the window
//!   from a press on the bar's empty part; a second press of a double click
//!   toggles maximize instead ([`window_toggle_maximize`]), as a native title
//!   bar does.
//! * [`window_watch`] tells the page whether its window is maximized (the
//!   button shows restore or maximize) and focused (the bar dims when it is
//!   not), now and on every change, over a channel ([`Frames`]).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tauri::{Runtime, State, WebviewWindow};
use ts_rs::TS;

use crate::ipc::IpcError;

/// What the title bar shows about its window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct FrameState {
    /// The window fills the screen's work area: the bar offers Restore.
    pub maximized: bool,
    /// The window has the keyboard focus: the bar is drawn active.
    pub focused: bool,
}

/// Where a window's frame changes go (a channel in the app, a recorder in tests).
pub trait FrameSink: Send + Sync {
    /// `false` when the listener is gone.
    fn send(&self, state: FrameState) -> bool;
}

impl FrameSink for Channel<FrameState> {
    fn send(&self, state: FrameState) -> bool {
        Channel::send(self, state).is_ok()
    }
}

struct Watched {
    sink: Arc<dyn FrameSink>,
    last: FrameState,
}

/// The listener of each window's title bar, by window label.
#[derive(Default)]
pub struct Frames {
    inner: Mutex<HashMap<String, Watched>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Frames {
    /// The page of `label` listens; it hears `now` at once. A page that
    /// loads again replaces its earlier listener.
    pub fn watch(&self, label: &str, sink: Arc<dyn FrameSink>, now: FrameState) {
        let mut g = lock(&self.inner);
        if sink.send(now) {
            g.insert(label.to_owned(), Watched { sink, last: now });
        } else {
            g.remove(label);
        }
    }

    /// Something about `label` changed; its listener hears the new state only
    /// if it differs from what it heard last (a resize is not news unless it
    /// maximized or restored the window).
    pub fn update(&self, label: &str, change: impl FnOnce(&mut FrameState)) {
        let mut g = lock(&self.inner);
        let Some(w) = g.get_mut(label) else { return };
        let mut next = w.last;
        change(&mut next);
        if next == w.last {
            return;
        }
        w.last = next;
        if !w.sink.send(next) {
            g.remove(label);
        }
    }

    /// The window is gone.
    pub fn forget(&self, label: &str) {
        lock(&self.inner).remove(label);
    }

    /// Whether `label`'s title bar is listening (tests).
    pub fn is_watched(&self, label: &str) -> bool {
        lock(&self.inner).contains_key(label)
    }
}

/// What the frame commands need of a window, so that the decisions are tested
/// without a display.
pub trait FrameWindow {
    fn minimize(&self) -> tauri::Result<()>;
    fn is_maximized(&self) -> tauri::Result<bool>;
    fn is_maximizable(&self) -> tauri::Result<bool>;
    fn is_resizable(&self) -> tauri::Result<bool>;
    fn maximize(&self) -> tauri::Result<()>;
    fn unmaximize(&self) -> tauri::Result<()>;
    fn close(&self) -> tauri::Result<()>;
    fn start_dragging(&self) -> tauri::Result<()>;
    fn is_focused(&self) -> tauri::Result<bool>;
}

impl<R: Runtime> FrameWindow for WebviewWindow<R> {
    fn minimize(&self) -> tauri::Result<()> {
        WebviewWindow::minimize(self)
    }
    fn is_maximized(&self) -> tauri::Result<bool> {
        WebviewWindow::is_maximized(self)
    }
    fn is_maximizable(&self) -> tauri::Result<bool> {
        WebviewWindow::is_maximizable(self)
    }
    fn is_resizable(&self) -> tauri::Result<bool> {
        WebviewWindow::is_resizable(self)
    }
    fn maximize(&self) -> tauri::Result<()> {
        WebviewWindow::maximize(self)
    }
    fn unmaximize(&self) -> tauri::Result<()> {
        WebviewWindow::unmaximize(self)
    }
    fn close(&self) -> tauri::Result<()> {
        WebviewWindow::close(self)
    }
    fn start_dragging(&self) -> tauri::Result<()> {
        WebviewWindow::start_dragging(self)
    }
    fn is_focused(&self) -> tauri::Result<bool> {
        WebviewWindow::is_focused(self)
    }
}

fn failed(what: &str) -> impl FnOnce(tauri::Error) -> IpcError + '_ {
    move |e| IpcError {
        code: mochi_core::ErrorCode::IoError.as_str().to_owned(),
        message: format!("the window could not {what}: {e}"),
    }
}

/// Maximize the window, or restore it if it is maximized. A window that may
/// not be maximized (the compact window) stays as it is. Returns whether the
/// window is maximized now.
pub fn toggle_maximize(window: &impl FrameWindow) -> Result<bool, IpcError> {
    let allowed =
        window.is_maximizable().unwrap_or(false) && window.is_resizable().unwrap_or(false);
    let maximized = window.is_maximized().map_err(failed("be measured"))?;
    if !allowed {
        return Ok(maximized);
    }
    if maximized {
        window.unmaximize().map_err(failed("be restored"))?;
    } else {
        window.maximize().map_err(failed("be maximized"))?;
    }
    Ok(!maximized)
}

/// The state the title bar starts from.
pub fn frame_state(window: &impl FrameWindow) -> FrameState {
    FrameState {
        maximized: window.is_maximized().unwrap_or(false),
        focused: window.is_focused().unwrap_or(true),
    }
}

/// The title bar's Minimize button.
#[tauri::command]
pub fn window_minimize<R: Runtime>(window: WebviewWindow<R>) -> Result<(), IpcError> {
    FrameWindow::minimize(&window).map_err(failed("be minimized"))
}

/// The title bar's Maximize / Restore button, and a double click on the bar.
#[tauri::command]
pub fn window_toggle_maximize<R: Runtime>(window: WebviewWindow<R>) -> Result<bool, IpcError> {
    toggle_maximize(&window)
}

/// The title bar's Close button: the same request as the operating system's
/// close button and Alt+F4 (`CloseRequested`, then `Destroyed`).
#[tauri::command]
pub fn window_close<R: Runtime>(window: WebviewWindow<R>) -> Result<(), IpcError> {
    FrameWindow::close(&window).map_err(failed("be closed"))
}

/// A press on the bar's empty part: the operating system moves the window
/// while the button is held.
#[tauri::command]
pub fn window_start_drag<R: Runtime>(window: WebviewWindow<R>) -> Result<(), IpcError> {
    FrameWindow::start_dragging(&window).map_err(failed("be moved"))
}

/// Listen for the window's maximized and focused state; returns it now.
#[tauri::command]
pub fn window_watch<R: Runtime>(
    window: WebviewWindow<R>,
    on_change: Channel<FrameState>,
    frames: State<'_, Arc<Frames>>,
) -> FrameState {
    let now = frame_state(&window);
    frames.watch(window.label(), Arc::new(on_change), now);
    now
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::cell::{Cell, RefCell};
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    /// Records what the title bar heard; `gone` makes the next send fail.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<FrameState>>, AtomicBool);

    impl FrameSink for Recorder {
        fn send(&self, state: FrameState) -> bool {
            self.0.lock().unwrap().push(state);
            !self.1.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn a_listener_hears_the_state_now_and_only_real_changes() {
        let frames = Frames::default();
        let rec = Arc::new(Recorder::default());
        let now = FrameState {
            maximized: false,
            focused: true,
        };
        frames.watch("main", rec.clone(), now);
        // A resize that did not maximize is not news.
        frames.update("main", |s| s.maximized = false);
        frames.update("main", |s| s.maximized = true);
        frames.update("main", |s| s.maximized = true);
        frames.update("main", |s| s.focused = false);
        // Another window's change does not reach this one.
        frames.update("win-1", |s| s.maximized = true);
        assert_eq!(
            *rec.0.lock().unwrap(),
            vec![
                now,
                FrameState {
                    maximized: true,
                    focused: true
                },
                FrameState {
                    maximized: true,
                    focused: false
                },
            ]
        );
        frames.forget("main");
        assert!(!frames.is_watched("main"));
        frames.update("main", |s| s.maximized = false);
        assert_eq!(rec.0.lock().unwrap().len(), 3);
    }

    #[test]
    fn a_listener_that_is_gone_is_dropped() {
        let frames = Frames::default();
        let rec = Arc::new(Recorder::default());
        frames.watch("main", rec.clone(), FrameState::default());
        rec.1.store(true, Ordering::SeqCst);
        frames.update("main", |s| s.maximized = true);
        assert!(!frames.is_watched("main"));
    }

    struct Fake {
        maximized: Cell<bool>,
        maximizable: bool,
        calls: RefCell<Vec<&'static str>>,
    }

    impl Fake {
        fn new(maximizable: bool) -> Self {
            Fake {
                maximized: Cell::new(false),
                maximizable,
                calls: RefCell::default(),
            }
        }
    }

    impl FrameWindow for Fake {
        fn minimize(&self) -> tauri::Result<()> {
            self.calls.borrow_mut().push("minimize");
            Ok(())
        }
        fn is_maximized(&self) -> tauri::Result<bool> {
            Ok(self.maximized.get())
        }
        fn is_maximizable(&self) -> tauri::Result<bool> {
            Ok(self.maximizable)
        }
        fn is_resizable(&self) -> tauri::Result<bool> {
            Ok(self.maximizable)
        }
        fn maximize(&self) -> tauri::Result<()> {
            self.calls.borrow_mut().push("maximize");
            self.maximized.set(true);
            Ok(())
        }
        fn unmaximize(&self) -> tauri::Result<()> {
            self.calls.borrow_mut().push("unmaximize");
            self.maximized.set(false);
            Ok(())
        }
        fn close(&self) -> tauri::Result<()> {
            self.calls.borrow_mut().push("close");
            Ok(())
        }
        fn start_dragging(&self) -> tauri::Result<()> {
            self.calls.borrow_mut().push("drag");
            Ok(())
        }
        fn is_focused(&self) -> tauri::Result<bool> {
            Ok(false)
        }
    }

    #[test]
    fn toggle_maximize_maximizes_then_restores() {
        let w = Fake::new(true);
        assert!(toggle_maximize(&w).unwrap());
        assert!(!toggle_maximize(&w).unwrap());
        assert_eq!(*w.calls.borrow(), vec!["maximize", "unmaximize"]);
        assert_eq!(
            frame_state(&w),
            FrameState {
                maximized: false,
                focused: false
            }
        );
    }

    #[test]
    fn the_compact_window_is_never_maximized() {
        let w = Fake::new(false);
        assert!(!toggle_maximize(&w).unwrap());
        assert!(w.calls.borrow().is_empty());
    }
}
