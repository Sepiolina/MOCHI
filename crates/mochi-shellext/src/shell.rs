//! The COM side: class factory, `IShellExtInit` and `IContextMenu`. Everything
//! unsafe in this crate is here, each block with a `SAFETY:` comment, and every
//! entry point that Explorer can call is wrapped in `catch_unwind` so that no
//! panic crosses into the shell (a failure answers `E_FAIL`).

use std::ffi::{c_void, CStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use windows::core::{implement, Interface, Ref, Result, BOOL, GUID, HRESULT, PCWSTR, PSTR, PWSTR};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_FAIL, E_INVALIDARG, E_NOTIMPL, HMODULE,
    S_FALSE, S_OK, TRUE,
};
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HGDIOBJ,
};
use windows::Win32::System::Com::{
    IClassFactory, IClassFactory_Impl, IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL,
};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Ole::{ReleaseStgMedium, CF_HDROP};
use windows::Win32::System::Registry::HKEY;
use windows::Win32::System::SystemServices::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    DragQueryFileW, ExtractIconExW, IContextMenu, IContextMenu_Impl, IShellExtInit,
    IShellExtInit_Impl, SHGetPathFromIDListW, CMF_DEFAULTONLY, CMINVOKECOMMANDINFO, GCS_HELPTEXTA,
    GCS_HELPTEXTW, GCS_VALIDATEA, GCS_VALIDATEW, GCS_VERBA, GCS_VERBW, HDROP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, DrawIconEx, GetSystemMetrics, InsertMenuItemW, DI_NORMAL, HICON, HMENU,
    MENUITEMINFOW, MFT_STRING, MIIM_BITMAP, MIIM_FTYPE, MIIM_ID, MIIM_STRING, SM_CXSMICON,
    SM_CYSMICON,
};

use crate::plan::{self, Item};

/// The class id registered by `packaging/nsis/hooks.nsh` (MOCHI_SHELLEXT_CLSID).
const CLSID_MOCHI_MENU: GUID = GUID::from_u128(0x508772cd_d924_4b90_8b87_37fd5d593932);

/// The app next to this DLL.
const APP_EXE: &str = "mochi-desktop.exe";

/// More items than this, or a command line that Windows (32,767 UTF-16 units)
/// could not carry, show no MOCHI entries at all: a launch that cannot start
/// is worse than a missing entry.
const MAX_ITEMS: usize = 2048;
const MAX_COMMAND_UNITS: usize = 30_000;

/// The DLL's own module handle (set once, in `DllMain`).
static MODULE: AtomicIsize = AtomicIsize::new(0);
/// Live menu objects and `LockServer` locks, for `DllCanUnloadNow`.
static LIVE_OBJECTS: AtomicU32 = AtomicU32::new(0);
static SERVER_LOCKS: AtomicU32 = AtomicU32::new(0);

fn guard<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| Err(E_FAIL.into()))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------- exports

/// # Safety
/// Called by the loader with the DLL's own handle.
#[no_mangle]
pub unsafe extern "system" fn DllMain(hinst: HMODULE, reason: u32, reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(hinst.0 as isize, Ordering::Relaxed);
    } else if reason == DLL_PROCESS_DETACH && reserved.is_null() {
        // FreeLibrary (not process exit): release the cached icon bitmap.
        let _ = catch_unwind(free_menu_icon);
    }
    TRUE
}

/// # Safety
/// `rclsid`, `riid` and `ppv` are the COM caller's pointers, each valid or null.
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    catch_unwind(AssertUnwindSafe(|| {
        if rclsid.is_null() || riid.is_null() || ppv.is_null() {
            return E_INVALIDARG;
        }
        // SAFETY: `ppv` is non-null and, by the COM contract, writable.
        unsafe { *ppv = std::ptr::null_mut() };
        // SAFETY: `rclsid` is non-null and points to a GUID for this call.
        if unsafe { *rclsid } != CLSID_MOCHI_MENU {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        let factory: IClassFactory = Factory.into();
        // SAFETY: `riid` and `ppv` were checked above; `query` writes an
        // owned interface pointer into `ppv` on success.
        unsafe { factory.query(riid, ppv) }
    }))
    .unwrap_or(E_FAIL)
}

#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    catch_unwind(|| {
        if LIVE_OBJECTS.load(Ordering::Acquire) == 0 && SERVER_LOCKS.load(Ordering::Acquire) == 0 {
            S_OK
        } else {
            S_FALSE
        }
    })
    .unwrap_or(S_FALSE)
}

// ---------------------------------------------------------------- factory

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<'_, windows::core::IUnknown>,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> Result<()> {
        guard(|| {
            if riid.is_null() || ppv.is_null() {
                return Err(E_INVALIDARG.into());
            }
            // SAFETY: `ppv` is non-null and writable by the COM contract.
            unsafe { *ppv = std::ptr::null_mut() };
            if outer.is_some() {
                return Err(CLASS_E_NOAGGREGATION.into());
            }
            let unknown: windows::core::IUnknown = MochiMenu::new().into();
            // SAFETY: `riid` and `ppv` were checked; `query` stores an owned
            // interface pointer in `ppv` on success.
            unsafe { unknown.query(riid, ppv).ok() }
        })
    }

    fn LockServer(&self, lock: BOOL) -> Result<()> {
        guard(|| {
            if lock.as_bool() {
                SERVER_LOCKS.fetch_add(1, Ordering::AcqRel);
            } else {
                // Saturating: an unbalanced unlock must not wrap to "locked forever".
                let _ = SERVER_LOCKS
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
            }
            Ok(())
        })
    }
}

// ---------------------------------------------------------------- the menu

#[implement(IShellExtInit, IContextMenu)]
struct MochiMenu {
    items: Mutex<Vec<Item>>,
    /// The menu is a folder's background (no selection): the one item is that folder.
    background: AtomicBool,
}

impl MochiMenu {
    fn new() -> Self {
        LIVE_OBJECTS.fetch_add(1, Ordering::AcqRel);
        Self {
            items: Mutex::new(Vec::new()),
            background: AtomicBool::new(false),
        }
    }
}

impl Drop for MochiMenu {
    fn drop(&mut self) {
        let _ =
            LIVE_OBJECTS.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
    }
}

/// The menu language: the user's Windows display language. The app's own
/// language setting lives in the webview and is not readable from here.
fn display_language() -> plan::Lang {
    // SAFETY: takes no arguments, reads no memory of ours, and cannot fail.
    plan::Lang::from_langid(unsafe { GetUserDefaultUILanguage() })
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The selected paths from the data object's `CF_HDROP`.
fn read_selection(data: &IDataObject) -> Result<Vec<PathBuf>> {
    let format = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    // SAFETY: `format` is a fully initialised FORMATETC that lives across the call.
    let mut medium = unsafe { data.GetData(&format)? };
    // SAFETY: the medium was requested as TYMED_HGLOBAL, so `hGlobal` is the
    // active union member; a CF_HDROP global is a valid HDROP.
    let hdrop = HDROP(unsafe { medium.u.hGlobal.0 });
    let paths = (|| {
        // SAFETY: `hdrop` stays valid until ReleaseStgMedium below. Index
        // 0xFFFFFFFF asks for the count.
        let count = unsafe { DragQueryFileW(hdrop, u32::MAX, None) } as usize;
        if count == 0 || count > MAX_ITEMS {
            return Vec::new();
        }
        let mut paths = Vec::with_capacity(count);
        for i in 0..count {
            // SAFETY: `i` < count; a `None` buffer returns the length in units.
            let len = unsafe { DragQueryFileW(hdrop, i as u32, None) } as usize;
            if len == 0 {
                continue;
            }
            let mut buf = vec![0u16; len + 1];
            // SAFETY: `buf` has room for `len` units plus the terminator.
            let got = unsafe { DragQueryFileW(hdrop, i as u32, Some(&mut buf)) } as usize;
            buf.truncate(got.min(len));
            paths.push(PathBuf::from(OsString::from_wide(&buf)));
        }
        paths
    })();
    // SAFETY: `medium` came from GetData above and is released exactly once.
    unsafe { ReleaseStgMedium(&mut medium) };
    Ok(paths)
}

fn command_units(paths: &[PathBuf]) -> usize {
    paths
        .iter()
        .map(|p| p.as_os_str().encode_wide().count() + 3)
        .sum::<usize>()
        + APP_EXE.len()
        + 32
}

/// Most items asked of the file system whether they are folders (each is one
/// `metadata` call, made while Explorer waits to draw its menu). Items past
/// this are treated as files, so a huge selection may lack the Find entry.
const MAX_STAT: usize = 256;

/// The folder behind a background menu's item-ID list, or `None` (a virtual
/// folder such as "This PC", a path past MAX_PATH, or no list at all).
fn background_folder(pidl: *const ITEMIDLIST) -> Option<PathBuf> {
    if pidl.is_null() {
        return None;
    }
    let mut buf = [0u16; 260];
    // SAFETY: `pidl` is the item-ID list the shell passed to `Initialize` for
    // this call (checked non-null); `buf` is the MAX_PATH-unit buffer the call
    // requires, and it writes a terminated string into it on success.
    let found = unsafe { SHGetPathFromIDListW(pidl, &mut buf) }.as_bool();
    if !found {
        return None;
    }
    let len = buf.iter().position(|&u| u == 0)?;
    (len > 0).then(|| PathBuf::from(OsString::from_wide(&buf[..len])))
}

impl IShellExtInit_Impl for MochiMenu_Impl {
    fn Initialize(
        &self,
        folder: *const ITEMIDLIST,
        data: Ref<'_, IDataObject>,
        _prog_id: HKEY,
    ) -> Result<()> {
        guard(|| {
            let Some(data) = data.as_ref() else {
                // A folder's background has no selection: the shell names the
                // open folder instead. Only the Find entry is offered there.
                let Some(path) = background_folder(folder) else {
                    return Err(E_INVALIDARG.into());
                };
                *lock(&self.items) = vec![Item { path, is_dir: true }];
                self.background.store(true, Ordering::Release);
                return Ok(());
            };
            self.background.store(false, Ordering::Release);
            let paths = read_selection(data)?;
            let too_long = command_units(&paths) > MAX_COMMAND_UNITS;
            let items = if too_long {
                Vec::new()
            } else {
                // Whether an item is a folder decides its label (a lone item)
                // and whether Find is offered, so each is asked, up to a limit.
                paths
                    .into_iter()
                    .enumerate()
                    .map(|(n, path)| {
                        let is_dir =
                            n < MAX_STAT && std::fs::metadata(&path).is_ok_and(|m| m.is_dir());
                        Item { path, is_dir }
                    })
                    .collect()
            };
            *lock(&self.items) = items;
            Ok(())
        })
    }
}

/// The cached menu bitmap: 0 = not tried yet, -1 = tried and failed (no icon
/// in the menu), otherwise an `HBITMAP`. One per process, freed on DLL unload.
static MENU_BITMAP: AtomicIsize = AtomicIsize::new(0);

/// The MOCHI icon as a 32-bit premultiplied-alpha bitmap at the small-icon
/// size, or `None`. Any failure means "no icon", never a failed menu.
fn menu_icon() -> Option<HBITMAP> {
    match MENU_BITMAP.load(Ordering::Acquire) {
        0 => {
            let made = catch_unwind(build_menu_icon).ok().flatten();
            let raw = made.map_or(-1, |b| b.0 as isize);
            // Two threads may race: the loser frees its copy.
            match MENU_BITMAP.compare_exchange(0, raw, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => made,
                Err(current) => {
                    if let Some(b) = made {
                        // SAFETY: `b` was created above and never shared.
                        let _ = unsafe { DeleteObject(HGDIOBJ(b.0)) };
                    }
                    (current > 0).then_some(HBITMAP(current as *mut c_void))
                }
            }
        }
        n if n > 0 => Some(HBITMAP(n as *mut c_void)),
        _ => None,
    }
}

fn free_menu_icon() {
    let raw = MENU_BITMAP.swap(0, Ordering::AcqRel);
    if raw > 0 {
        // SAFETY: the handle came from `build_menu_icon` and was swapped out,
        // so it is deleted once.
        let _ = unsafe { DeleteObject(HGDIOBJ(raw as *mut c_void)) };
    }
}

/// Small icon of the app's exe (resource 0, the one `Icon` values name),
/// drawn into a top-down 32-bit DIB section.
fn build_menu_icon() -> Option<HBITMAP> {
    let exe = wide_path(&app_path()?);
    let mut small = HICON::default();
    // SAFETY: `exe` is NUL-terminated; one small-icon out slot is supplied.
    let got = unsafe { ExtractIconExW(PCWSTR(exe.as_ptr()), 0, None, Some(&mut small), 1) };
    if got == 0 || small.is_invalid() {
        return None;
    }
    let bitmap = icon_to_bitmap(small);
    // SAFETY: `small` was returned by ExtractIconExW and is destroyed once.
    let _ = unsafe { DestroyIcon(small) };
    bitmap
}

fn wide_path(p: &std::path::Path) -> Vec<u16> {
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn icon_to_bitmap(icon: HICON) -> Option<HBITMAP> {
    // SAFETY: plain metric queries.
    let (w, h) = unsafe { (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)) };
    if !(8..=256).contains(&w) || !(8..=256).contains(&h) {
        return None;
    }
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    // SAFETY: `info` is initialised; `bits` receives the DIB's pixel pointer.
    let dib = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) }.ok()?;
    if bits.is_null() {
        // SAFETY: `dib` was just created.
        let _ = unsafe { DeleteObject(HGDIOBJ(dib.0)) };
        return None;
    }
    // SAFETY: a memory DC compatible with the screen.
    let dc = unsafe { CreateCompatibleDC(None) };
    if dc.is_invalid() {
        // SAFETY: `dib` was just created.
        let _ = unsafe { DeleteObject(HGDIOBJ(dib.0)) };
        return None;
    }
    // SAFETY: `dc` and `dib` are valid; the old object is restored before the DC goes.
    let old = unsafe { SelectObject(dc, HGDIOBJ(dib.0)) };
    // SAFETY: the DIB is selected into `dc`; `icon` is valid. Drawing onto the
    // zeroed DIB leaves premultiplied alpha for an icon with an alpha channel.
    let drawn = unsafe { DrawIconEx(dc, 0, 0, icon, w, h, 0, None, DI_NORMAL) }.is_ok();
    // SAFETY: restores the DC's original bitmap, then deletes the DC once.
    unsafe {
        SelectObject(dc, old);
        let _ = DeleteDC(dc);
    }
    if drawn {
        // SAFETY: the DIB section owns w * h 32-bit pixels at `bits`, no longer
        // selected into any DC.
        let px = unsafe { std::slice::from_raw_parts_mut(bits.cast::<u32>(), (w * h) as usize) };
        // An icon without an alpha channel leaves alpha 0 everywhere: make the
        // painted pixels opaque.
        if px.iter().all(|p| p >> 24 == 0) {
            for p in px.iter_mut().filter(|p| **p != 0) {
                *p |= 0xFF00_0000;
            }
        }
        return Some(dib);
    }
    // SAFETY: `dib` is ours and unused.
    let _ = unsafe { DeleteObject(HGDIOBJ(dib.0)) };
    None
}

/// The app that sits next to this DLL.
fn app_path() -> Option<PathBuf> {
    let module = HMODULE(MODULE.load(Ordering::Relaxed) as *mut c_void);
    let mut buf = vec![0u16; 32_768];
    // SAFETY: `buf` is writable for its whole length; the call writes at most that many units.
    let len = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    if len == 0 || len >= buf.len() {
        return None;
    }
    let dll = PathBuf::from(OsString::from_wide(&buf[..len]));
    Some(dll.parent()?.join(APP_EXE))
}

/// Starts the app with `verb -- PATH...` in one process. No shell, no string
/// built from the paths.
fn launch(verb: &str, paths: &[PathBuf]) -> Result<()> {
    let exe = app_path().ok_or(E_FAIL)?;
    Command::new(exe)
        .args(plan::launch_args(verb, paths))
        .spawn()
        .map(drop)
        .map_err(|_| E_FAIL.into())
}

use plan::{CMD_COUNT, CMD_FIND, CMD_NOW, CMD_PANEL};

fn verb_for(offset: u32) -> Option<&'static str> {
    match offset {
        CMD_PANEL => Some(plan::VERB_PANEL),
        CMD_NOW => Some(plan::VERB_NOW),
        CMD_FIND => Some(plan::VERB_FIND),
        _ => None,
    }
}

fn canonical(offset: usize) -> Option<&'static str> {
    match offset as u32 {
        CMD_PANEL => Some(plan::CANONICAL_PANEL),
        CMD_NOW => Some(plan::CANONICAL_NOW),
        CMD_FIND => Some(plan::CANONICAL_FIND),
        _ => None,
    }
}

impl IContextMenu_Impl for MochiMenu_Impl {
    fn QueryContextMenu(
        &self,
        menu: HMENU,
        index: u32,
        first_id: u32,
        last_id: u32,
        flags: u32,
    ) -> HRESULT {
        catch_unwind(AssertUnwindSafe(|| {
            let none = HRESULT(0);
            if flags & CMF_DEFAULTONLY != 0 {
                return none;
            }
            let items = lock(&self.items).clone();
            if items.is_empty() || last_id < first_id || last_id - first_id + 1 < CMD_COUNT {
                return none;
            }
            let rows = plan::menu_rows(
                &items,
                display_language(),
                self.background.load(Ordering::Acquire),
            );
            if rows.is_empty() {
                return none;
            }
            let icon = menu_icon();
            for (n, entry) in rows.iter().enumerate() {
                let mut text = wide(&entry.label);
                let mut mii = MENUITEMINFOW {
                    cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                    fMask: MIIM_FTYPE | MIIM_ID | MIIM_STRING,
                    fType: MFT_STRING,
                    wID: first_id + entry.offset,
                    dwTypeData: PWSTR(text.as_mut_ptr()),
                    ..Default::default()
                };
                if let Some(bitmap) = icon {
                    mii.fMask |= MIIM_BITMAP;
                    mii.hbmpItem = bitmap;
                }
                // SAFETY: `menu` is the shell's live menu; `mii` is fully
                // initialised, `text` is NUL-terminated and outlives the call,
                // which copies it. The bitmap is owned by the DLL-wide cache and
                // stays alive until the DLL unloads.
                let added = unsafe { InsertMenuItemW(menu, index + n as u32, true, &mii) };
                if added.is_err() {
                    return none;
                }
            }
            // MAKE_HRESULT(SEVERITY_SUCCESS, FACILITY_NULL, number of ids used).
            HRESULT(plan::ids_used(&rows) as i32)
        }))
        .unwrap_or(E_FAIL)
    }

    fn InvokeCommand(&self, info: *const CMINVOKECOMMANDINFO) -> Result<()> {
        guard(|| {
            if info.is_null() {
                return Err(E_INVALIDARG.into());
            }
            // SAFETY: the shell passes a CMINVOKECOMMANDINFO (or the larger
            // Ex form, which begins with the same fields) valid for this call.
            let info = unsafe { &*info };
            let verb = info.lpVerb.0;
            let offset = if (verb as usize) >> 16 == 0 {
                (verb as usize) as u32
            } else {
                // SAFETY: a non-index lpVerb is a NUL-terminated ANSI string.
                let name = unsafe { CStr::from_ptr(verb.cast()) };
                match name.to_bytes() {
                    b if b == plan::CANONICAL_PANEL.as_bytes() => CMD_PANEL,
                    b if b == plan::CANONICAL_NOW.as_bytes() => CMD_NOW,
                    b if b == plan::CANONICAL_FIND.as_bytes() => CMD_FIND,
                    _ => return Err(E_INVALIDARG.into()),
                }
            };
            let verb = verb_for(offset).ok_or(E_INVALIDARG)?;
            let items = lock(&self.items).clone();
            if items.is_empty() {
                return Err(E_FAIL.into());
            }
            // Find is given the folders only; Add gets the whole selection.
            let paths = if offset == CMD_FIND {
                plan::find_paths(&items)
            } else {
                items.iter().map(|i| i.path.clone()).collect()
            };
            if paths.is_empty() {
                return Err(E_FAIL.into());
            }
            launch(verb, &paths)
        })
    }

    fn GetCommandString(
        &self,
        id: usize,
        kind: u32,
        _reserved: *const u32,
        name: PSTR,
        cch: u32,
    ) -> Result<()> {
        guard(|| {
            let Some(verb) = canonical(id) else {
                return Err(E_INVALIDARG.into());
            };
            let help = match id as u32 {
                CMD_PANEL => "Choose options and add to an archive",
                CMD_FIND => "Search the file names inside every MOCHI archive in the folders",
                _ => "Add to a new archive next to the selection",
            };
            let (text, unicode) = match kind {
                k if k == GCS_VERBA => (verb, false),
                k if k == GCS_VERBW => (verb, true),
                k if k == GCS_HELPTEXTA => (help, false),
                k if k == GCS_HELPTEXTW => (help, true),
                k if k == GCS_VALIDATEA || k == GCS_VALIDATEW => return Ok(()),
                _ => return Err(E_NOTIMPL.into()),
            };
            if name.is_null() || cch == 0 {
                return Err(E_INVALIDARG.into());
            }
            let cch = cch as usize;
            if unicode {
                let units: Vec<u16> = text.encode_utf16().take(cch - 1).collect();
                // SAFETY: the caller's buffer holds `cch` UTF-16 units; at most
                // cch - 1 are copied, then the terminator.
                unsafe {
                    let dst = name.0.cast::<u16>();
                    std::ptr::copy_nonoverlapping(units.as_ptr(), dst, units.len());
                    *dst.add(units.len()) = 0;
                }
            } else {
                let bytes: Vec<u8> = text.bytes().take(cch - 1).collect();
                // SAFETY: the caller's buffer holds `cch` bytes; at most cch - 1
                // are copied, then the terminator.
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), name.0, bytes.len());
                    *name.0.add(bytes.len()) = 0;
                }
            }
            Ok(())
        })
    }
}
