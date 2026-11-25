//! The MOCHI desktop application (plan §7, phases D0 to D6).
//!
//! Commands are thin adapters (AGENTS.md "Tauri v2 rules"): they validate
//! input, call into `mochi-core` or the job runner, and return typed
//! results. No format logic lives here or in the UI. The IPC types are
//! generated into `ui/src/ipc/generated/` by `ts-rs` when the tests run;
//! CI fails if the checked-in copies are stale.

pub mod afterextract;
mod automation;
pub mod browse;
pub mod check;
pub mod commands;
pub mod crashlog;
#[cfg(test)]
mod d5_tests;
pub mod dragout;
pub mod extract;
pub mod find;
pub mod folders;
pub mod foreign;
pub mod formats;
pub mod frame;
pub mod help;
pub mod history;
pub mod ipc;
pub mod jobs;
pub mod launch;
mod picker;
pub mod quick;
pub mod recent;
pub mod rekey;
pub mod retrieve;
pub mod route;
pub mod scan;
pub mod search;
pub mod self_test;
pub mod speed;
pub mod trashbin;
pub mod update;
pub mod windows;
#[cfg(test)]
mod windows_tests;
pub mod write;

use std::sync::Arc;

/// Build and run the application until its last window closes.
pub fn run() -> Result<(), tauri::Error> {
    let archives = Arc::new(browse::Archives::default());
    let recents = Arc::new(recent::Recents::default());
    archives.set_recents(Arc::clone(&recents));
    let foreign = Arc::new(foreign::Foreign::default());
    foreign.set_recents(Arc::clone(&recents));
    let updates = Arc::new(update::Updates::default());
    // The shared services. What belongs to one window (its staged sources,
    // extract folder, Test and Repair tool) is made per window and handed to
    // each job when it starts (`windows::WindowState`, `JobRegistry::services_for`).
    // "Open the folder when done": one choice for the compact window and the archive windows.
    let quick = quick::QuickPool::new();
    let after_extract = quick.after_extract();
    after_extract.set_opener(Arc::new(|folder: &std::path::Path| {
        help::open_folder(folder).map_err(|e| std::io::Error::other(e.message))
    }));
    let jobs = Arc::new(jobs::JobRegistry::with_services(write::Services {
        archives: Arc::clone(&archives),
        foreign: Arc::clone(&foreign),
        updates: Arc::clone(&updates),
        after_extract: Some(Arc::clone(&after_extract)),
        ..write::Services::default()
    }));
    let windows = Arc::new(windows::WindowStates::new(
        Arc::clone(&archives),
        Arc::clone(&foreign),
        Arc::clone(&jobs),
    ));
    let launches = Arc::new(launch::Launches::new(
        Arc::clone(&windows),
        Arc::clone(&quick),
    ));
    let window_states = Arc::clone(&windows);
    let setup_windows = Arc::clone(&windows);
    let second_launches = Arc::clone(&launches);
    let first_launches = Arc::clone(&launches);
    let setup_updates = Arc::clone(&updates);
    let setup_recents = Arc::clone(&recents);
    let setup_quick = Arc::clone(&quick);
    let event_quick = Arc::clone(&quick);
    let frames = Arc::new(frame::Frames::default());
    let event_frames = Arc::clone(&frames);
    tauri::Builder::default()
        // First, so that a second process hands over before anything else
        // starts. Its arguments are parsed and validated in Rust like the
        // first launch's (`launch`). "Open with MOCHI" brings the main window
        // forward (from the worker, which creates it if only a compact window
        // is open); every other verb opens only the compact window. This
        // callback may run on the event loop, so it creates no window itself.
        .plugin(tauri_plugin_single_instance::init(
            move |_app, argv, cwd| match launch::from_forwarded(&argv, &cwd) {
                Ok(None) => second_launches.focus(),
                parsed => second_launches.submit(parsed),
            },
        ))
        .plugin(tauri_plugin_dialog::init())
        // The updater is driven from Rust only (`update`): the webview is not
        // granted the plugin's permissions, so its own commands are unreachable.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(launches)
        .manage(jobs)
        .manage(archives)
        .manage(windows)
        .manage(foreign)
        .manage(updates)
        .manage(recents)
        .manage(quick)
        .manage(frames)
        .on_window_event(move |window, event| {
            let label = window.label();
            // What the page's own title bar shows: maximize or restore, and
            // whether the window is active (`frame`).
            match event {
                tauri::WindowEvent::Resized(_) => {
                    let maximized = window.is_maximized().unwrap_or(false);
                    event_frames.update(label, |s| s.maximized = maximized);
                }
                tauri::WindowEvent::Focused(focused) => {
                    event_frames.update(label, |s| s.focused = *focused);
                }
                tauri::WindowEvent::Destroyed => event_frames.forget(label),
                _ => {}
            }
            match event {
                // Items dragged from the file manager arrive here, in Rust.
                // Only an archive window takes them (into its own staging):
                // the compact window is not an add panel.
                tauri::WindowEvent::DragDrop(drag) if windows::is_archive_label(label) => {
                    let state = window_states.state(label);
                    write::on_drag_drop(drag, &state.staging, &state.drops);
                }
                tauri::WindowEvent::Focused(true) if windows::is_archive_label(label) => {
                    window_states.touch(label);
                }
                tauri::WindowEvent::Destroyed => {
                    if quick::is_quick_label(label) {
                        // Closing a compact window stops its job, and no other.
                        event_quick.closed(label);
                    } else if windows::is_archive_label(label) {
                        // Closing an archive window closes its archives
                        // (releasing their readers), cancels its jobs and
                        // forgets its passphrases, and nothing of any other
                        // window.
                        window_states.release(label);
                    }
                }
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::start_job,
            commands::cancel_job,
            commands::pause_job,
            commands::open_archive,
            commands::unlock_archive,
            commands::cancel_unlock,
            commands::list_envelopes,
            commands::unlock_check_subject,
            commands::close_archive,
            commands::open_snapshot,
            commands::archive_info,
            commands::scan_setup,
            commands::pick_scanner_program,
            commands::list_children,
            commands::entry_details,
            commands::watch_drops,
            commands::staged_sources,
            commands::stage_sources,
            commands::unstage_source,
            commands::clear_staged,
            commands::choose_destination,
            commands::plan_extract,
            commands::plan_extract_default,
            commands::extract_wizard_setup,
            commands::get_extract_settings,
            commands::set_extract_settings,
            commands::browse_extract_folder,
            commands::plan_extract_to,
            commands::plan_extract_foreign_to,
            commands::preview_extract_to,
            commands::set_open_folder_when_done,
            commands::discard_extract_folder,
            commands::recent_archives,
            commands::forget_recent_archives,
            commands::open_recent,
            commands::open_dropped_archive,
            commands::stage_dropped_archive,
            commands::show_in_folder,
            commands::open_foreign_archive,
            commands::close_foreign_archive,
            commands::foreign_children,
            commands::foreign_search,
            commands::plan_extract_foreign,
            commands::watch_launches,
            commands::watch_quick,
            commands::quick_passphrase,
            commands::quick_cancel,
            commands::quick_pause,
            commands::quick_show_folder,
            commands::quick_close,
            commands::quick_choose_location,
            commands::quick_add_submit,
            commands::quick_set_open_folder,
            commands::quick_rename_folder,
            commands::quick_move_folder,
            commands::set_archive_format,
            commands::choose_check_subject,
            commands::health_report,
            commands::export_report,
            commands::repair_setup,
            commands::get_update_settings,
            commands::set_update_settings,
            commands::check_for_update,
            commands::get_crash_settings,
            commands::set_crash_settings,
            commands::get_speed_settings,
            commands::set_speed_settings,
            commands::read_crash_log,
            commands::export_crash_log,
            commands::clear_crash_log,
            commands::open_user_guide,
            find::find_scopes,
            find::find_choose_folders,
            find::find_remove_scope,
            find::find_start,
            find::find_cancel,
            find::find_query,
            find::find_query_many,
            find::find_choose_retrieve_folder,
            find::find_open_hit,
            find::find_show_folder,
            dragout::drag_out,
            dragout::drag_out_foreign,
            frame::window_minimize,
            frame::window_toggle_maximize,
            frame::window_close,
            frame::window_start_drag,
            frame::window_watch
        ])
        .setup(move |app| {
            // The opt-in crash log (D7): off until the user turns it on.
            use tauri::Manager;
            let crash_log = crashlog::CrashLog::open(app.path().app_local_data_dir()?);
            // How many cores a write may use (0.9.11), before any write can start.
            speed::load(app.path().app_local_data_dir()?);
            // The compact window's "Open the folder when done", before a quick verb runs.
            setup_quick.load_settings(app.path().app_local_data_dir()?);
            crashlog::install(Arc::clone(&crash_log));
            app.manage(crash_log);
            setup_quick.set_host(Arc::new(Windows {
                app: app.handle().clone(),
                states: Arc::clone(&setup_windows),
            }));
            // The shell's arguments for this first launch. A quick verb opens
            // only the compact window; the main one is made if it is needed.
            let first = launch::from_environment();
            if !matches!(&first, Ok(Some(l)) if l.verb.is_quick()) {
                setup_windows.register(MAIN_LABEL);
                build_archive_window(app, MAIN_LABEL, None)?;
            }
            {
                use tauri::Manager;
                let dir = app.path().app_config_dir()?;
                setup_recents.configure(&dir);
                // The virus scanner chosen last, and scan folders an earlier run left.
                scan::configure(dir.clone(), app.path().app_cache_dir()?);
                setup_windows.history().configure(&dir);
                setup_updates.configure(
                    dir,
                    Arc::new(update::PluginBackend::new(app.handle().clone())),
                );
            }
            first_launches.submit(first);
            // Drag-out folders left by an earlier run (an hour old or more).
            dragout::sweep_in_background(app.handle());
            Ok(())
        })
        .build(tauri::generate_context!())?
        .run(|app, event| {
            // The same sweep at exit.
            if matches!(event, tauri::RunEvent::Exit) {
                dragout::sweep_on_exit(app);
                scan::sweep_on_exit();
            }
        });
    Ok(())
}

use windows::MAIN_LABEL;

/// How far a new archive window sits from the one it was opened from, in
/// logical pixels.
const CASCADE_STEP: f64 = 32.0;

/// The size of an archive window with no opener to copy, in logical pixels.
const DEFAULT_SIZE: (f64, f64) = (1000.0, 680.0);

/// Build an archive window: the first one (`main`) or one opened beside
/// another (`win-N`; same page, same capability, `capabilities/main.json`).
/// Built here rather than in `tauri.conf.json` so that test builds can
/// configure WebView2 for WebDriver (`automation`). A new window is the size
/// of `opener` and cascaded down and to the right of it.
fn build_archive_window<M: tauri::Manager<tauri::Wry>>(
    manager: &M,
    label: &str,
    opener: Option<&str>,
) -> tauri::Result<()> {
    let mut size = DEFAULT_SIZE;
    let mut position = None;
    if let Some(opener) = opener.and_then(|o| manager.get_webview_window(o)) {
        let scale = opener.scale_factor().unwrap_or(1.0).max(0.1);
        if let Ok(inner) = opener.inner_size() {
            size = (
                f64::from(inner.width) / scale,
                f64::from(inner.height) / scale,
            );
        }
        if let (Ok(at), Ok(Some(monitor))) = (opener.outer_position(), opener.current_monitor()) {
            let (mp, ms) = (monitor.position(), monitor.size());
            let step = (CASCADE_STEP * scale).round() as i32;
            let (x, y) = windows::cascade(
                (at.x, at.y),
                step,
                ((mp.x, mp.y), (ms.width, ms.height)),
                ((size.0 * scale) as u32, (size.1 * scale) as u32),
            );
            position = Some((f64::from(x) / scale, f64::from(y) / scale));
        }
    }
    let mut builder =
        tauri::WebviewWindowBuilder::new(manager, label, tauri::WebviewUrl::default())
            .title("MOCHI")
            .inner_size(size.0, size.1)
            // The title bar is one row (title, toolbar, window buttons) and
            // the list needs room: below this the page would have to scroll
            // sideways.
            .min_inner_size(720.0, 480.0)
            // One title bar, drawn by the page (`frame`, release 0.9.10). The
            // shadow keeps Windows 11's shadow, border and rounded corners;
            // the edges still resize the window on Windows and Linux.
            .decorations(false)
            .shadow(true);
    if let Some((x, y)) = position {
        builder = builder.position(x, y);
    }
    automation::configure(builder).build()?;
    Ok(())
}

/// The windows, for the launch worker and the quick-verb worker (never the
/// event loop: creating a window there can deadlock on Windows).
struct Windows {
    app: tauri::AppHandle,
    states: Arc<windows::WindowStates>,
}

impl Windows {
    fn bring_forward(&self, label: &str) -> bool {
        use tauri::Manager;
        match self.app.get_webview_window(label) {
            Some(window) => {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
                true
            }
            None => false,
        }
    }
}

impl quick::WindowHost for Windows {
    fn show_quick(&self, label: &str, size: quick::QuickSize) {
        use tauri::Manager;
        // The Add form is taller than a job's progress and result.
        let (width, height) = match size {
            // Room for "Open the folder when done" and the rename row after an extraction.
            quick::QuickSize::Compact => (460.0, 340.0),
            quick::QuickSize::Form => (480.0, 470.0),
        };
        if let Some(window) = self.app.get_webview_window(label) {
            let _ = window.set_size(tauri::LogicalSize::new(width, height));
        }
        if self.bring_forward(label) {
            return;
        }
        // Several compact windows may be open at once (one per job): each new one sits down
        // and to the right of the centre, by how many are open already, so none hides another.
        let open = self
            .app
            .webview_windows()
            .keys()
            .filter(|l| quick::is_quick_label(l))
            .count();
        // The same page as the main window, told apart by the fragment.
        let builder = tauri::WebviewWindowBuilder::new(
            &self.app,
            label,
            tauri::WebviewUrl::App("index.html#quick".into()),
        )
        .title("MOCHI")
        .inner_size(width, height)
        .resizable(false)
        .maximizable(false)
        // The same title bar as the archive windows, without maximize (`frame`).
        .decorations(false)
        .shadow(true)
        .center()
        .focused(true);
        // If it cannot be made the job still runs; its result is kept for a
        // window that opens later.
        if let Ok(window) = automation::configure(builder).build() {
            if open > 0 {
                let step = i32::try_from(open.min(8)).unwrap_or(8) * CASCADE_STEP as i32;
                let scale = window.scale_factor().unwrap_or(1.0).max(0.1);
                if let Ok(at) = window.outer_position() {
                    let px = (f64::from(step) * scale).round() as i32;
                    let _ = window.set_position(tauri::PhysicalPosition::new(at.x + px, at.y + px));
                }
            }
        }
    }

    fn close_quick(&self, label: &str) {
        use tauri::Manager;
        if let Some(window) = self.app.get_webview_window(label) {
            let _ = window.destroy();
        }
    }

    fn show_main(&self) {
        // The window used last, else the first archive window, made again if
        // every archive window was closed (only the compact window is open).
        if let Some(label) = self.states.labels().into_iter().next() {
            if self.bring_forward(&label) {
                return;
            }
        }
        let label = self.states.next_label();
        self.states.register(&label);
        if build_archive_window(&self.app, &label, None).is_err() {
            self.states.release(&label);
        }
    }

    fn create_window(&self, label: &str, opener: Option<&str>) -> bool {
        build_archive_window(&self.app, label, opener).is_ok()
    }

    fn focus_window(&self, label: &str) -> bool {
        self.bring_forward(label)
    }

    fn window_exists(&self, label: &str) -> bool {
        use tauri::Manager;
        self.app.get_webview_window(label).is_some()
    }

    fn set_title(&self, label: &str, title: &str) {
        use tauri::Manager;
        if let Some(window) = self.app.get_webview_window(label) {
            let _ = window.set_title(title);
        }
    }

    fn open_folder(&self, folder: &std::path::Path) -> std::io::Result<()> {
        use help::Opener;
        #[cfg(feature = "e2e")]
        if let Some(log) = std::env::var_os("MOCHI_E2E_OPEN_LOG") {
            return help::RecordingOpener(log.into()).open(folder);
        }
        help::OsOpener.open(folder)
    }
}
