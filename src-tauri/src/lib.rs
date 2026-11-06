use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs::File, io::Read, path::{Component, Path, PathBuf}, sync::{atomic::{AtomicBool, AtomicU64, Ordering}, Arc, Mutex}};
use tauri::{Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;
use walkdir::WalkDir;

#[cfg(windows)] mod windows_desktop;

/// Real path without the \\?\ prefix Windows would otherwise add.
trait CanonicalPath { fn canonical(&self) -> std::io::Result<PathBuf>; }
impl CanonicalPath for Path { fn canonical(&self) -> std::io::Result<PathBuf> { dunce::canonicalize(self) } }

#[derive(Clone)] struct ScanState { cancel: Arc<AtomicBool>, db_path: Arc<Mutex<Option<PathBuf>>>, selected_root: Arc<Mutex<Option<PathBuf>>>, plans: Arc<Mutex<HashMap<String, Plan>>>, selected_files: Arc<Mutex<Vec<String>>>, operations: Arc<Mutex<()>>, duplicate_cancel: Arc<AtomicBool>, move_destination: Arc<Mutex<Option<PathBuf>>> }
#[derive(Clone, Serialize)] struct Entry { path: String, name: String, extension: Option<String>, size: u64, modified: Option<i64>, category: String }
#[derive(Clone, Serialize)] struct ScanEvent { kind: String, item: Option<Entry>, scanned: usize, message: Option<String> }
#[derive(Clone, Serialize)] struct DuplicateFile { path: String, size: u64, modified: Option<i64> }
#[derive(Clone, Serialize)] struct DuplicateGroup { sha256: String, files: Vec<DuplicateFile> }
#[derive(Clone, Serialize)] struct DuplicateProgress { stage: String, scanned: usize, total: usize }
#[derive(Clone, Serialize)] struct DuplicateScanResult { groups: Vec<DuplicateGroup>, skipped: usize }
#[derive(Clone, Deserialize)] struct OperationRequest { source: String, target: String }
#[derive(Clone, Serialize)] struct PlanItem { source: String, target: String, action: String, size: u64, modified_ns: Option<u128>, #[serde(skip_serializing)] sha256: String, status: String, error: Option<String> }
#[derive(Clone, Serialize)] struct Plan { id: String, items: Vec<PlanItem>, status: String, created: i64, #[serde(skip_serializing)] root: String }
trait AnalysisProvider { fn availability(&self) -> &'static str; }
struct UnavailableProvider;
impl AnalysisProvider for UnavailableProvider { fn availability(&self) -> &'static str { "unavailable-no-model-installed" } }
#[derive(Serialize)] struct AnalysisAvailability { status: &'static str, supports_images: bool, supports_documents: bool, network_enabled: bool }
#[tauri::command]
fn analysis_availability() -> AnalysisAvailability {
    let provider = UnavailableProvider;
    AnalysisAvailability { status: provider.availability(), supports_images: false, supports_documents: false, network_enabled: false }
}
static PLAN_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Serialize)] struct CollectedFile { source: String, target: String, status: String, error: Option<String> }
#[derive(Clone, Serialize)] struct CollectionResult { folder: String, items: Vec<CollectedFile>, job_id: Option<String> }
/// Where picked files go: a new folder on the desktop, or a folder chosen in the native picker.
#[derive(Deserialize)] #[serde(tag = "kind", rename_all = "lowercase")] enum MoveDestination { New { name: String }, Existing { path: String } }
#[derive(Clone, Serialize)] struct CollectProgress { done: usize, total: usize, name: String }
#[derive(Clone, Serialize)] struct DesktopArrangeSummary { apps: usize, folders: usize, files: usize, total: usize, failed: Vec<String> }

#[derive(Clone)] struct DesktopEntry { path: PathBuf, name: String, color: usize }
#[derive(Clone, Deserialize)] struct ScreenRegion { x: f64, y: f64, width: f64, height: f64 }
#[derive(Clone, Copy)] struct DisplayArea { left: f64, top: f64, width: f64, height: f64 }
#[derive(Serialize)] struct DesktopScreenInfo { width: f64, height: f64 }

#[tauri::command]
fn request_screen_capture_permission() -> bool {
    #[cfg(target_os = "macos")]
    {
        // CGRequestScreenCaptureAccess may return false even when TCC already
        // has a grant. Check the existing grant first, and re-check after the
        // request so the UI follows the current permission state.
        let allowed = objc2_core_graphics::CGPreflightScreenCaptureAccess()
            || objc2_core_graphics::CGRequestScreenCaptureAccess()
            || objc2_core_graphics::CGPreflightScreenCaptureAccess();
        if !allowed {
            let _ = std::process::Command::new("/usr/bin/open")
                .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
                .spawn();
        }
        allowed
    }
    #[cfg(not(target_os = "macos"))]
    { false }
}

#[derive(Clone, Copy, Serialize)] struct SceneRect { x: f64, y: f64, width: f64, height: f64 }
#[derive(Serialize)] struct SceneWidget { name: String, bounds: SceneRect, image: Option<String> }
#[derive(Serialize)] struct DesktopScene { display: SceneRect, work: SceneRect, wallpaper: Option<String>, icons: Option<String>, widgets: Vec<SceneWidget>, permission: bool }

#[cfg(target_os = "macos")]
mod desktop_windows {
    use std::ffi::c_void;
    #[repr(C)] #[derive(Default, Clone, Copy)] pub struct CGRect { pub x: f64, pub y: f64, pub width: f64, pub height: f64 }
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> *const c_void;
        fn CGRectMakeWithDictionaryRepresentation(dict: *const c_void, rect: *mut CGRect) -> bool;
        fn CGWindowLevelForKey(key: i32) -> i32;
        static kCGWindowNumber: *const c_void;
        static kCGWindowLayer: *const c_void;
        static kCGWindowBounds: *const c_void;
        static kCGWindowOwnerPID: *const c_void;
        static kCGWindowName: *const c_void;
        static kCGWindowIsOnscreen: *const c_void;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFArrayGetCount(array: *const c_void) -> isize;
        fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
        fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
        fn CFNumberGetValue(number: *const c_void, kind: isize, value: *mut c_void) -> bool;
        fn CFStringGetCString(string: *const c_void, buffer: *mut u8, size: isize, encoding: u32) -> bool;
        fn CFBooleanGetValue(boolean: *const c_void) -> bool;
        fn CFRelease(value: *const c_void);
    }

    #[derive(Clone, Copy, PartialEq)] pub enum Owner { WindowManager, WallpaperAgent, Finder, NotificationCenter, Other }
    #[derive(Clone)] pub struct WindowInfo { pub id: u32, pub layer: i32, pub owner: Owner, pub name: String, pub onscreen: bool, pub bounds: CGRect }

    /// Desktop widgets are Notification Center windows just above the desktop-icon level.
    pub fn widgets<'a>(windows: &'a [WindowInfo], screen: &super::SceneRect) -> Vec<&'a WindowInfo> {
        let icon_level = desktop_icon_level();
        windows.iter().filter(|w| w.owner == Owner::NotificationCenter && w.layer > icon_level && w.layer < 0
            && w.bounds.width > 0.0 && w.bounds.height > 0.0
            && w.bounds.width < screen.width * 0.9 && w.bounds.height < screen.height * 0.9
            && w.bounds.x < screen.x + screen.width && w.bounds.x + w.bounds.width > screen.x
            && w.bounds.y < screen.y + screen.height && w.bounds.y + w.bounds.height > screen.y).collect()
    }

    pub fn desktop_level() -> i32 { unsafe { CGWindowLevelForKey(2) } } // kCGDesktopWindowLevelKey

    pub fn desktop_icon_level() -> i32 { unsafe { CGWindowLevelForKey(18) } } // kCGDesktopIconWindowLevelKey

    // Process names are localized ("알림 센터"), so owners are identified by executable path.
    fn owner_of(pid: i32) -> Owner {
        let mut buffer = [0u8; 4096];
        let length = unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
        if length <= 0 { return Owner::Other; }
        let path = String::from_utf8_lossy(&buffer[..length as usize]);
        if path.contains("/WindowManager.app/") { Owner::WindowManager }
        else if path.contains("/WallpaperAgent.app/") { Owner::WallpaperAgent }
        else if path.ends_with("/Finder.app/Contents/MacOS/Finder") { Owner::Finder }
        else if path.contains("/NotificationCenter.app/") { Owner::NotificationCenter }
        else { Owner::Other }
    }

    fn number(dict: *const c_void, key: *const c_void) -> Option<i64> {
        let value = unsafe { CFDictionaryGetValue(dict, key) };
        if value.is_null() { return None; }
        let mut out = 0i64;
        unsafe { CFNumberGetValue(value, 4, (&mut out as *mut i64).cast()) }.then_some(out) // kCFNumberSInt64Type
    }

    fn string(dict: *const c_void, key: *const c_void) -> String {
        let value = unsafe { CFDictionaryGetValue(dict, key) };
        if value.is_null() { return String::new(); }
        let mut buffer = [0u8; 512];
        if !unsafe { CFStringGetCString(value, buffer.as_mut_ptr(), buffer.len() as isize, 0x0800_0100) } { return String::new(); } // UTF-8
        let end = buffer.iter().position(|byte| *byte == 0).unwrap_or(0);
        String::from_utf8_lossy(&buffer[..end]).into_owned()
    }

    /// While a Space switch or app activation animates, the desktop layers are briefly
    /// missing or mid-slide (seen: Finder's icon layer absent, a widget at x=-67 instead
    /// of 368). Re-read until two snapshots agree and the icon layer is present.
    // The preview, the scene and an arrangement often ask within a second of each other;
    // a settled list stays valid that long, so they skip the 300ms settle wait.
    static SETTLED: std::sync::Mutex<Option<(std::time::Instant, Vec<WindowInfo>)>> = std::sync::Mutex::new(None);
    pub fn list_settled() -> Vec<WindowInfo> {
        if let Ok(cache) = SETTLED.lock() {
            if let Some((at, windows)) = cache.as_ref() { if at.elapsed() < std::time::Duration::from_secs(3) { return windows.clone(); } }
        }
        let windows = settle();
        if let Ok(mut cache) = SETTLED.lock() { *cache = Some((std::time::Instant::now(), windows.clone())); }
        windows
    }

    /// Drops the cached list (the desktop just changed, e.g. after arranging).
    pub fn forget_settled() { if let Ok(mut cache) = SETTLED.lock() { *cache = None; } }

    fn settle() -> Vec<WindowInfo> {
        let signature = |windows: &[WindowInfo]| -> Vec<(u32, i64, i64, i64, i64)> {
            let icon_level = desktop_icon_level();
            windows.iter().filter(|w| w.owner == Owner::NotificationCenter || (w.owner == Owner::Finder && w.layer == icon_level))
                .map(|w| (w.id, w.bounds.x.round() as i64, w.bounds.y.round() as i64, w.bounds.width.round() as i64, w.bounds.height.round() as i64)).collect()
        };
        let has_icons = |windows: &[WindowInfo]| { let icon_level = desktop_icon_level(); windows.iter().any(|w| w.owner == Owner::Finder && w.layer == icon_level) };
        let mut current = list();
        for _ in 0..10 {
            std::thread::sleep(std::time::Duration::from_millis(300));
            let next = list();
            let settled = has_icons(&next) && signature(&current) == signature(&next);
            current = next;
            if settled { break; }
        }
        current
    }

    /// Lists every window (on- and off-screen) owned by the desktop-related system processes.
    pub fn list() -> Vec<WindowInfo> {
        let array = unsafe { CGWindowListCopyWindowInfo(0, 0) }; // kCGWindowListOptionAll
        if array.is_null() { return Vec::new(); }
        let mut windows = Vec::new();
        for index in 0..unsafe { CFArrayGetCount(array) } {
            let dict = unsafe { CFArrayGetValueAtIndex(array, index) };
            let (Some(id), Some(layer), Some(pid)) = (unsafe { (number(dict, kCGWindowNumber), number(dict, kCGWindowLayer), number(dict, kCGWindowOwnerPID)) }) else { continue };
            let owner = owner_of(pid as i32);
            if owner == Owner::Other { continue; }
            let mut bounds = CGRect::default();
            let bounds_dict = unsafe { CFDictionaryGetValue(dict, kCGWindowBounds) };
            if bounds_dict.is_null() || !unsafe { CGRectMakeWithDictionaryRepresentation(bounds_dict, &mut bounds) } { continue; }
            let onscreen_value = unsafe { CFDictionaryGetValue(dict, kCGWindowIsOnscreen) };
            let onscreen = !onscreen_value.is_null() && unsafe { CFBooleanGetValue(onscreen_value) };
            windows.push(WindowInfo { id: id as u32, layer: layer as i32, owner, name: unsafe { string(dict, kCGWindowName) }, onscreen, bounds });
        }
        unsafe { CFRelease(array) };
        windows
    }
}

#[cfg(target_os = "macos")]
fn capture_window_bytes(id: u32, max_pixels: u32, jpeg: bool) -> Option<Vec<u8>> {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    let path = std::env::temp_dir().join(format!("tidy-layer-{id}-{stamp}.{}", if jpeg { "jpg" } else { "png" }));
    let result = (|| {
        let capture = std::process::Command::new("/usr/sbin/screencapture").args(["-x", "-o", "-l", &id.to_string()]).arg(&path).output().ok()?;
        if !capture.status.success() { return None; }
        let mut resize = std::process::Command::new("/usr/bin/sips");
        resize.args(["-Z", &max_pixels.to_string()]);
        if jpeg { resize.args(["-s", "format", "jpeg", "-s", "formatOptions", "82"]); }
        let _ = resize.arg(&path).output();
        std::fs::read(&path).ok().filter(|bytes| !bytes.is_empty() && bytes.len() < 12 * 1024 * 1024)
    })();
    let _ = std::fs::remove_file(&path);
    result
}

#[cfg(target_os = "macos")]
fn data_url(bytes: &[u8], jpeg: bool) -> String {
    use base64::Engine;
    format!("data:image/{};base64,{}", if jpeg { "jpeg" } else { "png" }, base64::engine::general_purpose::STANDARD.encode(bytes))
}

#[cfg(target_os = "macos")]
fn capture_window_image(id: u32, max_pixels: u32, jpeg: bool) -> Option<String> {
    capture_window_bytes(id, max_pixels, jpeg).map(|bytes| data_url(&bytes, jpeg))
}

/// Decodes an image with ImageIO and reports whether any pixel is not fully transparent
/// black. A blank capture compresses to about the same size as a sparse one, so size
/// alone cannot tell them apart.
#[cfg(target_os = "macos")]
fn image_has_visible_pixels(bytes: &[u8]) -> bool {
    use std::ffi::c_void;
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFDataCreate(allocator: *const c_void, bytes: *const u8, length: isize) -> *const c_void;
        fn CFDataGetLength(data: *const c_void) -> isize;
        fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
        fn CFRelease(value: *const c_void);
    }
    #[link(name = "ImageIO", kind = "framework")]
    extern "C" {
        fn CGImageSourceCreateWithData(data: *const c_void, options: *const c_void) -> *const c_void;
        fn CGImageSourceCreateImageAtIndex(source: *const c_void, index: usize, options: *const c_void) -> *const c_void;
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGImageGetDataProvider(image: *const c_void) -> *const c_void;
        fn CGDataProviderCopyData(provider: *const c_void) -> *const c_void;
        fn CGImageRelease(image: *const c_void);
    }
    unsafe {
        let data = CFDataCreate(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize);
        if data.is_null() { return true; }
        let source = CGImageSourceCreateWithData(data, std::ptr::null());
        let image = if source.is_null() { std::ptr::null() } else { CGImageSourceCreateImageAtIndex(source, 0, std::ptr::null()) };
        let pixels = if image.is_null() { std::ptr::null() } else { CGDataProviderCopyData(CGImageGetDataProvider(image)) };
        // Undecodable input counts as visible so a decoding hiccup never hides real content.
        let visible = if pixels.is_null() { true } else {
            std::slice::from_raw_parts(CFDataGetBytePtr(pixels), CFDataGetLength(pixels) as usize).iter().any(|byte| *byte != 0)
        };
        if !pixels.is_null() { CFRelease(pixels); }
        if !image.is_null() { CGImageRelease(image); }
        if !source.is_null() { CFRelease(source); }
        CFRelease(data);
        visible
    }
}

// Right after a Space switch Finder's icon layer can still be blank. If the desktop has
// items, retry a few times until the capture actually shows something.
#[cfg(target_os = "macos")]
fn capture_icon_layer(app: &tauri::AppHandle, id: u32) -> Option<String> {
    let desktop_has_items = app.path().desktop_dir().ok().and_then(|dir| std::fs::read_dir(dir).ok())
        .is_some_and(|mut entries| entries.any(|entry| entry.is_ok_and(|entry| !entry.file_name().to_string_lossy().starts_with('.'))));
    for attempt in 0..3 {
        if attempt > 0 { std::thread::sleep(std::time::Duration::from_millis(400)); }
        if let Some(bytes) = capture_window_bytes(id, 1800, false) {
            if !desktop_has_items || image_has_visible_pixels(&bytes) { return Some(data_url(&bytes, false)); }
        }
    }
    None
}

/// Rebuilds the desktop as separate layers (wallpaper, Finder icons, widgets) so the
/// preview shows the real desktop without other apps' windows and without hiding the app window.
#[tauri::command(async)]
fn desktop_scene(app: tauri::AppHandle, icons_only: Option<bool>) -> Result<DesktopScene, String> {
    #[cfg(target_os = "macos")]
    {
        use desktop_windows::Owner;
        let window = app.get_webview_window("main").ok_or("tidy 창을 찾을 수 없습니다.")?;
        let monitor = window.primary_monitor().map_err(|_| "주 화면 정보를 읽지 못했습니다.".to_string())?.ok_or("주 화면을 찾을 수 없습니다.")?;
        let scale = monitor.scale_factor().max(1.0);
        let display = SceneRect { x: monitor.position().x as f64 / scale, y: monitor.position().y as f64 / scale, width: monitor.size().width as f64 / scale, height: monitor.size().height as f64 / scale };
        let area = primary_display_area(&app)?;
        let work = SceneRect { x: area.left, y: area.top, width: area.width, height: area.height };
        let covers_main = |bounds: &desktop_windows::CGRect| (bounds.x - display.x).abs() < 1.0 && (bounds.y - display.y).abs() < 1.0
            && (bounds.width - display.width).abs() < 1.0 && (bounds.height - display.height).abs() < 1.0;
        let icon_level = desktop_windows::desktop_icon_level();
        let windows = desktop_windows::list_settled();

        // Each Space has its own wallpaper window; prefer the one currently on screen.
        // The wallpaper sits at the desktop level; WindowManager's "Fullscreen Backdrop" sits just above it.
        let desktop_level = desktop_windows::desktop_level();
        let mut wallpapers: Vec<_> = windows.iter().filter(|w| matches!(w.owner, Owner::WindowManager | Owner::WallpaperAgent) && w.layer <= desktop_level && covers_main(&w.bounds)).collect();
        wallpapers.sort_by_key(|w| (!w.onscreen, w.owner == Owner::WallpaperAgent, std::cmp::Reverse(w.id)));
        let icons = windows.iter().find(|w| w.owner == Owner::Finder && w.layer == icon_level && covers_main(&w.bounds));
        let widget_windows = desktop_windows::widgets(&windows, &display);

        let permission = objc2_core_graphics::CGPreflightScreenCaptureAccess();
        // After an arrangement only the icon layer changed; the caller keeps its wallpaper and widgets.
        let icons_only = icons_only.unwrap_or(false);
        // Each layer is its own screencapture + sips process, so run them side by side.
        let (wallpaper, icons_image, widget_images) = std::thread::scope(|scope| {
            let wallpaper = (permission && !icons_only).then(|| scope.spawn(|| wallpapers.iter().find_map(|w| capture_window_image(w.id, 1800, true))));
            let icons_image = permission.then(|| scope.spawn(|| icons.and_then(|w| capture_icon_layer(&app, w.id))));
            let widget_images: Vec<_> = widget_windows.iter().map(|w| (permission && !icons_only).then(|| { let id = w.id; scope.spawn(move || capture_window_image(id, 640, false)) })).collect();
            (
                wallpaper.and_then(|handle| handle.join().ok().flatten()),
                icons_image.and_then(|handle| handle.join().ok().flatten()),
                widget_images.into_iter().map(|handle| handle.and_then(|handle| handle.join().ok().flatten())).collect::<Vec<_>>(),
            )
        });
        let widgets = widget_windows.iter().zip(widget_images).map(|(w, image)| SceneWidget {
            name: if w.name.is_empty() { "위젯".into() } else { w.name.clone() },
            bounds: SceneRect { x: w.bounds.x, y: w.bounds.y, width: w.bounds.width, height: w.bounds.height },
            image,
        }).collect();
        Ok(DesktopScene { display, work, wallpaper, icons: icons_image, widgets, permission })
    }
    #[cfg(windows)]
    {
        let window = app.get_webview_window("main").ok_or("tidy 창을 찾을 수 없습니다.")?;
        let monitor = window.primary_monitor().map_err(|_| "주 화면 정보를 읽지 못했습니다.".to_string())?.ok_or("주 화면을 찾을 수 없습니다.")?;
        let scale = monitor.scale_factor().max(1.0);
        let display = SceneRect { x: monitor.position().x as f64 / scale, y: monitor.position().y as f64 / scale, width: monitor.size().width as f64 / scale, height: monitor.size().height as f64 / scale };
        let area = primary_display_area(&app)?;
        let work = SceneRect { x: area.left, y: area.top, width: area.width, height: area.height };
        // Windows has no capturable icon layer or desktop widgets; the preview shows the wallpaper and planned spots.
        let wallpaper = if icons_only.unwrap_or(false) { None } else { windows_desktop::wallpaper() };
        Ok(DesktopScene { display, work, wallpaper, icons: None, widgets: Vec::new(), permission: true })
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    { let _ = (app, icons_only); Err("실제 바탕화면 미리보기는 macOS와 Windows에서만 지원합니다.".into()) }
}

fn primary_display_area(app: &tauri::AppHandle) -> Result<DisplayArea, String> {
    let window = app.get_webview_window("main").ok_or("화면 크기를 확인할 수 없습니다.")?;
    let monitor = window.primary_monitor().map_err(|_| "주 화면 크기를 확인할 수 없습니다.".to_string())?.ok_or("주 화면 크기를 확인할 수 없습니다.")?;
    let work = monitor.work_area();
    let scale = monitor.scale_factor().max(1.0);
    Ok(DisplayArea {
        left: work.position.x as f64 / scale,
        top: work.position.y as f64 / scale,
        width: work.size.width as f64 / scale,
        height: work.size.height as f64 / scale,
    })
}

fn valid_region(region: &ScreenRegion) -> bool {
    region.x.is_finite() && region.y.is_finite() && region.width.is_finite() && region.height.is_finite()
        && region.x >= 0.0 && region.y >= 0.0 && region.width >= 0.12 && region.height >= 0.12
        // Small tolerance: a range dragged flush to the edge can sum to 1.0000000000000002.
        && region.x + region.width <= 1.0 + 1e-9 && region.y + region.height <= 1.0 + 1e-9
}

fn regions_overlap(a: &ScreenRegion, b: &ScreenRegion) -> bool {
    a.x < b.x + b.width && a.x + a.width > b.x && a.y < b.y + b.height && a.y + a.height > b.y
}

// An icon plus its two-line label, measured from the point Finder positions it at,
// with a small margin so nothing touches a widget's edge.
fn icon_hits_obstacle(x: f64, y: f64, obstacles: &[SceneRect]) -> bool {
    obstacles.iter().any(|o| x - 46.0 < o.x + o.width + 8.0 && x + 46.0 > o.x - 8.0 && y - 40.0 < o.y + o.height + 8.0 && y + 52.0 > o.y - 8.0)
}

/// Fills rows top to bottom; each group starts on a fresh row and slots under a widget stay empty.
/// A partly filled row uses the free slots on the aligned side. None when `rows` are not enough.
fn fill_rows(groups: &[Vec<DesktopEntry>], slots: &[f64], rows: usize, row_y: impl Fn(usize) -> f64, obstacles: &[SceneRect], align: IconAlign) -> Option<Vec<(PathBuf, i32, i32)>> {
    let (mut placed, mut row) = (Vec::new(), 0usize);
    for group in groups.iter().filter(|group| !group.is_empty()) {
        let mut remaining = &group[..];
        while !remaining.is_empty() {
            if row >= rows { return None; }
            let y = row_y(row);
            row += 1;
            let free: Vec<f64> = slots.iter().copied().filter(|x| !icon_hits_obstacle(*x, y, obstacles)).collect();
            let take = remaining.len().min(free.len());
            let start = match align { IconAlign::Left => 0, IconAlign::Center => (free.len() - take) / 2, IconAlign::Right => free.len() - take };
            let mut xs = free[start..start + take].to_vec();
            // Centred rows on a fixed slot grid land half a slot off when the leftover count
            // is odd; nudge them onto the true centre when no widget is in this row.
            if align == IconAlign::Center && free.len() == slots.len() && slots.len() > 1 && (free.len() - take) % 2 == 1 {
                let half = (slots[1] - slots[0]) / 2.0;
                xs.iter_mut().for_each(|x| *x += half);
            }
            for (item, x) in remaining[..take].iter().zip(&xs) { placed.push((item.path.clone(), x.round() as i32, y.round() as i32)); }
            remaining = &remaining[take..];
        }
    }
    Some(placed)
}

fn append_region_positions(groups: &[Vec<DesktopEntry>], label: &str, region: &ScreenRegion, display: DisplayArea, obstacles: &[SceneRect], spacing: Option<IconSpacing>, align: IconAlign, positions: &mut Vec<(PathBuf, i32, i32)>) -> Result<(), String> {
    let area = DisplayArea {
        left: display.left + region.x * display.width,
        top: display.top + region.y * display.height,
        width: region.width * display.width,
        height: region.height * display.height,
    };
    let item_count: usize = groups.iter().map(Vec::len).sum();
    if item_count == 0 { return Ok(()); }
    let inner_width = area.width - 48.0;
    let inner_height = area.height - 64.0;
    if inner_width < 88.0 || inner_height < 72.0 { return Err("정리 범위가 너무 좁습니다. 영역을 넓혀 주세요.".into()); }
    // Finder snaps icon positions to its desktop grid. Pack using a conservative
    // cell size, then choose enough columns to fit all grouped rows vertically.
    let max_columns = ((inner_width / 92.0).floor() as usize).max(1);
    let max_rows = ((inner_height / 88.0).floor() as usize).max(1);
    let shortage = |blocked: f64| {
        let (needed_width, needed_height) = (1..=item_count).map(|columns| {
            let rows = groups.iter().map(|group| if group.is_empty() { 0 } else { (group.len() + columns - 1) / columns }).sum::<usize>();
            ((columns as f64 * 92.0 + 48.0), (rows as f64 * 88.0 + 64.0))
        }).min_by(|a, b| (a.0 * a.1).total_cmp(&(b.0 * b.1))).unwrap_or((0.0, 0.0));
        // Grow the suggestion by the area widgets take away so auto-expansion has a chance to fit.
        let grow = if blocked > 0.0 { (1.0 + blocked / (needed_width * needed_height).max(1.0)).sqrt() } else { 1.0 };
        let note = if blocked > 0.0 { " 위젯이 있는 자리는 비워 둡니다." } else { "" };
        format!("{} 범위에 항목 {}개를 넣을 공간이 부족합니다. 현재 {:.0}×{:.0}pt, 권장 최소 {:.0}×{:.0}pt입니다. 미리보기에서 {} 범위를 넓혀 주세요.{}", label, item_count, area.width, area.height, needed_width * grow, needed_height * grow, label, note)
    };
    let obstacles: Vec<SceneRect> = obstacles.iter().copied()
        .filter(|o| o.x < area.left + area.width && o.x + o.width > area.left && o.y < area.top + area.height && o.y + o.height > area.top).collect();
    let blocked_area = || -> f64 { obstacles.iter().map(|o| {
        let width = (o.x + o.width).min(area.left + area.width) - o.x.max(area.left);
        let height = (o.y + o.height).min(area.top + area.height) - o.y.max(area.top);
        (width + 100.0).max(0.0) * (height + 108.0).max(0.0)
    }).sum() };
    if let Some(spacing) = spacing {
        // Fixed spacing chosen by the user: a grid filled from the top instead of stretching
        // the icons over the whole region. Icon centres stay 46pt (half an icon with its
        // label) inside the region's edges so nothing pokes out on the aligned side.
        let columns = ((area.width - 92.0).max(0.0) / spacing.x).floor() as usize + 1;
        let rows = (inner_height / spacing.y).floor() as usize + 1;
        let span = spacing.x * (columns - 1) as f64;
        let first = match align {
            IconAlign::Left => area.left + 46.0,
            IconAlign::Center => area.left + area.width / 2.0 - span / 2.0,
            IconAlign::Right => area.left + area.width - 46.0 - span,
        };
        let slots: Vec<f64> = (0..columns).map(|index| first + index as f64 * spacing.x).collect();
        if let Some(placed) = fill_rows(groups, &slots, rows, |row| area.top + 32.0 + row as f64 * spacing.y, &obstacles, align) { positions.extend(placed); return Ok(()); }
        let needed_rows: usize = groups.iter().map(|group| if group.is_empty() { 0 } else { (group.len() + columns - 1) / columns }).sum();
        let blocked = blocked_area();
        let grow = if blocked > 0.0 { 1.0 + blocked / (area.width * area.height).max(1.0) } else { 1.0 };
        let note = if blocked > 0.0 { " 위젯이 있는 자리는 비워 둡니다." } else { "" };
        return Err(format!("{} 범위에 항목 {}개를 넣을 공간이 부족합니다. 현재 {:.0}×{:.0}pt, 권장 최소 {:.0}×{:.0}pt입니다. 미리보기에서 {} 범위를 넓히거나 아이콘 간격을 줄여 주세요.{}", label, item_count, area.width, area.height, area.width, ((needed_rows.max(1) - 1) as f64 * spacing.y + 64.0) * grow, label, note));
    }
    if !obstacles.is_empty() {
        // Same evenly spread rows and column slots as below, but slots under a widget stay
        // empty. Try the fewest rows first so the layout stays as airy as possible.
        let column_gap = inner_width / max_columns as f64;
        let slots: Vec<f64> = (0..max_columns).map(|index| area.left + 24.0 + (index as f64 + 0.5) * column_gap).collect();
        for rows in 1..=max_rows {
            let row_y = |row: usize| if rows == 1 { area.top + area.height / 2.0 } else { area.top + 32.0 + inner_height / (rows - 1) as f64 * row as f64 };
            if let Some(placed) = fill_rows(groups, &slots, rows, row_y, &obstacles, align) { positions.extend(placed); return Ok(()); }
        }
        let blocked = blocked_area();
        return Err(shortage(blocked));
    }
    let columns = (1..=max_columns).rev().find(|columns| {
        groups.iter().map(|group| if group.is_empty() { 0 } else { (group.len() + columns - 1) / columns }).sum::<usize>() <= max_rows
    }).ok_or_else(|| shortage(0.0))?;
    let row_count: usize = groups.iter().map(|group| if group.is_empty() { 0 } else { (group.len() + columns - 1) / columns }).sum();
    if row_count == 0 { return Ok(()); }
    let row_gap = if row_count == 1 { 0.0 } else { inner_height / (row_count - 1) as f64 };
    let mut row_index = 0usize;
    for group in groups {
        for row in group.chunks(columns) {
            let y = if row_count == 1 { (area.top + area.height / 2.0).round() as i32 }
                else { (area.top + 32.0 + row_gap * row_index as f64).round() as i32 };
            row_index += 1;
            let column_gap = inner_width / max_columns as f64;
            let row_width = column_gap * row.len().saturating_sub(1) as f64;
            let first_x = match align {
                IconAlign::Left => area.left + 24.0 + column_gap / 2.0,
                IconAlign::Center => area.left + (area.width - row_width) / 2.0,
                IconAlign::Right => area.left + area.width - 24.0 - column_gap / 2.0 - row_width,
            };
            for (index, item) in row.iter().enumerate() {
                let x = first_x + index as f64 * column_gap;
                positions.push((item.path.clone(), x.round() as i32, y));
            }
        }
    }
    Ok(())
}

// Finder stores tags as a binary plist of "name\ncolor-index" strings. Reading the
// color index (instead of matching names) works in every system language and
// ignores custom tags such as "Blueprint".
const NO_COLOR_RANK: usize = 7;

fn tag_color_rank(tag: &str) -> usize {
    let Some((_, index)) = tag.rsplit_once('\n') else { return NO_COLOR_RANK; };
    // Finder color index → sort rank (red, orange, yellow, green, blue, purple, gray).
    match index.trim() { "6" => 0, "7" => 1, "5" => 2, "2" => 3, "4" => 4, "3" => 5, "1" => 6, _ => NO_COLOR_RANK }
}

fn parse_tag_plist(data: &[u8]) -> Option<Vec<String>> {
    if data.len() < 40 || !data.starts_with(b"bplist00") { return None; }
    let trailer = &data[data.len() - 32..];
    let read_uint = |bytes: &[u8]| bytes.iter().fold(0usize, |value, byte| (value << 8) | *byte as usize);
    let (offset_size, ref_size) = (trailer[6] as usize, trailer[7] as usize);
    if !(1..=8).contains(&offset_size) || !(1..=8).contains(&ref_size) { return None; }
    let object_count = read_uint(&trailer[8..16]);
    let (top, table) = (read_uint(&trailer[16..24]), read_uint(&trailer[24..32]));
    let object_offset = |index: usize| -> Option<usize> {
        if index >= object_count { return None; }
        let start = table.checked_add(index.checked_mul(offset_size)?)?;
        Some(read_uint(data.get(start..start + offset_size)?))
    };
    let header = |offset: usize| -> Option<(u8, usize, usize)> {
        let marker = *data.get(offset)?;
        let (kind, small) = (marker >> 4, (marker & 0x0f) as usize);
        if small != 0x0f { return Some((kind, small, offset + 1)); }
        let int_marker = *data.get(offset + 1)?;
        if int_marker >> 4 != 0x1 { return None; }
        let width = 1usize << (int_marker & 0x0f);
        Some((kind, read_uint(data.get(offset + 2..offset + 2 + width)?), offset + 2 + width))
    };
    let (kind, count, refs) = header(object_offset(top)?)?;
    if kind != 0xA { return None; }
    let mut tags = Vec::with_capacity(count);
    for index in 0..count {
        let start = refs + index * ref_size;
        let (kind, length, body) = header(object_offset(read_uint(data.get(start..start + ref_size)?))?)?;
        match kind {
            0x5 => tags.push(String::from_utf8_lossy(data.get(body..body + length)?).into_owned()),
            0x6 => {
                let units: Vec<u16> = data.get(body..body + length * 2)?.chunks_exact(2).map(|pair| u16::from_be_bytes([pair[0], pair[1]])).collect();
                tags.push(String::from_utf16_lossy(&units));
            }
            _ => {}
        }
    }
    Some(tags)
}

#[cfg(target_os = "macos")]
fn finder_color_rank(path: &Path) -> usize {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let (Ok(path_c), Ok(name)) = (CString::new(path.as_os_str().as_bytes()), CString::new("com.apple.metadata:_kMDItemUserTags")) else { return NO_COLOR_RANK; };
    let mut buffer = vec![0u8; 64 * 1024];
    let read = unsafe { libc::getxattr(path_c.as_ptr(), name.as_ptr(), buffer.as_mut_ptr().cast(), buffer.len(), 0, libc::XATTR_NOFOLLOW) };
    if read <= 0 { return NO_COLOR_RANK; }
    parse_tag_plist(&buffer[..read as usize])
        .and_then(|tags| tags.iter().map(|tag| tag_color_rank(tag)).min())
        .unwrap_or(NO_COLOR_RANK)
}

#[cfg(target_os = "macos")]
fn resolve_finder_alias(path: &Path) -> Option<PathBuf> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    #[repr(C)]
    struct FsRef { hidden: [u8; 80] }
    #[link(name = "Carbon", kind = "framework")]
    extern "C" {
        fn FSPathMakeRef(path: *const u8, reference: *mut FsRef, is_directory: *mut u8) -> i32;
        fn FSResolveAliasFile(reference: *mut FsRef, resolve_chains: u8, target_is_folder: *mut u8, was_aliased: *mut u8) -> i32;
        fn FSRefMakePath(reference: *const FsRef, path: *mut u8, max_path_size: u32) -> i32;
    }
    let path_c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut reference = FsRef { hidden: [0; 80] };
    let mut is_directory = 0u8;
    let mut target_is_folder = 0u8;
    let mut was_aliased = 0u8;
    unsafe {
        if FSPathMakeRef(path_c.as_ptr().cast(), &mut reference, &mut is_directory) != 0 { return None; }
        if FSResolveAliasFile(&mut reference, 0, &mut target_is_folder, &mut was_aliased) != 0 || was_aliased == 0 { return None; }
        let mut buffer = [0u8; 4096];
        if FSRefMakePath(&reference, buffer.as_mut_ptr(), buffer.len() as u32) != 0 { return None; }
        let end = buffer.iter().position(|byte| *byte == 0)?;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buffer[..end])))
    }
}

fn applescript_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n").replace('\r', "\\r"))
}

#[tauri::command]
fn desktop_screen_info(app: tauri::AppHandle) -> Result<DesktopScreenInfo, String> {
    #[cfg(any(target_os = "macos", windows))]
    { let display = primary_display_area(&app)?; Ok(DesktopScreenInfo { width: display.width, height: display.height }) }
    #[cfg(not(any(target_os = "macos", windows)))]
    { let _ = app; Err("바탕화면 범위 설정은 macOS와 Windows에서만 지원합니다.".into()) }
}

#[derive(Clone, Copy, Deserialize, Debug)] struct IconSpacing { x: f64, y: f64 }
/// Where icons sit horizontally inside a region (whole layout block and partly filled rows).
#[derive(Clone, Copy, Default, Deserialize, PartialEq, Debug)] #[serde(rename_all = "lowercase")] enum IconAlign { Left, #[default] Center, Right }
/// Per-region arrangement settings: the app region and the folder/file region each have their own.
#[derive(Clone, Copy, Default, Deserialize, Debug)] struct RegionStyle { #[serde(default)] spacing: Option<IconSpacing>, #[serde(default)] align: IconAlign }
fn valid_spacing(spacing: &IconSpacing) -> bool { spacing.x.is_finite() && spacing.y.is_finite() && (76.0..=400.0).contains(&spacing.x) && (76.0..=400.0).contains(&spacing.y) }

#[cfg(any(target_os = "macos", windows))]
struct DesktopPlan { app_count: usize, folder_count: usize, file_count: usize, positions: Vec<(PathBuf, i32, i32)>, kinds: HashMap<PathBuf, &'static str>, display: DisplayArea }

#[cfg(target_os = "macos")]
fn read_desktop_items(app: &tauri::AppHandle) -> Result<(Vec<DesktopEntry>, Vec<DesktopEntry>, Vec<DesktopEntry>), String> {
    let desktop = app.path().desktop_dir().map_err(|_| "바탕화면 폴더를 찾을 수 없습니다.".to_string())?;
    let mut apps = Vec::new();
    let mut folders = Vec::new();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&desktop).map_err(|_| "바탕화면 항목을 읽을 수 없습니다.".to_string())? {
        let entry = entry.map_err(|_| "바탕화면 항목을 읽을 수 없습니다.".to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name.starts_with("._") { continue; }
        let path = entry.path();
        // Items with the Finder hidden flag (e.g. "Icon\r") are not shown on the desktop.
        {
            use std::os::macos::fs::MetadataExt;
            if std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.st_flags() & libc::UF_HIDDEN != 0) { continue; }
        }
        let file_type = entry.file_type().map_err(|_| format!("{} 항목 정보를 읽을 수 없습니다.", name))?;
        let color = finder_color_rank(&path);
        let item = DesktopEntry { path, name, color };
        let resolved = resolve_finder_alias(&item.path).unwrap_or_else(|| item.path.clone());
        let resolved_metadata = std::fs::metadata(&resolved).ok();
        let app_bundle = resolved.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("app"));
        let is_folder = resolved_metadata.as_ref().is_some_and(|metadata| metadata.is_dir()) || file_type.is_dir();
        if app_bundle { apps.push(item); }
        else if is_folder { folders.push(item); }
        else { files.push(item); }
    }
    Ok((apps, folders, files))
}

#[cfg(windows)]
fn read_desktop_items(_app: &tauri::AppHandle) -> Result<(Vec<DesktopEntry>, Vec<DesktopEntry>, Vec<DesktopEntry>), String> {
    let (mut apps, mut folders, mut files) = (Vec::new(), Vec::new(), Vec::new());
    for item in windows_desktop::items()? {
        let entry = DesktopEntry { path: item.path, name: item.name, color: NO_COLOR_RANK };
        match item.kind {
            windows_desktop::ItemKind::App => apps.push(entry),
            windows_desktop::ItemKind::Folder => folders.push(entry),
            windows_desktop::ItemKind::File => files.push(entry),
        }
    }
    Ok((apps, folders, files))
}

/// Reads the desktop and computes where every item goes, without moving anything.
#[cfg(any(target_os = "macos", windows))]
fn plan_desktop_layout(app: &tauri::AppHandle, icon_area: &ScreenRegion, file_area: &ScreenRegion, icon_style: RegionStyle, file_style: RegionStyle) -> Result<DesktopPlan, String> {
    if !valid_region(&icon_area) || !valid_region(&file_area) { return Err("정리 범위가 화면 바깥에 있거나 너무 작습니다.".into()); }
    if regions_overlap(&icon_area, &file_area) { return Err("앱 아이콘 범위와 폴더·파일 범위가 겹칩니다. 서로 떨어지도록 조정해 주세요.".into()); }
    let display = primary_display_area(app)?;

    if [icon_style.spacing, file_style.spacing].iter().flatten().any(|spacing| !valid_spacing(spacing)) { return Err("아이콘 간격 값이 올바르지 않습니다.".into()); }
    let (mut apps, mut folders, mut files) = read_desktop_items(app)?;
    for group in [&mut apps, &mut folders, &mut files] {
        group.sort_by(|a,b| a.color.cmp(&b.color).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    }
    let kinds: HashMap<PathBuf, &'static str> = apps.iter().map(|item| (item.path.clone(), "app"))
        .chain(folders.iter().map(|item| (item.path.clone(), "folder")))
        .chain(files.iter().map(|item| (item.path.clone(), "file"))).collect();
    let app_count = apps.len();
    let folder_count = folders.len();
    let file_count = files.len();
    // Widgets can't be moved, so their spots inside a region are left empty.
    #[cfg(target_os = "macos")]
    let widgets: Vec<SceneRect> = {
        let work = SceneRect { x: display.left, y: display.top, width: display.width, height: display.height };
        let windows = desktop_windows::list_settled();
        desktop_windows::widgets(&windows, &work).iter().map(|w| SceneRect { x: w.bounds.x, y: w.bounds.y, width: w.bounds.width, height: w.bounds.height }).collect()
    };
    #[cfg(windows)]
    let widgets: Vec<SceneRect> = Vec::new();
    let mut positions = Vec::new();
    // Apps (and aliases to apps) gather in the app region; everything else — folders
    // first, then files on their own rows — gathers in the other region.
    append_region_positions(&[apps], "앱 아이콘", icon_area, display, &widgets, icon_style.spacing, icon_style.align, &mut positions)?;
    append_region_positions(&[folders, files], "폴더·파일", file_area, display, &widgets, file_style.spacing, file_style.align, &mut positions)?;
    Ok(DesktopPlan { app_count, folder_count, file_count, positions, kinds, display })
}

#[derive(Serialize)] struct PlannedIcon { kind: &'static str, x: f64, y: f64 }

/// Where each desktop item would land, as fractions of the work area, for the preview.
#[tauri::command(async)]
fn preview_desktop_arrangement(app: tauri::AppHandle, icon_area: ScreenRegion, file_area: ScreenRegion, icon_style: Option<RegionStyle>, file_style: Option<RegionStyle>) -> Result<Vec<PlannedIcon>, String> {
    #[cfg(any(target_os = "macos", windows))]
    {
        let plan = plan_desktop_layout(&app, &icon_area, &file_area, icon_style.unwrap_or_default(), file_style.unwrap_or_default())?;
        let display = plan.display;
        Ok(plan.positions.iter().map(|(path, x, y)| PlannedIcon {
            kind: plan.kinds.get(path).copied().unwrap_or("file"),
            x: (*x as f64 - display.left) / display.width,
            y: (*y as f64 - display.top) / display.height,
        }).collect())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    { let _ = (app, icon_area, file_area, icon_style, file_style); Err("바탕화면 범위별 정리는 macOS와 Windows에서만 지원합니다.".into()) }
}

#[tauri::command(async)]
fn arrange_desktop_in_regions(app: tauri::AppHandle, icon_area: ScreenRegion, file_area: ScreenRegion, icon_style: Option<RegionStyle>, file_style: Option<RegionStyle>) -> Result<DesktopArrangeSummary, String> {
    #[cfg(target_os = "macos")]
    {
        let DesktopPlan { app_count, folder_count, file_count, positions, .. } = plan_desktop_layout(&app, &icon_area, &file_area, icon_style.unwrap_or_default(), file_style.unwrap_or_default())?;
        if positions.is_empty() { return Ok(DesktopArrangeSummary { apps: 0, folders: 0, files: 0, total: 0, failed: Vec::new() }); }
        // Each item is placed inside its own try block so one stubborn item does not
        // leave the desktop half arranged; failures are reported back by name.
        let mut script = String::from("set failedItems to {}\ntell application \"Finder\"\nset arrangement of icon view options of window of desktop to not arranged\n");
        for (path, x, y) in &positions {
            let posix = path.to_string_lossy();
            let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| posix.to_string());
            script.push_str(&format!("try\nset targetItem to (POSIX file {} as alias)\nset desktop position of targetItem to {{{},{}}}\nset checkedPosition to desktop position of targetItem\nif item 1 of checkedPosition < {} or item 1 of checkedPosition > {} or item 2 of checkedPosition < {} or item 2 of checkedPosition > {} then error \"position not applied\"\non error errorMessage number errorNumber\nif errorNumber is -1743 then error errorMessage number errorNumber\nset end of failedItems to {}\nend try\n", applescript_string(&posix), x, y, x - 72, x + 72, y - 72, y + 72, applescript_string(&name)));
        }
        script.push_str("end tell\nset AppleScript's text item delimiters to linefeed\nreturn failedItems as text");
        let output = std::process::Command::new("/usr/bin/osascript").args(["-e", &script]).output()
            .map_err(|_| "Finder에 아이콘 배치 요청을 보내지 못했습니다.".to_string())?;
        desktop_windows::forget_settled();
        if output.status.success() {
            let failed: Vec<String> = String::from_utf8_lossy(&output.stdout).lines().map(str::trim).filter(|line| !line.is_empty()).map(str::to_string).collect();
            Ok(DesktopArrangeSummary { apps: app_count, folders: folder_count, files: file_count, total: positions.len() - failed.len().min(positions.len()), failed })
        }
        else {
            let details = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(if details.is_empty() || details.contains("-1743") { "Finder가 아이콘 배치를 허용하지 않았습니다. 시스템 설정 > 개인정보 보호 및 보안 > 자동화에서 Finder 제어를 허용해 주세요.".into() }
                else { format!("Finder가 아이콘 배치를 완료하지 못했습니다: {details}") })
        }
    }
    #[cfg(windows)]
    {
        let DesktopPlan { app_count, folder_count, file_count, positions, .. } = plan_desktop_layout(&app, &icon_area, &file_area, icon_style.unwrap_or_default(), file_style.unwrap_or_default())?;
        if positions.is_empty() { return Ok(DesktopArrangeSummary { apps: 0, folders: 0, files: 0, total: 0, failed: Vec::new() }); }
        let window = app.get_webview_window("main").ok_or("tidy 창을 찾을 수 없습니다.")?;
        let scale = window.primary_monitor().map_err(|_| "주 화면 정보를 읽지 못했습니다.".to_string())?.ok_or("주 화면을 찾을 수 없습니다.")?.scale_factor().max(1.0);
        let total = positions.len();
        let failed = windows_desktop::place(positions, scale)?;
        Ok(DesktopArrangeSummary { apps: app_count, folders: folder_count, files: file_count, total: total - failed.len().min(total), failed })
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    { let _ = (app, icon_area, file_area, icon_style, file_style); Err("바탕화면 범위별 정리는 macOS와 Windows에서만 지원합니다.".into()) }
}

// The blocking picker must not run on the main thread: macOS shows it as a sheet whose
// completion is delivered on the main run loop, which a blocked main thread cannot service.
#[tauri::command(async)]
fn pick_files_for_collection(app: tauri::AppHandle, state: State<'_, ScanState>) -> Result<Option<Vec<String>>, String> {
    let Some(files) = app.dialog().file().blocking_pick_files() else { return Ok(None); };
    let mut selected = Vec::new();
    for file in files {
        let chosen = file.into_path().map_err(|_| "선택한 파일 경로를 읽을 수 없습니다.".to_string())?;
        let chosen_meta = std::fs::symlink_metadata(&chosen).map_err(|_| "선택한 파일을 읽을 수 없습니다.".to_string())?;
        if chosen_meta.file_type().is_symlink() || !chosen_meta.is_file() { return Err("일반 파일만 선택할 수 있습니다.".into()); }
        let path = chosen.canonical().map_err(|_| "선택한 파일을 찾을 수 없습니다.".to_string())?;
        let text=path.to_string_lossy().into_owned(); if !selected.contains(&text) { selected.push(text); }
    }
    *state.selected_files.lock().map_err(|_| "선택 파일 상태를 저장할 수 없습니다.".to_string())? = selected.clone();
    Ok(Some(selected))
}

// The blocking picker must not run on the main thread (see pick_files_for_collection).
#[tauri::command(async)]
fn pick_move_destination(app: tauri::AppHandle, state: State<'_, ScanState>) -> Result<Option<String>, String> {
    let Some(chosen) = app.dialog().file().blocking_pick_folder() else { return Ok(None); };
    let folder = chosen.into_path().map_err(|_| "선택한 폴더 경로를 확인할 수 없습니다.".to_string())?.canonical().map_err(|_| "선택한 폴더를 열 수 없습니다.".to_string())?;
    if !folder.is_dir() { return Err("폴더를 선택해 주세요.".into()); }
    *state.move_destination.lock().map_err(|_| "폴더 선택 상태를 저장할 수 없습니다.".to_string())? = Some(folder.clone());
    Ok(Some(folder.to_string_lossy().into_owned()))
}

/// Moves files picked in the native file dialog into a new desktop folder or into a folder
/// picked in the native folder dialog. Never overwrites; every move is recorded as a job so
/// "작업 이력" can put the files back.
#[tauri::command(async)]
fn move_selected_files(destination: MoveDestination, paths: Vec<String>, names: Option<Vec<String>>, state: State<'_, ScanState>, app: tauri::AppHandle) -> Result<CollectionResult, String> {
    let _guard = state.operations.lock().map_err(|_| "다른 파일 작업이 진행 중입니다.".to_string())?;
    let authorized = state.selected_files.lock().map_err(|_| "선택 파일 상태를 확인할 수 없습니다.".to_string())?.clone();
    if authorized.is_empty() || authorized != paths { return Err("파일 선택 창에서 파일을 다시 선택해 주세요.".into()); }
    // Names chosen by the user for the moved files; checked before anything changes.
    if let Some(names) = &names {
        if names.len() != paths.len() { return Err("파일 이름 목록이 선택한 파일 수와 맞지 않습니다.".into()); }
        if let Some(bad) = names.iter().find(|name| !valid_folder_name(name.trim())) { return Err(format!("파일 이름 \"{}\"은(는) 쓸 수 없습니다. 점(.)으로 시작하거나 끝날 수 없고, / \\ : < > \" | ? * 기호와 CON·NUL 같은 예약된 이름은 쓸 수 없습니다.", bad.trim())); }
    }
    let (folder, created) = match destination {
        MoveDestination::New { name } => {
            let name = name.trim().to_string();
            if !valid_folder_name(&name) { return Err("새 폴더 이름을 확인해 주세요. 점(.)으로 시작하거나 끝날 수 없고, / \\ : < > \" | ? * 기호와 CON·NUL 같은 예약된 이름은 쓸 수 없습니다.".into()); }
            let desktop = app.path().desktop_dir().map_err(|_| "바탕화면 폴더를 찾을 수 없습니다.".to_string())?.canonical().map_err(|_| "바탕화면 폴더에 접근할 수 없습니다.".to_string())?;
            let folder = desktop.join(&name);
            std::fs::create_dir(&folder).map_err(|_| "같은 이름의 폴더가 있거나 새 폴더를 만들 수 없습니다. 이름을 바꿔 주세요.".to_string())?;
            (folder, true)
        }
        MoveDestination::Existing { path } => {
            let chosen = state.move_destination.lock().map_err(|_| "폴더 선택 상태를 확인할 수 없습니다.".to_string())?.clone().ok_or("옮길 폴더를 먼저 선택해 주세요.")?;
            let requested = PathBuf::from(path).canonical().map_err(|_| "옮길 폴더를 찾을 수 없습니다.".to_string())?;
            if requested != chosen || !chosen.is_dir() { return Err("폴더 선택 창에서 옮길 폴더를 다시 선택해 주세요.".into()); }
            (chosen, false)
        }
    };
    let mut items = Vec::new(); let mut job_items = Vec::new(); let mut used = std::collections::HashSet::new();
    let total = paths.len();
    for (index, source_text) in paths.into_iter().enumerate() {
        let source = PathBuf::from(&source_text);
        let original = source.file_name().and_then(|value| value.to_str()).unwrap_or("선택한 파일").to_string();
        let filename = names.as_ref().and_then(|names| names.get(index)).map(|name| name.trim().to_string()).unwrap_or_else(|| original.clone());
        if source.parent() == Some(folder.as_path()) && filename == original {
            items.push(CollectedFile { source: source_text.clone(), target: source_text, status: "skipped".into(), error: Some("이미 이 폴더에 있어 그대로 두었습니다.".into()) });
            continue;
        }
        // Never overwrite: names already taken in this batch or in the folder get " (2)", " (3)"…
        let name_path = Path::new(&filename);
        let stem = name_path.file_stem().and_then(|value| value.to_str()).unwrap_or(&filename).to_string();
        let extension = name_path.extension().and_then(|value| value.to_str()).map(|ext| format!(".{ext}")).unwrap_or_default();
        let mut target_name = filename.clone(); let mut suffix = 2;
        while used.contains(&target_name.to_lowercase()) || std::fs::symlink_metadata(folder.join(&target_name)).is_ok() { target_name = format!("{stem} ({suffix}){extension}"); suffix += 1; }
        used.insert(target_name.to_lowercase());
        let target = folder.join(&target_name);
        let _ = app.emit("collect-progress", CollectProgress { done: index, total, name: filename.clone() });
        let result = move_file(&source, &target);
        let (size, modified_ns) = result.as_ref().map(metadata_key).unwrap_or((0, None));
        let error = result.as_ref().err().cloned();
        job_items.push(PlanItem { source: source_text.clone(), target: target.to_string_lossy().into_owned(), action: "move".into(), size, modified_ns, sha256: String::new(), status: if error.is_none() { "succeeded" } else { "failed" }.into(), error: error.clone() });
        items.push(CollectedFile { source: source_text, target: target.to_string_lossy().into_owned(), status: if error.is_none() { "moved" } else { "failed" }.into(), error });
    }
    *state.selected_files.lock().map_err(|_| "선택 파일 상태를 초기화할 수 없습니다.".to_string())? = Vec::new();
    // Don't leave an empty new folder on the desktop when nothing could be moved.
    let removed = created && !items.iter().any(|item| item.status == "moved") && std::fs::remove_dir(&folder).is_ok();
    let job_id = if job_items.iter().any(|item| item.status == "succeeded") {
        let job = Plan { id: format!("{}-{}-{}", chrono_like_now(), std::process::id(), PLAN_COUNTER.fetch_add(1, Ordering::SeqCst)), status: finish_status(&job_items).into(), created: chrono_like_now(), root: "/".into(), items: job_items };
        database(&state).ok().and_then(|db| record_finished_job(&db, &job).ok()).map(|_| job.id)
    } else { None };
    Ok(CollectionResult { folder: if removed { String::new() } else { folder.to_string_lossy().into_owned() }, items, job_id })
}

/// Moves one regular file without ever replacing an existing one. Same volume: one atomic
/// no-replace rename. Another volume: verified copy, then the original is removed only if it
/// is still exactly the file that was copied. Returns the moved file's metadata.
fn move_file(source: &Path, target: &Path) -> Result<std::fs::Metadata, String> {
    let before = std::fs::symlink_metadata(source).map_err(|_| "원본 파일을 다시 확인할 수 없습니다.".to_string())?;
    if before.file_type().is_symlink() || !before.is_file() { return Err("선택한 항목이 일반 파일이 아니어서 건너뛰었습니다.".into()); }
    if std::fs::symlink_metadata(target).is_ok() { return Err("옮길 폴더에 같은 이름의 파일이 있습니다.".into()); }
    match rename_noreplace(Path::new("/"), source, target) {
        Ok(()) => {}
        Err(error) if is_cross_volume(&error) => {
            copy_verified(source, target)?;
            let unchanged = std::fs::symlink_metadata(source).is_ok_and(|after| metadata_key(&after) == metadata_key(&before));
            if !unchanged { let _ = std::fs::remove_file(target); return Err("옮기는 도중 원본이 바뀌어 원본은 그대로 두었습니다.".into()); }
            std::fs::remove_file(source).map_err(|_| "복사는 끝났지만 원본을 지우지 못해 두 곳에 모두 남아 있습니다.".to_string())?;
        }
        Err(error) => return Err(format!("{} 원본은 그대로 두었습니다.", rename_error_message(&error))),
    }
    std::fs::symlink_metadata(target).map_err(|_| "옮긴 파일을 확인할 수 없습니다.".to_string())
}

fn record_finished_job(db: &Connection, job: &Plan) -> rusqlite::Result<()> {
    let tx = db.unchecked_transaction()?;
    tx.execute("INSERT INTO jobs(id,created,status,root) VALUES (?1,?2,?3,?4)", rusqlite::params![job.id, job.created, job.status, job.root])?;
    for item in &job.items {
        tx.execute("INSERT INTO job_items(job_id,source,target,action,size,modified_ns,sha256,status,error) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)", rusqlite::params![job.id,item.source,item.target,item.action,item.size,item.modified_ns.map(|value| value.to_string()),item.sha256,item.status,item.error])?;
    }
    tx.commit()
}

/// Copies one file and proves the copy matches, reading as little as possible.
/// Same APFS volume: clonefile(2) makes an instant copy-on-write snapshot, so the
/// contents are identical by construction. Otherwise: one streaming pass that hashes
/// while it writes, then one hash of the copy. The source must not change meanwhile.
fn copy_verified(source: &Path, target: &Path) -> Result<(), String> {
    let before = std::fs::symlink_metadata(source).map_err(|_| "원본 파일을 다시 확인할 수 없습니다.".to_string())?;
    if before.file_type().is_symlink() || !before.is_file() { return Err("선택한 항목이 일반 파일이 아니어서 건너뛰었습니다.".into()); }
    #[cfg(target_os = "macos")]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let discard = |message: &str| { let _ = std::fs::remove_file(target); Err(message.to_string()) };
        let (Ok(from), Ok(to)) = (CString::new(source.as_os_str().as_bytes()), CString::new(target.as_os_str().as_bytes())) else { return Err("파일 경로를 읽을 수 없습니다.".into()); };
        // 0x0001 = CLONE_NOFOLLOW (sys/clonefile.h); fails with EXDEV across volumes and EEXIST if the target exists.
        if unsafe { libc::clonefile(from.as_ptr(), to.as_ptr(), 0x0001) } == 0 {
            let unchanged = std::fs::symlink_metadata(source).is_ok_and(|after| metadata_key(&after) == metadata_key(&before));
            let same_size = std::fs::symlink_metadata(target).is_ok_and(|copy| copy.is_file() && copy.len() == before.len());
            return if unchanged && same_size { Ok(()) } else { discard("복사 도중 파일이 변경되어 결과를 버렸습니다.") };
        }
        if std::io::Error::last_os_error().kind() == std::io::ErrorKind::AlreadyExists { return Err("새 폴더 안에 같은 이름의 파일이 있습니다.".into()); }
    }
    copy_streaming_verified(source, target, &before)
}

/// Fallback when cloning is impossible (another volume): hash while copying, then hash the copy.
fn copy_streaming_verified(source: &Path, target: &Path, before: &std::fs::Metadata) -> Result<(), String> {
    let before = before.clone();
    let discard = |message: &str| { let _ = std::fs::remove_file(target); Err(message.to_string()) };
    let mut input = File::open(source).map_err(|_| "원본 파일을 열 수 없습니다.".to_string())?;
    if input.metadata().map(|meta| metadata_key(&meta)).ok() != Some(metadata_key(&before)) { return Err("원본을 안정적으로 읽지 못했습니다.".into()); }
    let mut output = std::fs::OpenOptions::new().write(true).create_new(true).open(target).map_err(|_| "새 폴더 안에 파일을 만들 수 없습니다.".to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = match input.read(&mut buffer) { Ok(read) => read, Err(_) => return discard("파일 복사에 실패했습니다.") };
        if read == 0 { break; }
        hasher.update(&buffer[..read]);
        if std::io::Write::write_all(&mut output, &buffer[..read]).is_err() { return discard("파일 복사에 실패했습니다."); }
    }
    drop(output);
    let expected = format!("{:x}", hasher.finalize());
    let source_unchanged = input.metadata().is_ok_and(|meta| metadata_key(&meta) == metadata_key(&before))
        && std::fs::metadata(source).is_ok_and(|meta| metadata_key(&meta) == metadata_key(&before));
    match hash_stable(target) {
        Ok((copied, _)) if source_unchanged && copied == expected => Ok(()),
        _ => discard("복사 도중 파일이 변경되어 결과를 버렸습니다."),
    }
}

/// Valid for a new folder or a moved file's name: visible, one path component, allowed by Windows.
fn valid_folder_name(name: &str) -> bool {
    let device = name.split('.').next().unwrap_or(name).trim_end().to_uppercase();
    let reserved = matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (device.len() == 4 && (device.starts_with("COM") || device.starts_with("LPT")) && matches!(device.as_bytes()[3], b'1'..=b'9'));
    !name.is_empty() && name.len() <= 255 && !name.starts_with('.') && !name.ends_with('.') && !name.ends_with(' ') && !reserved
        && !name.chars().any(|c| matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*') || c.is_control())
}

fn classify(ext: Option<&str>) -> &'static str {
    match ext.unwrap_or("").to_lowercase().as_str() {
        "pdf"|"doc"|"docx"|"txt"|"rtf"|"md"|"xls"|"xlsx"|"ppt"|"pptx"|"hwp"|"hwpx"|"csv" => "문서",
        "jpg"|"jpeg"|"png"|"gif"|"webp"|"heic"|"bmp"|"tif"|"tiff"|"svg" => "사진",
        "mp4"|"mov"|"mkv"|"avi"|"webm"|"m4v" => "영상",
        "zip"|"7z"|"rar"|"tar"|"gz"|"bz2" => "압축",
        _ => "기타"
    }
}
fn visible_entry(entry: &walkdir::DirEntry) -> bool {
    if entry.depth() == 0 { return true; }
    if entry.file_name().to_string_lossy().starts_with('.') || entry.file_type().is_symlink() { return false; }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if entry.metadata().map(|m| m.file_attributes() & 0x2 != 0).unwrap_or(false) { return false; }
    }
    true
}
fn emit(app: &tauri::AppHandle, kind: &str, item: Option<Entry>, scanned: usize, message: Option<String>) {
    let _ = app.emit("scan-event", ScanEvent { kind: kind.into(), item, scanned, message });
}
fn save(db: &Connection, path: &str, root: &str, item: &Entry) -> rusqlite::Result<()> {
    db.execute("INSERT OR REPLACE INTO files(path,root,name,extension,size,modified,category) VALUES (?1,?2,?3,?4,?5,?6,?7)", params![path,root,item.name,item.extension,item.size,item.modified,item.category])?;
    Ok(())
}

fn replace_index(db_path: &Path, root: &str, items: &[Entry]) -> rusqlite::Result<()> {
    let mut db = open_db(db_path)?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS files(path TEXT PRIMARY KEY, root TEXT NOT NULL, name TEXT NOT NULL, extension TEXT, size INTEGER NOT NULL, modified INTEGER, category TEXT NOT NULL); CREATE INDEX IF NOT EXISTS files_root ON files(root);")?;
    let tx = db.transaction()?;
    tx.execute("DELETE FROM files WHERE root = ?1", [root])?;
    for item in items { save(&tx, &item.path, root, item)?; }
    tx.commit()
}

fn metadata_key(metadata: &std::fs::Metadata) -> (u64, Option<u128>) {
    let modified = metadata.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map(|duration| duration.as_nanos());
    (metadata.len(), modified)
}

fn hash_stable(path: &Path) -> std::io::Result<(String, std::fs::Metadata)> {
    let path_before = std::fs::metadata(path)?;
    let mut file = File::open(path)?;
    let file_before = file.metadata()?;
    if metadata_key(&path_before) != metadata_key(&file_before) { return Err(std::io::Error::other("file changed before hashing")); }
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 { break; }
        hasher.update(&buffer[..read]);
    }
    let file_after = file.metadata()?;
    let path_after = std::fs::metadata(path)?;
    if metadata_key(&file_before) != metadata_key(&file_after) || metadata_key(&file_after) != metadata_key(&path_after) {
        return Err(std::io::Error::other("file changed while hashing"));
    }
    Ok((format!("{:x}", hasher.finalize()), file_after))
}

fn same_bytes(left: &Path, right: &Path, left_expected: &(u64, Option<u128>), right_expected: &(u64, Option<u128>), cancel: &AtomicBool) -> std::io::Result<bool> {
    let mut left_file = File::open(left)?;
    let mut right_file = File::open(right)?;
    let left_before = left_file.metadata()?;
    let right_before = right_file.metadata()?;
    if metadata_key(&left_before) != *left_expected || metadata_key(&right_before) != *right_expected {
        return Err(std::io::Error::other("file changed after hashing"));
    }
    let mut left_buffer = [0u8; 64 * 1024];
    let mut right_buffer = [0u8; 64 * 1024];
    loop {
        if cancel.load(Ordering::SeqCst) { return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled")); }
        let left_read = left_file.read(&mut left_buffer)?;
        let right_read = right_file.read(&mut right_buffer)?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] { return Ok(false); }
        if left_read == 0 {
            let left_after = left_file.metadata()?; let right_after = right_file.metadata()?;
            let left_path_after = std::fs::metadata(left)?; let right_path_after = std::fs::metadata(right)?;
            if metadata_key(&left_after) != *left_expected || metadata_key(&right_after) != *right_expected ||
                metadata_key(&left_path_after) != *left_expected || metadata_key(&right_path_after) != *right_expected {
                return Err(std::io::Error::other("file changed during byte comparison"));
            }
            return Ok(true);
        }
    }
}

fn selected_root(path: String, state: &ScanState) -> Result<PathBuf, String> {
    let requested = PathBuf::from(path).canonical().map_err(|_| "선택한 폴더를 열 수 없습니다.".to_string())?;
    let authorized = state.selected_root.lock().map_err(|_| "폴더 선택 상태를 확인할 수 없습니다.".to_string())?;
    if authorized.as_ref() != Some(&requested) { return Err("앱의 폴더 선택 대화상자에서 폴더를 먼저 선택해 주세요.".into()); }
    if !requested.is_dir() { return Err("폴더를 선택해 주세요.".into()); }
    Ok(requested)
}

fn database(state: &ScanState) -> Result<Connection, String> {
    let path = state.db_path.lock().map_err(|_| "로컬 저장소를 사용할 수 없습니다.".to_string())?.clone().ok_or("로컬 저장소를 사용할 수 없습니다.")?;
    let db = open_db(&path).map_err(|_| "로컬 저장소를 열 수 없습니다.".to_string())?;
    ensure_history_schema(&db)
        .map_err(|_| "작업 이력 저장소를 초기화할 수 없습니다.".to_string())?;
    Ok(db)
}

// WAL lets history writes proceed while a scan reads, and the busy timeout
// waits out the short index commit instead of failing immediately.
fn open_db(path: &Path) -> rusqlite::Result<Connection> {
    let db = Connection::open(path)?;
    db.busy_timeout(std::time::Duration::from_secs(15))?;
    let _: String = db.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
    Ok(db)
}

fn table_columns(db:&Connection, table:&str)->rusqlite::Result<Vec<String>> {
    db.prepare(&format!("PRAGMA table_info({table})"))?.query_map([],|row|row.get::<_,String>(1))?.collect()
}

fn ensure_history_schema(db:&Connection)->rusqlite::Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS jobs(id TEXT PRIMARY KEY, created INTEGER NOT NULL, status TEXT NOT NULL, root TEXT); CREATE TABLE IF NOT EXISTS job_items(id INTEGER PRIMARY KEY AUTOINCREMENT, job_id TEXT NOT NULL, source TEXT NOT NULL, target TEXT NOT NULL, action TEXT NOT NULL, size INTEGER NOT NULL, modified_ns TEXT, sha256 TEXT, status TEXT NOT NULL, error TEXT, FOREIGN KEY(job_id) REFERENCES jobs(id));")?;
    if !table_columns(db,"job_items")?.iter().any(|column|column=="sha256") { db.execute("ALTER TABLE job_items ADD COLUMN sha256 TEXT",[])?; }
    if !table_columns(db,"jobs")?.iter().any(|column|column=="root") { db.execute("ALTER TABLE jobs ADD COLUMN root TEXT",[])?; }
    Ok(())
}

/// Picked-file moves are recorded with root "/" (anywhere), which on Windows means the path's own drive.
fn path_root<'a>(root: &'a Path, path: &'a Path) -> &'a Path {
    if root == Path::new("/") { path.ancestors().last().unwrap_or(root) } else { root }
}

fn ensure_no_symlink(root: &Path, parent: &Path) -> Result<(), String> {
    let root = path_root(root, parent);
    if !parent.starts_with(root) { return Err("대상은 선택한 폴더 안에 있어야 합니다.".into()); }
    let relative_input = parent.strip_prefix(root).map_err(|_| "대상 경로가 선택 범위를 벗어났습니다.".to_string())?;
    let mut lexical = root.to_path_buf();
    for component in relative_input.components() {
        match component {
            Component::Normal(part) => lexical.push(part),
            Component::CurDir => {},
            _ => return Err("대상 경로에 상위 폴더 이동이 포함되어 있습니다.".into()),
        }
        if std::fs::symlink_metadata(&lexical).map(|meta| meta.file_type().is_symlink()).unwrap_or(true) {
            return Err("대상 경로에 심볼릭 링크 또는 접근할 수 없는 항목이 있습니다.".into());
        }
    }
    let canonical_parent = parent.canonical().map_err(|_| "대상 폴더를 찾을 수 없습니다.".to_string())?;
    if !canonical_parent.starts_with(root) { return Err("대상은 선택한 폴더 안에 있어야 합니다.".into()); }
    Ok(())
}

fn ensure_target_parent(root: &Path, parent: &Path) -> Result<(), String> {
    if !parent.starts_with(root) { return Err("대상은 선택한 폴더 안에 있어야 합니다.".into()); }
    let relative = parent.strip_prefix(root).map_err(|_| "대상 경로가 선택 범위를 벗어났습니다.".to_string())?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else { return Err("대상 경로에 상위 폴더 이동이 포함되어 있습니다.".into()); };
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => return Err("대상 경로에 디렉터리가 아닌 항목이 있습니다.".into()),
            Ok(_) => {},
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err("대상 폴더를 확인할 수 없습니다.".into()),
        }
    }
    ensure_no_symlink(root, parent)
}

fn validate_operation(root: &Path, request: OperationRequest) -> Result<PlanItem, String> {
    let source = PathBuf::from(&request.source);
    if std::fs::symlink_metadata(&source).map(|meta| meta.file_type().is_symlink()).unwrap_or(true) { return Err("원본 파일을 찾을 수 없거나 심볼릭 링크입니다.".into()); }
    let canonical_source = source.canonical().map_err(|_| "원본 파일을 찾을 수 없습니다.".to_string())?;
    if !canonical_source.starts_with(root) || !canonical_source.is_file() { return Err("원본은 선택한 폴더 안의 일반 파일이어야 합니다.".into()); }
    ensure_no_symlink(root, source.parent().ok_or("원본 폴더를 확인할 수 없습니다.")?)?;
    let target = PathBuf::from(&request.target);
    let target_name = target.file_name().and_then(|name| name.to_str()).ok_or("새 파일 이름이 올바르지 않습니다.")?;
    if target_name.is_empty() || target_name == "." || target_name == ".." { return Err("새 파일 이름이 올바르지 않습니다.".into()); }
    let parent = target.parent().ok_or("대상 폴더를 지정해 주세요.")?;
    ensure_target_parent(root, parent)?;
    if (target.exists() || std::fs::symlink_metadata(&target).is_ok()) && !case_variant_of_same_file(&canonical_source, &target) { return Err("대상 경로에 이미 파일이 있습니다. 덮어쓰기는 허용되지 않습니다.".into()); }
    if canonical_source == target { return Err("원본과 대상이 같습니다.".into()); }
    let metadata = std::fs::metadata(&canonical_source).map_err(|_| "원본 상태를 읽을 수 없습니다.".to_string())?;
    let (size, modified) = metadata_key(&metadata);
    let (sha256, hashed_metadata)=hash_stable(&canonical_source).map_err(|_| "원본을 안정적으로 읽을 수 없어 계획을 만들지 못했습니다.".to_string())?;
    if metadata_key(&metadata)!=metadata_key(&hashed_metadata) { return Err("계획을 만드는 동안 원본 파일이 바뀌었습니다. 다시 시도해 주세요.".into()); }
    Ok(PlanItem { source: canonical_source.to_string_lossy().into_owned(), target: target.to_string_lossy().into_owned(), action: if canonical_source.parent() == Some(parent) { "rename" } else { "move" }.into(), size, modified_ns: modified, sha256, status: "pending".into(), error: None })
}

fn persist_item(db: &Connection, plan_id: &str, item: &PlanItem) -> Result<(), String> {
    db.execute("UPDATE job_items SET status=?1,error=?2 WHERE job_id=?3 AND source=?4", rusqlite::params![item.status,item.error,plan_id,item.source]).map_err(|_| "작업 이력을 갱신하지 못했습니다.".to_string())?;
    Ok(())
}

fn apply_plan_item(root: &Path, item: &mut PlanItem) {
    let refreshed = validate_operation(root, OperationRequest { source: item.source.clone(), target: item.target.clone() });
    match refreshed {
        Err(error) => { item.status = "failed".into(); item.error = Some(error); }
        Ok(current) if current.size != item.size || current.modified_ns != item.modified_ns || current.sha256 != item.sha256 => {
            item.status = "failed".into(); item.error = Some("계획을 만든 뒤 원본이 바뀌었습니다. 계획을 다시 확인해 주세요.".into());
        }
        Ok(_) => match rename_noreplace(root,Path::new(&item.source),Path::new(&item.target)) {
            Ok(()) => item.status="succeeded".into(),
            Err(error) => { item.status = "failed".into(); item.error = Some(format!("{} 원본은 보존했습니다.", rename_error_message(&error))); }
        }
    }
}

fn restore_plan_item(root: &Path, item: &mut PlanItem) {
    let source_path = PathBuf::from(&item.source); let target_path = PathBuf::from(&item.target);
    let result = (|| -> Result<(),String> {
        ensure_no_symlink(root,source_path.parent().ok_or("원래 폴더를 확인할 수 없습니다.")?)?;
        ensure_no_symlink(root,target_path.parent().ok_or("대상 폴더를 확인할 수 없습니다.")?)?;
        if (source_path.exists() && !case_variant_of_same_file(&target_path, &source_path)) || !target_path.is_file() { return Err("원래 위치가 점유되었거나 이동된 파일이 없어 복구할 수 없습니다.".into()); }
        // Picked-file moves record no hash (hashing would make big moves slow); their size and
        // nanosecond mtime still have to match exactly.
        let metadata = if item.sha256.is_empty() { std::fs::symlink_metadata(&target_path).map_err(|_| "이동된 파일을 읽을 수 없습니다.".to_string())? } else {
            let (sha256,metadata)=hash_stable(&target_path).map_err(|_| "이동된 파일을 안정적으로 읽을 수 없습니다.".to_string())?;
            if sha256!=item.sha256 { return Err("이동된 파일이 바뀌어 자동 복구를 중단했습니다.".into()); }
            metadata
        };
        if metadata_key(&metadata).0 != item.size || metadata_key(&metadata).1 != item.modified_ns { return Err("이동된 파일이 바뀌어 자동 복구를 중단했습니다.".into()); }
        match rename_noreplace(root,&target_path,&source_path) {
            Ok(()) => Ok(()),
            Err(error) if is_cross_volume(&error) => {
                copy_verified(&target_path,&source_path).map_err(|message| format!("원래 위치로 복구할 수 없습니다. {message} 파일은 현재 경로에 보존했습니다."))?;
                std::fs::remove_file(&target_path).map_err(|_| "원래 위치에 되돌렸지만 옮겨진 파일을 지우지 못해 두 곳에 모두 남아 있습니다.".to_string())
            }
            Err(error) => Err(format!("원래 위치로 복구할 수 없습니다. {} 파일은 현재 경로에 보존했습니다.", rename_error_message(&error))),
        }
    })();
    match result { Ok(()) => item.status="undone".into(), Err(error) => { item.status="failed".into(); item.error=Some(error); } }
}

// On case-insensitive volumes (default APFS) "a.txt" → "A.txt" sees the target as
// existing. Only a same-folder, case-only change of the very same inode counts.
fn case_variant_of_same_file(source: &Path, target: &Path) -> bool {
    let (Some(source_name), Some(target_name)) = (source.file_name().and_then(|n| n.to_str()), target.file_name().and_then(|n| n.to_str())) else { return false; };
    if source.parent() != target.parent() || source_name == target_name || source_name.to_lowercase() != target_name.to_lowercase() { return false; }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::symlink_metadata(source), std::fs::symlink_metadata(target)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    // canonical() returns the name as stored on disk, so both spellings resolve to one path.
    { matches!((source.canonical(), target.canonical()), (Ok(a), Ok(b)) if a == b) }
}

fn is_cross_volume(error: &std::io::Error) -> bool {
    #[cfg(windows)]
    { error.raw_os_error() == Some(17) } // ERROR_NOT_SAME_DEVICE
    #[cfg(not(windows))]
    { error.raw_os_error() == Some(libc::EXDEV) }
}

fn rename_error_message(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::AlreadyExists => "그 사이 대상 경로에 다른 파일이 생겨 덮어쓰지 않았습니다.".into(),
        std::io::ErrorKind::NotFound => "원본 파일이나 대상 폴더를 찾을 수 없습니다.".into(),
        std::io::ErrorKind::PermissionDenied => "이 위치에 대한 권한이 없습니다.".into(),
        std::io::ErrorKind::Unsupported => "이 운영체제에서는 덮어쓰기 없는 안전한 이동을 지원하지 않습니다.".into(),
        _ if is_cross_volume(error) => "다른 디스크(볼륨)로는 이동할 수 없습니다.".into(),
        _ => format!("파일을 옮기지 못했습니다 ({error})."),
    }
}

#[cfg(unix)] fn rename_noreplace(root:&Path,source:&Path,target:&Path)->std::io::Result<()> {
    use std::{ffi::CString,os::{fd::{AsRawFd,FromRawFd},unix::{ffi::OsStrExt,fs::OpenOptionsExt}}};
    fn open_dir(root:&Path,path:&Path,create:bool)->std::io::Result<File>{
        let mut current=std::fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(root)?;
        let relative=path.strip_prefix(root).map_err(|_|std::io::Error::new(std::io::ErrorKind::PermissionDenied,"outside root"))?;
        for component in relative.components(){ if let Component::Normal(name)=component {
            let name=CString::new(name.as_bytes()).map_err(|_|std::io::Error::new(std::io::ErrorKind::InvalidInput,"invalid directory name"))?;
            let fd=unsafe{libc::openat(current.as_raw_fd(),name.as_ptr(),libc::O_RDONLY|libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC)};
            if fd<0 {
                let error=std::io::Error::last_os_error();
                if !create || error.kind()!=std::io::ErrorKind::NotFound { return Err(error); }
                if unsafe{libc::mkdirat(current.as_raw_fd(),name.as_ptr(),0o755)}<0 { let mkdir_error=std::io::Error::last_os_error(); if mkdir_error.kind()!=std::io::ErrorKind::AlreadyExists{return Err(mkdir_error);} }
                let fd=unsafe{libc::openat(current.as_raw_fd(),name.as_ptr(),libc::O_RDONLY|libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC)};
                if fd<0{return Err(std::io::Error::last_os_error());}
                current=unsafe{File::from_raw_fd(fd)};
            } else { current=unsafe{File::from_raw_fd(fd)}; }
        }} Ok(current)
    }
    let source_parent=source.parent().ok_or_else(||std::io::Error::new(std::io::ErrorKind::InvalidInput,"missing source parent"))?;
    let target_parent=target.parent().ok_or_else(||std::io::Error::new(std::io::ErrorKind::InvalidInput,"missing target parent"))?;
    let source_dir=open_dir(root,source_parent,false)?; let target_dir=open_dir(root,target_parent,true)?;
    let source_name=CString::new(source.file_name().ok_or_else(||std::io::Error::new(std::io::ErrorKind::InvalidInput,"missing source name"))?.as_bytes()).map_err(|_|std::io::Error::new(std::io::ErrorKind::InvalidInput,"invalid source name"))?;
    let target_name=CString::new(target.file_name().ok_or_else(||std::io::Error::new(std::io::ErrorKind::InvalidInput,"missing target name"))?.as_bytes()).map_err(|_|std::io::Error::new(std::io::ErrorKind::InvalidInput,"invalid target name"))?;
    #[cfg(target_os="macos")] let result=unsafe{libc::renameatx_np(source_dir.as_raw_fd(),source_name.as_ptr(),target_dir.as_raw_fd(),target_name.as_ptr(),libc::RENAME_EXCL)};
    #[cfg(target_os="linux")] let result=unsafe{libc::renameat2(source_dir.as_raw_fd(),source_name.as_ptr(),target_dir.as_raw_fd(),target_name.as_ptr(),libc::RENAME_NOREPLACE)};
    #[cfg(not(any(target_os="macos",target_os="linux")))] let result={ let _=(source_dir,target_dir,source_name,target_name); return Err(std::io::Error::new(std::io::ErrorKind::Unsupported,"safe no-replace rename is unsupported")); };
    if result==0 { return Ok(()); }
    let error=std::io::Error::last_os_error();
    // A case-only rename of the same file: the "existing" target is the source itself.
    if error.kind()==std::io::ErrorKind::AlreadyExists && case_variant_of_same_file(source,target) {
        if unsafe{libc::renameat(source_dir.as_raw_fd(),source_name.as_ptr(),target_dir.as_raw_fd(),target_name.as_ptr())}==0 { return Ok(()); }
        return Err(std::io::Error::last_os_error());
    }
    Err(error)
}
// Without MOVEFILE_REPLACE_EXISTING this fails on an existing target, and without
// MOVEFILE_COPY_ALLOWED it fails across volumes. Callers check `root` with ensure_no_symlink.
#[cfg(windows)] fn rename_noreplace(_root:&Path,source:&Path,target:&Path)->std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{core::PCWSTR,Win32::Storage::FileSystem::{MoveFileExW,MOVE_FILE_FLAGS}};
    if let Some(parent)=target.parent() { std::fs::create_dir_all(parent)?; }
    let wide=|path:&Path|path.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<u16>>();
    let (from,to)=(wide(source),wide(target));
    unsafe{MoveFileExW(PCWSTR(from.as_ptr()),PCWSTR(to.as_ptr()),MOVE_FILE_FLAGS(0))}.map_err(std::io::Error::from)
}
#[cfg(not(any(unix,windows)))] fn rename_noreplace(_root:&Path,_source:&Path,_target:&Path)->std::io::Result<()> { Err(std::io::Error::new(std::io::ErrorKind::Unsupported,"safe no-replace rename is unsupported on this platform")) }

const HISTORY_WARNING: &str = "파일은 처리됐지만 작업 이력을 저장하지 못했습니다. 앱을 다시 시작하면 이 항목을 되돌릴 수 없을 수 있습니다.";

fn finish_status(items: &[PlanItem]) -> &'static str {
    if items.iter().all(|item| item.status == "succeeded") { "succeeded" }
    else if items.iter().any(|item| item.status == "succeeded" || item.status == "rollback-needed") { "partial-failure" } else { "failed" }
}

// Runs every item even if a history write fails, so the in-memory plan and the
// returned result always reflect what actually happened on disk.
fn run_plan(db: &Connection, plan: &mut Plan) {
    let root = PathBuf::from(&plan.root);
    let mut history_saved = true;
    for item in &mut plan.items {
        apply_plan_item(&root, item);
        if persist_item(db, &plan.id, item).is_err() { history_saved = false; }
    }
    plan.status = finish_status(&plan.items).into();
    if db.execute("UPDATE jobs SET status=?1 WHERE id=?2", rusqlite::params![plan.status,plan.id]).is_err() { history_saved = false; }
    if !history_saved {
        for item in plan.items.iter_mut().filter(|item| item.status == "succeeded") { item.error = Some(HISTORY_WARNING.into()); }
    }
}

#[tauri::command(async)]
fn execute_plan(plan_id: String, state: State<'_, ScanState>) -> Result<Plan, String> {
    let _guard = state.operations.lock().map_err(|_| "다른 파일 작업이 진행 중입니다.".to_string())?;
    let mut plan = {
        let mut plans = state.plans.lock().map_err(|_| "계획 상태를 확인할 수 없습니다.".to_string())?;
        let plan = plans.get_mut(&plan_id).ok_or("승인할 수 있는 계획이 없습니다. 계획을 새로 만들어 주세요.")?;
        if plan.status != "pending" { return Err("이 계획은 이미 처리되었거나 실행할 수 없습니다.".into()); }
        plan.status = "running".into();
        plan.clone()
    };
    // Refuse to touch files unless the job can be marked as running first.
    let started = database(&state).and_then(|db| {
        db.execute("UPDATE jobs SET status='running' WHERE id=?1 AND status='pending'", [&plan_id]).map_err(|_| "작업 상태를 갱신하지 못했습니다.".to_string())?;
        Ok(db)
    });
    let db = match started {
        Ok(db) => db,
        Err(error) => {
            if let Ok(mut plans) = state.plans.lock() { if let Some(stored) = plans.get_mut(&plan_id) { stored.status = "pending".into(); } }
            return Err(format!("{error} 파일은 변경하지 않았습니다."));
        }
    };
    run_plan(&db, &mut plan);
    state.plans.lock().map_err(|_| "계획 상태를 저장할 수 없습니다.".to_string())?.insert(plan_id, plan.clone());
    Ok(plan)
}

fn undo_job(db: &Connection, plan_id: &str, fallback_root: Option<PathBuf>) -> Result<Plan, String> {
    let stored_root: Option<String> = db.query_row("SELECT root FROM jobs WHERE id=?1", [plan_id], |row| row.get(0)).map_err(|_| "복구할 작업 이력이 없습니다.".to_string())?;
    // Jobs recorded before the root column existed fall back to the current folder.
    let root = match stored_root {
        Some(root) if root == "/" => PathBuf::from(root),
        Some(root) => PathBuf::from(root).canonical().ok().filter(|path| path.is_dir()).ok_or("작업했던 폴더를 찾을 수 없어 되돌릴 수 없습니다.")?,
        None => fallback_root.ok_or("이 작업은 폴더 정보가 없는 이전 이력입니다. 작업했던 폴더를 먼저 선택해 주세요.")?,
    };
    let mut statement = db.prepare("SELECT source,target,size,modified_ns,sha256,status FROM job_items WHERE job_id=?1 ORDER BY id").map_err(|_| "작업 이력을 읽을 수 없습니다.".to_string())?;
    let rows = statement.query_map([plan_id], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,u64>(2)?,row.get::<_,Option<String>>(3)?,row.get::<_,Option<String>>(4)?,row.get::<_,String>(5)?))).map_err(|_| "작업 이력을 읽을 수 없습니다.".to_string())?;
    let mut plan = Plan { id: plan_id.to_string(), items: Vec::new(), status: "undoing".into(), created: chrono_like_now(), root: root.to_string_lossy().into_owned() };
    let mut history_saved = true;
    for row in rows {
        let (source,target,size,modified,sha256,status) = row.map_err(|_| "작업 이력을 읽을 수 없습니다.".to_string())?;
        let mut item = PlanItem { source: source.clone(),target: target.clone(),action:"undo".into(),size,modified_ns:modified.and_then(|value| value.parse().ok()),sha256:sha256.unwrap_or_default(),status,error:None };
        if item.status == "succeeded" {
            restore_plan_item(&root,&mut item);
            if persist_item(db, plan_id, &item).is_err() { history_saved = false; }
        }
        plan.items.push(item);
    }
    if plan.items.is_empty() { return Err("복구할 작업 이력이 없습니다.".into()); }
    plan.status = if plan.items.iter().all(|item| item.status == "undone") { "undone" } else { "undo-partial" }.into();
    if db.execute("UPDATE jobs SET status=?1 WHERE id=?2",rusqlite::params![plan.status,plan_id]).is_err() { history_saved = false; }
    if !history_saved {
        for item in plan.items.iter_mut().filter(|item| item.status == "undone") { item.error = Some("복구는 됐지만 이력을 갱신하지 못했습니다.".into()); }
    }
    Ok(plan)
}

#[tauri::command(async)]
fn undo_operation(plan_id: String, state: State<'_, ScanState>) -> Result<Plan, String> {
    let _guard = state.operations.lock().map_err(|_| "다른 파일 작업이 진행 중입니다.".to_string())?;
    let fallback_root = state.selected_root.lock().map_err(|_| "폴더 선택 상태를 확인할 수 없습니다.".to_string())?.clone();
    let db = database(&state)?;
    let plan = undo_job(&db, &plan_id, fallback_root)?;
    if let Ok(mut plans) = state.plans.lock() { plans.insert(plan_id, plan.clone()); }
    Ok(plan)
}

#[tauri::command(async)]
fn list_operations(state: State<'_, ScanState>) -> Result<Vec<Plan>, String> {
    let db=database(&state)?;
    let mut jobs=db.prepare("SELECT id,status,created,root FROM jobs ORDER BY created DESC").map_err(|_| "작업 이력을 읽을 수 없습니다.".to_string())?;
    let job_rows=jobs.query_map([],|row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,i64>(2)?,row.get::<_,Option<String>>(3)?))).map_err(|_| "작업 이력을 읽을 수 없습니다.".to_string())?;
    let mut results=Vec::new();
    for job_row in job_rows {
        let (id,status,created,root)=job_row.map_err(|_| "작업 이력을 읽을 수 없습니다.".to_string())?;
        let mut statement=db.prepare("SELECT source,target,action,size,modified_ns,sha256,status,error FROM job_items WHERE job_id=?1 ORDER BY id").map_err(|_| "작업 이력을 읽을 수 없습니다.".to_string())?;
        let rows=statement.query_map([&id],|row| Ok(PlanItem { source:row.get(0)?,target:row.get(1)?,action:row.get(2)?,size:row.get(3)?,modified_ns:row.get::<_,Option<String>>(4)?.and_then(|value|value.parse().ok()),sha256:row.get::<_,Option<String>>(5)?.unwrap_or_default(),status:row.get(6)?,error:row.get(7)? })).map_err(|_| "작업 이력을 읽을 수 없습니다.".to_string())?;
        let items=rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|_| "작업 이력을 읽을 수 없습니다.".to_string())?;
        results.push(Plan{id,status,items,created,root:root.unwrap_or_default()});
    }
    Ok(results)
}

// The blocking picker must not run on the main thread: macOS shows it as a sheet whose
// completion is delivered on the main run loop, which a blocked main thread cannot service.
#[tauri::command(async)]
fn pick_destination_folder(app: tauri::AppHandle, path: String, state: State<'_, ScanState>) -> Result<Option<String>, String> {
    let root = selected_root(path, &state)?;
    let Some(chosen) = app.dialog().file().blocking_pick_folder() else { return Ok(None); };
    let folder = chosen.into_path().map_err(|_| "대상 폴더를 확인할 수 없습니다.".to_string())?.canonical().map_err(|_| "대상 폴더를 열 수 없습니다.".to_string())?;
    if !folder.starts_with(&root) || !folder.is_dir() { return Err("대상 폴더는 선택한 폴더 안이어야 합니다.".into()); }
    ensure_no_symlink(&root, &folder)?;
    Ok(Some(folder.to_string_lossy().into_owned()))
}

#[tauri::command(async)]
fn create_operation_plan(path: String, operations: Vec<OperationRequest>, state: State<'_, ScanState>) -> Result<Plan, String> {
    let root = selected_root(path, &state)?;
    if operations.is_empty() || operations.len() > 1000 { return Err("한 번에 1개에서 1000개 파일까지 계획할 수 있습니다.".into()); }
    let mut items = Vec::with_capacity(operations.len());
    let mut targets = std::collections::HashSet::new();
    let mut sources = std::collections::HashSet::new();
    for operation in operations {
        let item = validate_operation(&root, operation)?;
        if !targets.insert(item.target.clone()) { return Err("여러 작업이 같은 대상 경로를 사용합니다.".into()); }
        if !sources.insert(item.source.clone()) { return Err("같은 원본 파일이 여러 작업에 들어 있습니다.".into()); }
        items.push(item);
    }
    // The pid alone can repeat across launches, so add a timestamp to keep ids unique in history.
    let id = format!("{}-{}-{}", chrono_like_now(), std::process::id(), PLAN_COUNTER.fetch_add(1, Ordering::SeqCst));
    let plan = Plan { id, items, status: "pending".into(), created: chrono_like_now(), root: root.to_string_lossy().into_owned() };
    let db = database(&state)?;
    record_plan(&db, &plan).map_err(|_| "계획을 저장할 수 없습니다.".to_string())?;
    state.plans.lock().map_err(|_| "계획 상태를 저장할 수 없습니다.".to_string())?.insert(plan.id.clone(), plan.clone());
    Ok(plan)
}

fn record_plan(db: &Connection, plan: &Plan) -> rusqlite::Result<()> {
    let tx = db.unchecked_transaction()?;
    tx.execute("INSERT INTO jobs(id,created,status,root) VALUES (?1,?2,'pending',?3)", rusqlite::params![plan.id, plan.created, plan.root])?;
    for item in &plan.items {
        tx.execute("INSERT INTO job_items(job_id,source,target,action,size,modified_ns,sha256,status,error) VALUES (?1,?2,?3,?4,?5,?6,?7,'pending',NULL)", rusqlite::params![plan.id,item.source,item.target,item.action,item.size,item.modified_ns.map(|value| value.to_string()),item.sha256])?;
    }
    tx.commit()
}

fn chrono_like_now() -> i64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64 }

// The blocking picker must not run on the main thread: macOS shows it as a sheet whose
// completion is delivered on the main run loop, which a blocked main thread cannot service.
#[tauri::command(async)]
fn pick_scan_folder(app: tauri::AppHandle, state: State<'_, ScanState>) -> Result<Option<String>, String> {
    let Some(chosen) = app.dialog().file().blocking_pick_folder() else { return Ok(None); };
    let selected = chosen.into_path().map_err(|_| "선택한 폴더 경로를 확인할 수 없습니다.".to_string())?.canonical().map_err(|_| "선택한 폴더를 열 수 없습니다.".to_string())?;
    if !selected.is_dir() { return Err("폴더를 선택해 주세요.".into()); }
    *state.selected_root.lock().map_err(|_| "폴더 선택 상태를 저장할 수 없습니다.".to_string())? = Some(selected.clone());
    Ok(Some(selected.to_string_lossy().into_owned()))
}

fn find_duplicates(root: &Path, cancel: &AtomicBool, mut progress: impl FnMut(&str, usize, usize)) -> Result<DuplicateScanResult, String> {
    let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    let mut skipped = 0;
    for entry in WalkDir::new(root).follow_links(false).into_iter().filter_entry(visible_entry) {
        if cancel.load(Ordering::SeqCst) { return Err("스캔을 취소했습니다.".into()); }
        let entry = match entry { Ok(entry) => entry, Err(_) => { skipped += 1; continue } };
        if !entry.file_type().is_file() || !entry.path().starts_with(root) { continue; }
        match entry.metadata() { Ok(metadata) => by_size.entry(metadata.len()).or_default().push(entry.into_path()), Err(_) => skipped += 1 }
    }
    let candidates: Vec<PathBuf> = by_size.into_values().filter(|paths| paths.len() > 1).flatten().collect();
    let total = candidates.len();
    let mut hashed: HashMap<(u64, String), Vec<(PathBuf, std::fs::Metadata)>> = HashMap::new();
    for (index, path) in candidates.into_iter().enumerate() {
        if cancel.load(Ordering::SeqCst) { return Err("스캔을 취소했습니다.".into()); }
        match hash_stable(&path) {
            Ok((digest, metadata)) => { let size = metadata.len(); hashed.entry((size, digest)).or_default().push((path, metadata)); }
            Err(_) => skipped += 1,
        }
        progress("hash", index + 1, total);
    }
    let mut groups = Vec::new();
    let comparison_total: usize = hashed.values().map(|entries| entries.len().saturating_sub(1)).sum();
    let mut comparison_done = 0;
    for ((size, digest), entries) in hashed {
        let mut exact_groups: Vec<Vec<(PathBuf, std::fs::Metadata)>> = Vec::new();
        'entry: for entry in entries {
            for group in &mut exact_groups {
                if cancel.load(Ordering::SeqCst) { return Err("스캔을 취소했습니다.".into()); }
                let compared = same_bytes(&entry.0, &group[0].0, &metadata_key(&entry.1), &metadata_key(&group[0].1), cancel);
                comparison_done += 1;
                progress("compare", comparison_done, comparison_total);
                match compared {
                    Ok(true) => { group.push(entry); continue 'entry; }
                    Ok(false) => {},
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => return Err("스캔을 취소했습니다.".into()),
                    Err(_) => skipped += 1,
                }
            }
            exact_groups.push(vec![entry]);
        }
        for exact in exact_groups.into_iter().filter(|group| group.len() > 1) {
            let mut files: Vec<_> = exact.into_iter().map(|(path, metadata)| DuplicateFile {
                path: path.to_string_lossy().into_owned(), size,
                modified: metadata.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map(|duration| duration.as_secs() as i64),
            }).collect();
            files.sort_by(|a, b| a.path.cmp(&b.path));
            groups.push(DuplicateGroup { sha256: digest.clone(), files });
        }
    }
    groups.sort_by(|a, b| a.files[0].path.cmp(&b.files[0].path));
    Ok(DuplicateScanResult { groups, skipped })
}

#[tauri::command]
async fn scan_duplicates(path: String, app: tauri::AppHandle, state: State<'_, ScanState>) -> Result<DuplicateScanResult, String> {
    let root = selected_root(path, &state)?;
    // A separate flag so the periodic folder scan cannot un-cancel (or cancel) this one.
    state.duplicate_cancel.store(false, Ordering::SeqCst);
    let cancel = state.duplicate_cancel.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let result = find_duplicates(&root, &cancel, |stage, scanned, total| { let _ = app.emit("duplicate-progress", DuplicateProgress { stage: stage.into(), scanned, total }); });
        if let Ok(scan) = &result { if scan.skipped > 0 { let _ = app.emit("duplicate-warning", format!("일부 항목 {}개는 읽지 못했거나 스캔 도중 변경되어 제외했습니다.", scan.skipped)); } }
        result
    }).await.map_err(|_| "중복 스캔을 완료하지 못했습니다.".to_string())?
}

#[tauri::command]
async fn scan_folder(path: String, app: tauri::AppHandle, state: State<'_, ScanState>) -> Result<(), String> {
    let root = selected_root(path, &state)?;
    state.cancel.store(false, Ordering::SeqCst);
    let cancel = state.cancel.clone(); let root_string = root.to_string_lossy().into_owned();
    let db_path = state.db_path.lock().map_err(|_| "로컬 저장소를 사용할 수 없습니다.".to_string())?.clone().ok_or("로컬 저장소를 사용할 수 없습니다.")?;
    tauri::async_runtime::spawn_blocking(move || {
        // Walk first and write the index in one short transaction at the end, so the
        // database is not locked for the whole scan while file operations record history.
        let mut found = Vec::new();
        let mut count = 0;
        let mut skipped = 0;
        for entry in WalkDir::new(&root).follow_links(false).into_iter().filter_entry(visible_entry) {
            if cancel.load(Ordering::SeqCst) { break; }
            let entry = match entry { Ok(e) => e, Err(_) => { skipped += 1; continue } };
            if !entry.file_type().is_file() { continue; }
            let meta = match entry.metadata() { Ok(m) => m, Err(_) => { skipped += 1; continue } };
            let p = entry.path();
            if !p.starts_with(&root) { continue; }
            let name = match p.file_name().and_then(|s| s.to_str()) { Some(n) => n.to_owned(), None => continue };
            let extension = p.extension().and_then(|s| s.to_str()).map(str::to_lowercase);
            let modified = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64);
            let item = Entry { path: p.to_string_lossy().into_owned(), name, extension: extension.clone(), size: meta.len(), modified, category: classify(extension.as_deref()).into() };
            count += 1;
            emit(&app,"item",Some(item.clone()),count,None);
            found.push(item);
            if count % 128 == 0 { std::thread::yield_now(); }
        }
        if cancel.load(Ordering::SeqCst) {
            emit(&app,"cancelled",None,count,None);
            return;
        }
        if replace_index(&db_path, &root_string, &found).is_err() { emit(&app,"error",None,count,Some("로컬 인덱스를 저장하지 못했습니다.".into())); return; }
        if skipped > 0 { emit(&app,"warning",None,count,Some(format!("일부 항목 {skipped}개를 읽지 못해 건너뛰었습니다."))); }
        emit(&app,"done",None,count,None);
    }).await.map_err(|_| "스캔 작업을 완료하지 못했습니다.".to_string())?;
    Ok(())
}

#[tauri::command]
fn cancel_scan(state: State<'_, ScanState>) { state.cancel.store(true, Ordering::SeqCst); }

#[tauri::command]
fn cancel_duplicate_scan(state: State<'_, ScanState>) { state.duplicate_cancel.store(true, Ordering::SeqCst); }

// WebView downloads (<a download>) are not reliable inside Tauri, so rules are
// written through a native save dialog instead.
#[tauri::command(async)]
fn export_rules(app: tauri::AppHandle, contents: String) -> Result<bool, String> {
    if contents.len() > 1024 * 1024 || serde_json::from_str::<serde_json::Value>(&contents).is_err() { return Err("내보낼 규칙 형식이 올바르지 않습니다.".into()); }
    let Some(chosen) = app.dialog().file().set_file_name("moa-rules.json").add_filter("JSON", &["json"]).blocking_save_file() else { return Ok(false); };
    let path = chosen.into_path().map_err(|_| "저장 위치를 확인할 수 없습니다.".to_string())?;
    std::fs::write(&path, contents).map_err(|_| "규칙 파일을 저장하지 못했습니다.".to_string())?;
    Ok(true)
}

pub fn run() {
    tauri::Builder::default().plugin(tauri_plugin_dialog::init()).setup(|app| {
        let path = app.path().app_data_dir()?.join("organizer.sqlite3");
        std::fs::create_dir_all(path.parent().unwrap())?;
        let history=open_db(&path)?;
        ensure_history_schema(&history)?; history.execute_batch("UPDATE jobs SET status='interrupted' WHERE status IN ('pending','running'); UPDATE job_items SET status='interrupted',error='앱이 종료되어 실행 상태를 확인할 수 없습니다.' WHERE status IN ('pending','running');")?;
        app.manage(ScanState { cancel: Arc::new(AtomicBool::new(false)), db_path: Arc::new(Mutex::new(Some(path))), selected_root: Arc::new(Mutex::new(None)), plans: Arc::new(Mutex::new(HashMap::new())), selected_files: Arc::new(Mutex::new(Vec::new())), operations: Arc::new(Mutex::new(())), duplicate_cancel: Arc::new(AtomicBool::new(false)), move_destination: Arc::new(Mutex::new(None)) });
        Ok(())
    }).invoke_handler(tauri::generate_handler![pick_scan_folder,pick_destination_folder,scan_folder,scan_duplicates,create_operation_plan,execute_plan,undo_operation,list_operations,cancel_scan,cancel_duplicate_scan,export_rules,analysis_availability,desktop_screen_info,request_screen_capture_permission,desktop_scene,arrange_desktop_in_regions,preview_desktop_arrangement,pick_files_for_collection,pick_move_destination,move_selected_files]).run(tauri::generate_context!()).expect("error while running app");
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn windows_file_names_are_checked() {
        for name in ["보고서.pdf", "a b", "con2", "COMPANY", "LPT0"] { assert!(valid_folder_name(name), "{name}"); }
        for name in ["", ".숨김", "끝점.", "끝공백 ", "a<b", "a>b", "a\"b", "a|b", "a?b", "a*b", "a:b", "CON", "con.txt", "NUL ", "com1.log", "LPT9"] { assert!(!valid_folder_name(name), "{name}"); }
    }
    #[test] fn whole_disk_root_resolves_to_the_paths_own_root() {
        assert_eq!(path_root(Path::new("/"), Path::new("/tmp/a")), Path::new("/"));
        assert_eq!(path_root(Path::new("/tmp"), Path::new("/tmp/a")), Path::new("/tmp"));
    }
    #[test] fn classifies_common_extensions() { assert_eq!(classify(Some("PDF")),"문서"); assert_eq!(classify(Some("jpg")),"사진"); assert_eq!(classify(Some("mp4")),"영상"); assert_eq!(classify(Some("zip")),"압축"); assert_eq!(classify(None),"기타"); }
    #[test] fn unavailable_analysis_provider_is_explicit_and_offline() { let result=analysis_availability(); assert_eq!(result.status,"unavailable-no-model-installed"); assert!(!result.supports_documents && !result.supports_images && !result.network_enabled); }
    #[test] fn sqlite_index_round_trips_metadata() {
        let db = Connection::open_in_memory().unwrap(); db.execute("CREATE TABLE files(path TEXT PRIMARY KEY,root TEXT NOT NULL,name TEXT NOT NULL,extension TEXT,size INTEGER NOT NULL,modified INTEGER,category TEXT NOT NULL)",[]).unwrap();
        let item = Entry { path:"/tmp/a.pdf".into(),name:"a.pdf".into(),extension:Some("pdf".into()),size:12,modified:Some(1),category:"문서".into() };
        save(&db,&item.path,"/tmp", &item).unwrap(); assert_eq!(db.query_row("SELECT name FROM files",[],|r|r.get::<_,String>(0)).unwrap(),"a.pdf");
    }
    #[test] fn cancelled_duplicate_scan_returns_no_result() {
        let base = std::env::temp_dir().join(format!("moa-precancel-{}", std::process::id())); std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("a"), b"x").unwrap(); std::fs::write(base.join("b"), b"x").unwrap();
        let cancel = AtomicBool::new(true);
        assert!(find_duplicates(&base, &cancel, |_, _, _| {}).is_err());
        let _ = std::fs::remove_dir_all(base);
    }
    #[test] fn cancelled_index_transaction_keeps_previous_rows() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute("CREATE TABLE files(path TEXT PRIMARY KEY,root TEXT NOT NULL,name TEXT NOT NULL,extension TEXT,size INTEGER NOT NULL,modified INTEGER,category TEXT NOT NULL)",[]).unwrap();
        db.execute("INSERT INTO files(path,root,name,size,category) VALUES ('old','root','old.txt',1,'기타')",[]).unwrap();
        { let tx = db.transaction().unwrap(); tx.execute("DELETE FROM files WHERE root = 'root'",[]).unwrap(); }
        assert_eq!(db.query_row("SELECT COUNT(*) FROM files WHERE path='old'",[],|r|r.get::<_,i64>(0)).unwrap(),1);
    }
    #[test] fn scan_skips_hidden_items_and_symlinks() {
        let base = std::env::temp_dir().join(format!("moa-scan-test-{}", std::process::id()));
        let root = base.join("root"); let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap(); std::fs::create_dir_all(&outside).unwrap(); let root=root.canonical().unwrap(); let outside=outside.canonical().unwrap();
        std::fs::write(root.join("visible.txt"), "fixture").unwrap(); std::fs::write(root.join(".secret.txt"), "fixture").unwrap(); std::fs::write(outside.join("outside.txt"), "fixture").unwrap();
        #[cfg(unix)] std::os::unix::fs::symlink(&outside, root.join("linked")).unwrap();
        let found: Vec<_> = WalkDir::new(&root).follow_links(false).into_iter().filter_entry(visible_entry).filter_map(Result::ok).filter(|e| e.file_type().is_file()).collect();
        assert_eq!(found.len(), 1); assert_eq!(found[0].file_name(), "visible.txt");
        let _ = std::fs::remove_dir_all(base);
    }
    #[test] fn duplicate_scan_requires_matching_size_and_hash() {
        let base = std::env::temp_dir().join(format!("moa-hash-test-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("one.bin"), b"same bytes").unwrap();
        std::fs::write(base.join("two.bin"), b"same bytes").unwrap();
        std::fs::write(base.join("different.bin"), b"other bytes").unwrap();
        std::fs::write(base.join("empty-a"), b"").unwrap();
        std::fs::write(base.join("empty-b"), b"").unwrap();
        let cancel = AtomicBool::new(false);
        let groups = find_duplicates(&base, &cancel, |_, _, _| {}).unwrap().groups;
        assert_eq!(groups.len(), 2);
        assert!(groups.iter().any(|group| group.files.len() == 2 && group.files[0].size == 0));
        assert!(groups.iter().any(|group| group.files.len() == 2 && group.files[0].size == 10));
        let _ = std::fs::remove_dir_all(base);
    }
    #[test] fn duplicate_scan_can_be_cancelled() {
        let base = std::env::temp_dir().join(format!("moa-hash-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("one.bin"), b"same").unwrap(); std::fs::write(base.join("two.bin"), b"same").unwrap();
        let cancel = AtomicBool::new(false);
        let result = find_duplicates(&base, &cancel, |_, _, _| cancel.store(true, Ordering::SeqCst));
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(base);
    }
    #[test] fn operation_plan_is_read_only_and_rejects_collision_and_escape() {
        let base=std::env::temp_dir().join(format!("moa-plan-test-{}",std::process::id())); let root=base.join("root"); let outside=base.join("outside");
        std::fs::create_dir_all(&root).unwrap(); std::fs::create_dir_all(&outside).unwrap(); let root=root.canonical().unwrap(); let outside=outside.canonical().unwrap();
        let source=root.join("before.txt"); std::fs::write(&source,b"fixture").unwrap();
        let target=root.join("after.txt");
        let plan=validate_operation(&root,OperationRequest{source:source.to_string_lossy().into_owned(),target:target.to_string_lossy().into_owned()}).unwrap();
        assert_eq!(plan.status,"pending"); assert!(source.exists()); assert!(!target.exists());
        std::fs::write(&target,b"do not overwrite").unwrap();
        assert!(validate_operation(&root,OperationRequest{source:source.to_string_lossy().into_owned(),target:target.to_string_lossy().into_owned()}).is_err());
        assert!(validate_operation(&root,OperationRequest{source:source.to_string_lossy().into_owned(),target:outside.join("escape.txt").to_string_lossy().into_owned()}).is_err());
        let _=std::fs::remove_dir_all(base);
    }
    #[test] fn approved_plan_moves_and_undo_restores_without_overwrite() {
        let base=std::env::temp_dir().join(format!("moa-undo-test-{}",std::process::id())); let root=base.join("root"); let folder=root.join("sub");
        std::fs::create_dir_all(&folder).unwrap(); let root=root.canonical().unwrap(); let folder=root.join("sub"); let source=root.join("before.txt"); let target=folder.join("after.txt"); std::fs::write(&source,b"fixture").unwrap();
        let mut plan=validate_operation(&root,OperationRequest{source:source.to_string_lossy().into_owned(),target:target.to_string_lossy().into_owned()}).unwrap();
        apply_plan_item(&root,&mut plan); assert_eq!(plan.status,"succeeded"); assert!(!source.exists()); assert_eq!(std::fs::read(&target).unwrap(),b"fixture");
        restore_plan_item(&root,&mut plan); assert_eq!(plan.status,"undone"); assert!(source.exists()); assert!(!target.exists());
        let _=std::fs::remove_dir_all(base);
    }
    #[test] fn approved_plan_creates_missing_category_directory_safely() {
        let base=std::env::temp_dir().join(format!("moa-category-plan-{}",std::process::id())); let root=base.join("root"); std::fs::create_dir_all(&root).unwrap(); let root=root.canonical().unwrap();
        let source=root.join("report.pdf"); let target=root.join("문서").join("report.pdf"); std::fs::write(&source,b"fixture").unwrap();
        let mut plan=validate_operation(&root,OperationRequest{source:source.to_string_lossy().into_owned(),target:target.to_string_lossy().into_owned()}).unwrap();
        assert!(!target.parent().unwrap().exists()); apply_plan_item(&root,&mut plan);
        assert_eq!(plan.status,"succeeded"); assert!(!source.exists()); assert_eq!(std::fs::read(&target).unwrap(),b"fixture");
        restore_plan_item(&root,&mut plan); assert_eq!(plan.status,"undone"); assert!(source.exists());
        let _=std::fs::remove_dir_all(base);
    }
    #[test] fn stale_plan_and_undo_collision_leave_files_safe() {
        let base=std::env::temp_dir().join(format!("moa-stale-test-{}",std::process::id())); let root=base.join("root"); std::fs::create_dir_all(&root).unwrap(); let root=root.canonical().unwrap();
        let source=root.join("before.txt"); let target=root.join("after.txt"); std::fs::write(&source,b"old").unwrap();
        let mut stale=validate_operation(&root,OperationRequest{source:source.to_string_lossy().into_owned(),target:target.to_string_lossy().into_owned()}).unwrap();
        std::fs::write(&source,b"changed size").unwrap(); apply_plan_item(&root,&mut stale); assert_eq!(stale.status,"failed"); assert!(source.exists()); assert!(!target.exists());
        std::fs::write(&source,b"old").unwrap(); let mut plan=validate_operation(&root,OperationRequest{source:source.to_string_lossy().into_owned(),target:target.to_string_lossy().into_owned()}).unwrap(); apply_plan_item(&root,&mut plan);
        std::fs::write(&source,b"user file").unwrap(); restore_plan_item(&root,&mut plan); assert_eq!(plan.status,"failed"); assert_eq!(std::fs::read(&source).unwrap(),b"user file"); assert!(target.exists());
        let _=std::fs::remove_dir_all(base);
    }
    #[test] fn undo_uses_job_root_even_after_another_folder_is_selected() {
        let base=std::env::temp_dir().join(format!("moa-undo-root-{}",std::process::id())); let root=base.join("root"); let other=base.join("other");
        std::fs::create_dir_all(&root).unwrap(); std::fs::create_dir_all(&other).unwrap(); let root=root.canonical().unwrap(); let other=other.canonical().unwrap();
        let source=root.join("before.txt"); let target=root.join("after.txt"); std::fs::write(&source,b"fixture").unwrap();
        let db=Connection::open_in_memory().unwrap(); ensure_history_schema(&db).unwrap();
        let item=validate_operation(&root,OperationRequest{source:source.to_string_lossy().into_owned(),target:target.to_string_lossy().into_owned()}).unwrap();
        let mut plan=Plan{id:"job-1".into(),items:vec![item],status:"pending".into(),created:1,root:root.to_string_lossy().into_owned()};
        record_plan(&db,&plan).unwrap(); run_plan(&db,&mut plan);
        assert_eq!(plan.status,"succeeded"); assert!(target.exists());
        let undone=undo_job(&db,"job-1",Some(other)).unwrap();
        assert_eq!(undone.status,"undone"); assert!(source.exists()); assert!(!target.exists());
        let _=std::fs::remove_dir_all(base);
    }
    #[test] fn legacy_job_without_root_needs_a_selected_folder() {
        let db=Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE jobs(id TEXT PRIMARY KEY, created INTEGER NOT NULL, status TEXT NOT NULL); INSERT INTO jobs VALUES ('old',1,'succeeded');").unwrap();
        ensure_history_schema(&db).unwrap();
        assert!(undo_job(&db,"old",None).is_err());
    }
    #[test] fn plan_keeps_running_and_reports_when_history_cannot_be_saved() {
        let base=std::env::temp_dir().join(format!("moa-history-fail-{}",std::process::id())); let root=base.join("root"); std::fs::create_dir_all(&root).unwrap(); let root=root.canonical().unwrap();
        let first=root.join("a.txt"); let second=root.join("b.txt"); std::fs::write(&first,b"a").unwrap(); std::fs::write(&second,b"b").unwrap();
        let items=[&first,&second].iter().map(|path| validate_operation(&root,OperationRequest{source:path.to_string_lossy().into_owned(),target:root.join(format!("moved-{}",path.file_name().unwrap().to_string_lossy())).to_string_lossy().into_owned()}).unwrap()).collect();
        let mut plan=Plan{id:"job-2".into(),items,status:"running".into(),created:1,root:root.to_string_lossy().into_owned()};
        let db=Connection::open_in_memory().unwrap(); // no history tables: every write fails
        run_plan(&db,&mut plan);
        assert_eq!(plan.status,"succeeded"); assert!(plan.items.iter().all(|item| item.error.as_deref()==Some(HISTORY_WARNING)));
        assert!(root.join("moved-a.txt").exists() && root.join("moved-b.txt").exists());
        let _=std::fs::remove_dir_all(base);
    }
    #[test] fn index_replace_swaps_rows_for_one_root_only() {
        let base=std::env::temp_dir().join(format!("moa-index-{}",std::process::id())); std::fs::create_dir_all(&base).unwrap(); let db_path=base.join("index.sqlite3");
        let entry=|path:&str| Entry{path:path.into(),name:"x".into(),extension:None,size:1,modified:None,category:"기타".into()};
        replace_index(&db_path,"/a",&[entry("/a/1"),entry("/a/2")]).unwrap(); replace_index(&db_path,"/b",&[entry("/b/1")]).unwrap();
        replace_index(&db_path,"/a",&[entry("/a/3")]).unwrap();
        let db=open_db(&db_path).unwrap();
        let paths:Vec<String>=db.prepare("SELECT path FROM files ORDER BY path").unwrap().query_map([],|r|r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        assert_eq!(paths,vec!["/a/3","/b/1"]);
        let _=std::fs::remove_dir_all(base);
    }
    fn hex(text: &str) -> Vec<u8> { (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i+2], 16).unwrap()).collect() }
    #[test] fn finder_tags_rank_by_color_index_in_any_language() {
        // plutil output for ["업무", "빨간색\n6", "Blue\n4"]
        let data = hex("62706c6973743030a301020362c5c5bb3465be68ac04c0c9000a003656426c75650a34080c111c0000000000000101000000000000000400000000000000000000000000000023");
        let tags = parse_tag_plist(&data).unwrap();
        assert_eq!(tags, vec!["업무".to_string(), "빨간색\n6".into(), "Blue\n4".into()]);
        assert_eq!(tags.iter().map(|tag| tag_color_rank(tag)).min(), Some(0));
        assert_eq!(tag_color_rank("Blueprint"), NO_COLOR_RANK);
        assert_eq!(tag_color_rank("회색\n1"), 6);
        assert!(parse_tag_plist(b"not a plist").is_none());
        assert!(parse_tag_plist(&data[..data.len() - 1]).is_none());
    }
    #[test] fn case_only_rename_is_allowed_for_the_same_file() {
        let base=std::env::temp_dir().join(format!("moa-case-{}",std::process::id())); let root=base.join("root"); std::fs::create_dir_all(&root).unwrap(); let root=root.canonical().unwrap();
        let source=root.join("report.txt"); let target=root.join("Report.txt"); std::fs::write(&source,b"fixture").unwrap();
        let case_insensitive = target.exists();
        let mut plan=validate_operation(&root,OperationRequest{source:source.to_string_lossy().into_owned(),target:target.to_string_lossy().into_owned()}).unwrap();
        apply_plan_item(&root,&mut plan); assert_eq!(plan.status,"succeeded",  "{:?}", plan.error);
        let names: Vec<_> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["Report.txt"]);
        restore_plan_item(&root,&mut plan); assert_eq!(plan.status,"undone", "{:?}", plan.error);
        let names: Vec<_> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["report.txt"]);
        // A different file that only differs by case is still protected on case-sensitive volumes.
        if !case_insensitive { std::fs::write(&target,b"other").unwrap(); assert!(validate_operation(&root,OperationRequest{source:source.to_string_lossy().into_owned(),target:target.to_string_lossy().into_owned()}).is_err()); }
        let _=std::fs::remove_dir_all(base);
    }
    #[test] fn region_layout_stays_inside_region_and_rejects_tiny_regions() {
        let display = DisplayArea { left: 0.0, top: 25.0, width: 1440.0, height: 875.0 };
        let region = ScreenRegion { x: 0.5, y: 0.1, width: 0.4, height: 0.8 };
        let entries: Vec<DesktopEntry> = (0..20).map(|i| DesktopEntry { path: PathBuf::from(format!("/d/{i}")), name: i.to_string(), color: 7 }).collect();
        let mut positions = Vec::new();
        append_region_positions(&[entries.clone()], "파일", &region, display, &[], None, IconAlign::Center, &mut positions).unwrap();
        assert_eq!(positions.len(), 20);
        let (left, top) = (display.left + region.x * display.width, display.top + region.y * display.height);
        let (right, bottom) = (left + region.width * display.width, top + region.height * display.height);
        assert!(positions.iter().all(|(_, x, y)| (*x as f64) >= left && (*x as f64) <= right && (*y as f64) >= top && (*y as f64) <= bottom));
        let tiny = ScreenRegion { x: 0.0, y: 0.0, width: 0.12, height: 0.12 };
        let many: Vec<DesktopEntry> = (0..200).map(|i| DesktopEntry { path: PathBuf::from(format!("/d/{i}")), name: i.to_string(), color: 7 }).collect();
        assert!(append_region_positions(&[many.clone()], "파일", &tiny, display, &[], None, IconAlign::Center, &mut Vec::new()).unwrap_err().contains("너무 좁습니다"));
        let crowded = ScreenRegion { x: 0.0, y: 0.0, width: 0.2, height: 0.2 };
        assert!(append_region_positions(&[many], "파일", &crowded, display, &[], None, IconAlign::Center, &mut Vec::new()).unwrap_err().contains("권장 최소"));
    }
    #[test] fn applescript_strings_are_escaped() {
        assert_eq!(applescript_string("a\"b\\c\nd"), "\"a\\\"b\\\\c\\nd\"");
    }
    #[test] fn folder_names_reject_path_tricks() {
        assert!(valid_folder_name("모은 파일"));
        for bad in ["", ".hidden", "a/b", "a\\b", "a:b", "a\nb", &"가".repeat(100)] { assert!(!valid_folder_name(bad), "{bad:?}"); }
    }
    #[test] fn region_layout_leaves_widget_spots_empty() {
        let display = DisplayArea { left: 0.0, top: 39.0, width: 1800.0, height: 1060.0 };
        let region = ScreenRegion { x: 0.0, y: 0.0, width: 0.5, height: 1.0 };
        let widgets = [SceneRect { x: 8.0, y: 47.0, width: 540.0, height: 180.0 }, SceneRect { x: 8.0, y: 227.0, width: 360.0, height: 360.0 }];
        let folders: Vec<DesktopEntry> = (0..12).map(|i| DesktopEntry { path: PathBuf::from(format!("/d/folder{i}")), name: i.to_string(), color: 7 }).collect();
        let files: Vec<DesktopEntry> = (0..30).map(|i| DesktopEntry { path: PathBuf::from(format!("/d/file{i}")), name: i.to_string(), color: 7 }).collect();
        let mut positions = Vec::new();
        append_region_positions(&[folders, files], "폴더·파일", &region, display, &widgets, None, IconAlign::Center, &mut positions).unwrap();
        assert_eq!(positions.len(), 42);
        assert!(positions.iter().all(|(_, x, y)| !icon_hits_obstacle(*x as f64, *y as f64, &widgets)));
        let unique: std::collections::HashSet<_> = positions.iter().map(|(_, x, y)| (*x, *y)).collect();
        assert_eq!(unique.len(), 42);
        // Folders and files never share a row.
        let folder_rows: std::collections::HashSet<_> = positions[..12].iter().map(|p| p.2).collect();
        assert!(positions[12..].iter().all(|p| !folder_rows.contains(&p.2)));
        // A region almost entirely covered by a widget reports the shortage instead of overlapping it.
        let small = ScreenRegion { x: 0.0, y: 0.0, width: 0.3, height: 0.5 };
        let cover = [SceneRect { x: 0.0, y: 39.0, width: 540.0, height: 530.0 }];
        let many: Vec<DesktopEntry> = (0..5).map(|i| DesktopEntry { path: PathBuf::from(format!("/d/{i}")), name: i.to_string(), color: 7 }).collect();
        let error = append_region_positions(&[many], "앱 아이콘", &small, display, &cover, None, IconAlign::Center, &mut Vec::new()).unwrap_err();
        assert!(error.contains("위젯이 있는 자리는 비워 둡니다"));
    }
    #[test] fn fixed_spacing_places_icons_exactly_that_far_apart() {
        let display = DisplayArea { left: 0.0, top: 39.0, width: 1800.0, height: 1060.0 };
        let region = ScreenRegion { x: 0.6, y: 0.0, width: 0.4, height: 1.0 };
        let apps: Vec<DesktopEntry> = (0..11).map(|i| DesktopEntry { path: PathBuf::from(format!("/d/app{i}")), name: i.to_string(), color: 7 }).collect();
        let spacing = IconSpacing { x: 130.0, y: 140.0 };
        let mut positions = Vec::new();
        append_region_positions(&[apps.clone()], "앱 아이콘", &region, display, &[], Some(spacing), IconAlign::Center, &mut positions).unwrap();
        assert_eq!(positions.len(), 11);
        let mut xs: Vec<i32> = positions.iter().map(|p| p.1).collect(); xs.sort(); xs.dedup();
        let mut ys: Vec<i32> = positions.iter().map(|p| p.2).collect(); ys.sort(); ys.dedup();
        assert!(xs.windows(2).all(|pair| (pair[1] - pair[0] - 130).abs() <= 1), "{xs:?}");
        assert!(ys.windows(2).all(|pair| (pair[1] - pair[0] - 140).abs() <= 1), "{ys:?}");
        assert_eq!(ys[0], (39.0 + 32.0) as i32); // starts at the top of the region
        // Wider spacing in a short region reports a shortage with the spacing hint.
        let short = ScreenRegion { x: 0.6, y: 0.0, width: 0.2, height: 0.2 };
        let error = append_region_positions(&[apps], "앱 아이콘", &short, display, &[], Some(IconSpacing { x: 200.0, y: 200.0 }), IconAlign::Center, &mut Vec::new()).unwrap_err();
        assert!(error.contains("권장 최소") && error.contains("아이콘 간격"));
        assert!(!valid_spacing(&IconSpacing { x: 20.0, y: 100.0 }) && valid_spacing(&spacing));
    }
    #[test] fn alignment_moves_icons_to_the_chosen_side() {
        let display = DisplayArea { left: 0.0, top: 39.0, width: 1800.0, height: 1060.0 };
        let region = ScreenRegion { x: 0.5, y: 0.0, width: 0.5, height: 0.5 };
        let (left_edge, right_edge) = (900.0, 1800.0);
        let apps: Vec<DesktopEntry> = (0..3).map(|i| DesktopEntry { path: PathBuf::from(format!("/d/app{i}")), name: i.to_string(), color: 7 }).collect();
        for spacing in [None, Some(IconSpacing { x: 112.0, y: 114.0 })] {
            let place = |align| { let mut positions = Vec::new(); append_region_positions(&[apps.clone()], "앱 아이콘", &region, display, &[], spacing, align, &mut positions).unwrap(); positions.iter().map(|p| p.1 as f64).collect::<Vec<_>>() };
            let (left, center, right) = (place(IconAlign::Left), place(IconAlign::Center), place(IconAlign::Right));
            let gap_left = |xs: &Vec<f64>| xs.iter().cloned().fold(f64::MAX, f64::min) - left_edge;
            let gap_right = |xs: &Vec<f64>| right_edge - xs.iter().cloned().fold(f64::MIN, f64::max);
            assert!(gap_left(&left) < gap_left(&center) && gap_left(&center) < gap_left(&right), "{spacing:?} {left:?} {center:?} {right:?}");
            assert!(gap_right(&right) < gap_right(&center) && gap_right(&center) < gap_right(&left));
            assert!((gap_left(&center) - gap_right(&center)).abs() <= 2.0, "center {center:?}");
            // Icons (half width 46pt) never stick out of the region.
            assert!(gap_left(&left) >= 46.0 - 0.5 && gap_right(&right) >= 46.0 - 0.5);
        }
        // Widget-avoiding path respects alignment too.
        let widget = [SceneRect { x: 1700.0, y: 39.0, width: 100.0, height: 100.0 }];
        let mut positions = Vec::new();
        append_region_positions(&[apps.clone()], "앱 아이콘", &region, display, &widget, None, IconAlign::Right, &mut positions).unwrap();
        assert!(positions.iter().all(|(_, x, y)| !icon_hits_obstacle(*x as f64, *y as f64, &widget)));
    }
    #[test] fn region_styles_deserialize_with_defaults() {
        let style: RegionStyle = serde_json::from_str(r#"{"spacing":{"x":96,"y":100},"align":"right"}"#).unwrap();
        assert_eq!(style.align, IconAlign::Right); assert_eq!(style.spacing.map(|s| s.x), Some(96.0));
        let auto: RegionStyle = serde_json::from_str(r#"{"spacing":null}"#).unwrap();
        assert!(auto.spacing.is_none()); assert_eq!(auto.align, IconAlign::Center);
    }
    #[test] fn verified_copy_matches_source_and_refuses_overwrite() {
        let base = std::env::temp_dir().join(format!("moa-copy-{}", std::process::id())); let _ = std::fs::remove_dir_all(&base); std::fs::create_dir_all(&base).unwrap();
        let source = base.join("a.bin"); let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect(); std::fs::write(&source, &data).unwrap();
        let target = base.join("b.bin");
        copy_verified(&source, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), data);
        assert!(copy_verified(&source, &target).is_err()); // never overwrites
        assert_eq!(std::fs::read(&target).unwrap(), data);
        let link = base.join("link"); #[cfg(unix)] std::os::unix::fs::symlink(&source, &link).unwrap();
        assert!(copy_verified(&link, &base.join("c.bin")).is_err());
        let _ = std::fs::remove_dir_all(base);
    }
    // cargo test --release -- --ignored --nocapture copy_speed  (BENCH_DIR must hold big.bin and small/)
    #[test] #[ignore] fn copy_speed() {
        let dir = PathBuf::from(std::env::var("BENCH_DIR").unwrap());
        let out = dir.join("out"); let _ = std::fs::remove_dir_all(&out); std::fs::create_dir_all(&out).unwrap();
        let old = |source: &Path, target: &Path| {
            let (expected, before) = hash_stable(source).unwrap();
            let mut input = File::open(source).unwrap();
            let mut output = std::fs::OpenOptions::new().write(true).create_new(true).open(target).unwrap();
            std::io::copy(&mut input, &mut output).unwrap(); output.sync_all().unwrap(); drop(output);
            let (a, meta) = hash_stable(source).unwrap(); let (b, _) = hash_stable(target).unwrap();
            assert!(a == expected && b == expected && metadata_key(&meta) == metadata_key(&before));
        };
        let smalls: Vec<PathBuf> = std::fs::read_dir(dir.join("small")).unwrap().map(|e| e.unwrap().path()).collect();
        for (label, run_new) in [("old", false), ("new", true)] {
            let start = std::time::Instant::now();
            let target = out.join(format!("{label}-big.bin"));
            if run_new { copy_verified(&dir.join("big.bin"), &target).unwrap(); } else { old(&dir.join("big.bin"), &target); }
            let big = start.elapsed();
            if run_new {
                let start = std::time::Instant::now(); let source = dir.join("big.bin");
                copy_streaming_verified(&source, &out.join("stream-big.bin"), &std::fs::metadata(&source).unwrap()).unwrap();
                println!("new (other volume path): 1GB file {:.2}s", start.elapsed().as_secs_f64());
            }
            let start = std::time::Instant::now();
            for (i, path) in smalls.iter().enumerate() { let t = out.join(format!("{label}-s{i}")); if run_new { copy_verified(path, &t).unwrap(); } else { old(path, &t); } }
            println!("{label}: 1GB file {:.2}s, {} small files {:.2}s", big.as_secs_f64(), smalls.len(), start.elapsed().as_secs_f64());
        }
        let _ = std::fs::remove_dir_all(out);
    }
    // cargo test --release -- --ignored --nocapture scene_timing
    #[cfg(target_os = "macos")] #[test] #[ignore] fn scene_timing() {
        let t = std::time::Instant::now(); let windows = desktop_windows::list(); println!("list: {:?} ({} windows)", t.elapsed(), windows.len());
        let t = std::time::Instant::now(); let settled = desktop_windows::list_settled(); println!("list_settled: {:?}", t.elapsed());
        let icon_level = desktop_windows::desktop_icon_level(); let desktop_level = desktop_windows::desktop_level();
        let screen = SceneRect { x: 0.0, y: 0.0, width: 1800.0, height: 1169.0 };
        let wallpaper = settled.iter().filter(|w| matches!(w.owner, desktop_windows::Owner::WindowManager | desktop_windows::Owner::WallpaperAgent) && w.layer <= desktop_level).max_by_key(|w| w.onscreen).map(|w| w.id);
        let icons = settled.iter().find(|w| w.owner == desktop_windows::Owner::Finder && w.layer == icon_level && w.bounds.x == 0.0 && w.bounds.y == 0.0).map(|w| w.id);
        for (label, id, size, jpeg) in [("wallpaper", wallpaper, 1800, true), ("icons", icons, 1800, false)] {
            let t = std::time::Instant::now(); let image = id.and_then(|id| capture_window_bytes(id, size, jpeg)); println!("capture {label}: {:?} ({} KB, visible={:?})", t.elapsed(), image.as_ref().map(|i| i.len() / 1024).unwrap_or(0), image.as_ref().map(|i| image_has_visible_pixels(i)));
        }
        for w in desktop_windows::widgets(&settled, &screen) { let t = std::time::Instant::now(); let image = capture_window_image(w.id, 640, false); println!("capture widget {}: {:?} ({} KB)", w.id, t.elapsed(), image.map(|i| i.len() / 1024).unwrap_or(0)); }
        let ids: Vec<(u32, u32, bool)> = wallpaper.map(|id| (id, 1800, true)).into_iter().chain(icons.map(|id| (id, 1800, false))).chain(desktop_windows::widgets(&settled, &screen).iter().map(|w| (w.id, 640, false))).collect();
        let t = std::time::Instant::now();
        std::thread::scope(|scope| { for (id, size, jpeg) in ids { scope.spawn(move || capture_window_bytes(id, size, jpeg)); } });
        println!("all layers in parallel: {:?}", t.elapsed());
        let t = std::time::Instant::now(); let _ = desktop_windows::list_settled(); println!("list_settled again (cached): {:?}", t.elapsed());
        if let Ok(blank) = std::env::var("BLANK_PNG") { println!("blank png visible={}", image_has_visible_pixels(&std::fs::read(blank).unwrap())); }
        let desktop = std::path::PathBuf::from(std::env::var("HOME").unwrap()).join("Desktop");
        let t = std::time::Instant::now(); let mut n = 0; for entry in std::fs::read_dir(&desktop).unwrap().flatten() { let path = entry.path(); let _ = finder_color_rank(&path); let _ = resolve_finder_alias(&path); n += 1; } println!("desktop items {n}: tags+aliases {:?}", t.elapsed());
    }
    #[test] fn picked_files_move_without_overwrite_and_undo_from_history() {
        let base = std::env::temp_dir().join(format!("moa-move-{}", std::process::id())); let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("from")).unwrap(); std::fs::create_dir_all(base.join("to")).unwrap();
        let base = base.canonical().unwrap(); let (from, to) = (base.join("from"), base.join("to"));
        let source = from.join("photo.jpg"); std::fs::write(&source, b"pixels").unwrap();
        let target = to.join("여행 1.jpg");
        let moved = move_file(&source, &target).unwrap();
        assert!(!source.exists()); assert_eq!(std::fs::read(&target).unwrap(), b"pixels");
        // never replaces an existing file
        let other = from.join("other.txt"); std::fs::write(&other, b"keep").unwrap();
        assert!(move_file(&other, &target).is_err()); assert_eq!(std::fs::read(&target).unwrap(), b"pixels"); assert!(other.exists());
        // recorded like the app does, then undone through the regular history path
        let (size, modified_ns) = metadata_key(&moved);
        let db = Connection::open_in_memory().unwrap(); ensure_history_schema(&db).unwrap();
        let job = Plan { id: "move-1".into(), status: "succeeded".into(), created: 1, root: "/".into(), items: vec![PlanItem { source: source.to_string_lossy().into_owned(), target: target.to_string_lossy().into_owned(), action: "move".into(), size, modified_ns, sha256: String::new(), status: "succeeded".into(), error: None }] };
        record_finished_job(&db, &job).unwrap();
        let undone = undo_job(&db, "move-1", None).unwrap();
        assert_eq!(undone.status, "undone"); assert_eq!(std::fs::read(&source).unwrap(), b"pixels"); assert!(!target.exists());
        let _ = std::fs::remove_dir_all(base);
    }
    #[test] fn move_destination_deserializes() {
        assert!(matches!(serde_json::from_str::<MoveDestination>(r#"{"kind":"new","name":"모은 파일"}"#).unwrap(), MoveDestination::New { .. }));
        assert!(matches!(serde_json::from_str::<MoveDestination>(r#"{"kind":"existing","path":"/Users/me/문서"}"#).unwrap(), MoveDestination::Existing { .. }));
    }
    #[test] fn edge_flush_region_expanded_like_the_app_fits_49_items() {
        // The user's setup: work area 1800×1130 below the menu bar, folder/file range flush with the right edge.
        let display = DisplayArea { left: 0.0, top: 39.0, width: 1800.0, height: 1130.0 };
        let spacing = Some(IconSpacing { x: 84.0, y: 88.0 });
        let entry = |name: String| DesktopEntry { path: PathBuf::from(format!("/d/{name}")), name, color: 7 };
        let folders: Vec<DesktopEntry> = (0..10).map(|i| entry(format!("folder{i}"))).collect();
        let files: Vec<DesktopEntry> = (0..39).map(|i| entry(format!("file{i}"))).collect();
        let saved = ScreenRegion { x: 0.8025332629090269, y: 0.0, width: 0.19746673709097312, height: 0.4022220441524099 };
        let error = append_region_positions(&[folders.clone(), files.clone()], "폴더·파일", &saved, display, &[], spacing, IconAlign::Right, &mut Vec::new()).unwrap_err();
        assert!(error.contains("권장 최소 355×1120pt"), "{error}");
        // Same arithmetic as the app's auto-expansion.
        let width = (355.0_f64 / 1800.0 * 1.04).max(saved.width).min(1.0);
        let height = (1120.0_f64 / 1130.0 * 1.04).max(saved.height).min(1.0);
        let expanded = ScreenRegion { x: saved.x.min(1.0 - width).max(0.0), y: 0.0, width, height };
        assert!(valid_region(&expanded));
        let mut positions = Vec::new();
        append_region_positions(&[folders, files], "폴더·파일", &expanded, display, &[], spacing, IconAlign::Right, &mut positions).unwrap();
        assert_eq!(positions.len(), 49);
        assert!(positions.iter().all(|(_, x, y)| *x as f64 + 46.0 <= 1800.5 && (*y as f64) < 39.0 + 1130.0));
    }
}
