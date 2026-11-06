//! Windows desktop access: reads and places icons through Explorer's own desktop view,
//! so the list matches what is on screen (user + Public desktop, Recycle Bin, OneDrive).
use std::{collections::HashMap, ops::DerefMut, os::windows::ffi::OsStrExt, path::{Path, PathBuf}};
use windows::{
    core::{Interface, PCWSTR, PWSTR},
    Win32::{
        Foundation::{HWND, POINT},
        Graphics::Gdi::ScreenToClient,
        System::{
            Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IPersistFile, IServiceProvider, CLSCTX_ALL, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, STGM_READ},
            Variant::{VARIANT, VT_I4},
        },
        UI::{
            Shell::{
                Common::{ITEMIDLIST, STRRET}, IFolderView2, IShellBrowser, IShellFolder, IShellLinkW, IShellView, IShellWindows,
                ShellLink, ShellWindows, StrRetToStrW, FWF_AUTOARRANGE, FWF_SNAPTOGRID, SHGDNF, SHGDN_FORPARSING, SHGDN_NORMAL,
                SID_STopLevelBrowser, SVGIO_ALLVIEW, SVSI_POSITIONITEM, SWC_DESKTOP, SWFO_NEEDDISPATCH,
            },
            WindowsAndMessaging::{FindWindowExW, SystemParametersInfoW, SPI_GETDESKWALLPAPER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS},
        },
    },
};

pub enum ItemKind { App, Folder, File }
pub struct DesktopItem { pub path: PathBuf, pub name: String, pub kind: ItemKind }

const VIEW_ERROR: &str = "바탕화면을 읽지 못했습니다. 탐색기가 실행 중인지 확인해 주세요.";
// Largest gap, in pixels, between the requested and the reported spot that still counts as placed.
const PLACE_TOLERANCE: i32 = 8;
// Wallpapers beyond this size are skipped: the preview is sent to the page as a data URL.
const WALLPAPER_LIMIT: u64 = 16 * 1024 * 1024;

/// An item id owned by this process; Explorer hands these out with CoTaskMemAlloc.
struct ItemId(*mut ITEMIDLIST);
impl Drop for ItemId { fn drop(&mut self) { unsafe { CoTaskMemFree(Some(self.0 as *const _)); } } }

/// Shell objects need an initialized apartment, so each call gets its own short-lived thread.
fn on_com_thread<T: Send + 'static>(work: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    std::thread::spawn(move || {
        let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
        let result = work();
        if initialized { unsafe { CoUninitialize(); } }
        result
    }).join().map_err(|_| VIEW_ERROR.to_string())?
}

fn wide(path: &Path) -> Vec<u16> { path.as_os_str().encode_wide().chain(Some(0)).collect() }

fn desktop_view() -> windows::core::Result<IFolderView2> {
    unsafe {
        let shell: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL)?;
        let mut location = VARIANT::default();
        location.Anonymous.Anonymous.deref_mut().vt = VT_I4; // CSIDL_DESKTOP is 0, already the zeroed value
        let root = VARIANT::default();
        let mut window = 0;
        let dispatch = shell.FindWindowSW(&location, &root, SWC_DESKTOP, &mut window, SWFO_NEEDDISPATCH)?;
        let provider: IServiceProvider = dispatch.cast()?;
        let browser: IShellBrowser = provider.QueryService(&SID_STopLevelBrowser)?;
        browser.QueryActiveShellView()?.cast()
    }
}

fn display_name(folder: &IShellFolder, item: &ItemId, flags: SHGDNF) -> Option<String> {
    unsafe {
        let mut value = STRRET::default();
        folder.GetDisplayNameOf(item.0, flags, &mut value).ok()?;
        let mut text = PWSTR::null();
        StrRetToStrW(&mut value, Some(item.0), &mut text).ok()?;
        let name = text.to_string().ok();
        CoTaskMemFree(Some(text.0 as *const _));
        name
    }
}

/// Every icon on the desktop with its parsing path (a "::{GUID}" for Recycle Bin and the like).
fn view_items(view: &IFolderView2) -> windows::core::Result<Vec<(ItemId, PathBuf, String)>> {
    unsafe {
        let folder: IShellFolder = view.GetFolder()?;
        let mut items = Vec::new();
        for index in 0..view.ItemCount(SVGIO_ALLVIEW)? {
            let item = ItemId(view.Item(index)?);
            let Some(path) = display_name(&folder, &item, SHGDN_FORPARSING) else { continue; };
            let name = display_name(&folder, &item, SHGDN_NORMAL).unwrap_or_else(|| path.clone());
            items.push((item, PathBuf::from(path), name));
        }
        Ok(items)
    }
}

fn shortcut_target(path: &Path) -> Option<PathBuf> {
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let file: IPersistFile = link.cast().ok()?;
        file.Load(PCWSTR(wide(path).as_ptr()), STGM_READ).ok()?;
        let mut buffer = [0u16; 1024];
        link.GetPath(&mut buffer, std::ptr::null_mut(), 0).ok()?;
        let end = buffer.iter().position(|unit| *unit == 0)?;
        (end > 0).then(|| PathBuf::from(String::from_utf16_lossy(&buffer[..end])))
    }
}

fn has_extension(path: &Path, names: &[&str]) -> bool {
    path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| names.iter().any(|name| ext.eq_ignore_ascii_case(name)))
}

/// Shortcuts count as what they open. Store apps and virtual icons have no file path, so they are apps.
fn classify(path: &Path) -> ItemKind {
    if !path.is_absolute() { return ItemKind::App; }
    if path.is_dir() { return ItemKind::Folder; }
    if has_extension(path, &["exe", "url", "appref-ms"]) { return ItemKind::App; }
    if !has_extension(path, &["lnk"]) { return ItemKind::File; }
    match shortcut_target(path) {
        None => ItemKind::App,
        Some(target) if target.is_dir() => ItemKind::Folder,
        Some(target) if has_extension(&target, &["exe", "bat", "cmd", "msc"]) => ItemKind::App,
        Some(_) => ItemKind::File,
    }
}

pub fn items() -> Result<Vec<DesktopItem>, String> {
    on_com_thread(|| {
        let view = desktop_view().map_err(|_| VIEW_ERROR.to_string())?;
        let items = view_items(&view).map_err(|_| VIEW_ERROR.to_string())?;
        Ok(items.into_iter().map(|(_, path, name)| { let kind = classify(&path); DesktopItem { path, name, kind } }).collect())
    })
}

/// Moves each item so its icon cell is centered on the given spot (logical screen units).
/// Returns the names of items that did not end up there.
pub fn place(positions: Vec<(PathBuf, i32, i32)>, scale: f64) -> Result<Vec<String>, String> {
    on_com_thread(move || unsafe {
        let view = desktop_view().map_err(|_| VIEW_ERROR.to_string())?;
        let items = view_items(&view).map_err(|_| VIEW_ERROR.to_string())?;
        let by_path: HashMap<&Path, (&ItemId, &str)> = items.iter().map(|(item, path, name)| (path.as_path(), (item, name.as_str()))).collect();

        // Auto arrange and snap-to-grid would pull every icon straight back onto Explorer's grid.
        view.SetCurrentFolderFlags((FWF_AUTOARRANGE.0 | FWF_SNAPTOGRID.0) as u32, 0).map_err(|_| "바탕화면의 '아이콘 자동 정렬'을 끄지 못했습니다. 바탕화면 우클릭 > 보기에서 직접 꺼 주세요.".to_string())?;
        let mut cell = POINT::default();
        view.GetSpacing(&mut cell).map_err(|_| VIEW_ERROR.to_string())?;

        // Item positions are relative to the icon list, which starts at the leftmost monitor.
        let shell_view: IShellView = view.cast().map_err(|_| VIEW_ERROR.to_string())?;
        let host: HWND = shell_view.GetWindow().map_err(|_| VIEW_ERROR.to_string())?;
        let list = FindWindowExW(Some(host), None, windows::core::w!("SysListView32"), None).map_err(|_| VIEW_ERROR.to_string())?;

        let mut failed = Vec::new();
        for (path, x, y) in &positions {
            let Some((item, name)) = by_path.get(path.as_path()) else {
                failed.push(path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string_lossy().into_owned()));
                continue;
            };
            let mut spot = POINT { x: (*x as f64 * scale).round() as i32 - cell.x / 2, y: (*y as f64 * scale).round() as i32 - cell.y / 2 };
            if !ScreenToClient(list, &mut spot).as_bool() { failed.push((*name).to_string()); continue; }
            let id = item.0 as *const ITEMIDLIST;
            let placed = view.SelectAndPositionItems(1, &id, Some(&spot), SVSI_POSITIONITEM.0 as u32).is_ok()
                && view.GetItemPosition(id).is_ok_and(|at| (at.x - spot.x).abs() <= PLACE_TOLERANCE && (at.y - spot.y).abs() <= PLACE_TOLERANCE);
            if !placed { failed.push((*name).to_string()); }
        }
        Ok(failed)
    })
}

fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0xFF, 0xD8, ..] => Some("jpeg"),
        [0x89, b'P', b'N', b'G', ..] => Some("png"),
        [b'B', b'M', ..] => Some("bmp"),
        [b'G', b'I', b'F', ..] => Some("gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("webp"),
        _ => None,
    }
}

/// The current wallpaper as a data URL. A solid color has no file, so it gives None.
pub fn wallpaper() -> Option<String> {
    use base64::Engine;
    let mut buffer = [0u16; 1024];
    unsafe { SystemParametersInfoW(SPI_GETDESKWALLPAPER, buffer.len() as u32, Some(buffer.as_mut_ptr().cast()), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)) }.ok()?;
    let end = buffer.iter().position(|unit| *unit == 0)?;
    if end == 0 { return None; }
    let path = PathBuf::from(String::from_utf16_lossy(&buffer[..end]));
    if std::fs::metadata(&path).ok()?.len() > WALLPAPER_LIMIT { return None; }
    let bytes = std::fs::read(&path).ok()?;
    Some(format!("data:image/{};base64,{}", image_mime(&bytes)?, base64::engine::general_purpose::STANDARD.encode(&bytes)))
}
