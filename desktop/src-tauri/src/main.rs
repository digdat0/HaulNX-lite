// Prevents an extra console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use base64::Engine;
use serde::Serialize;
use serde_json::json;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::{Emitter, Manager};

// Optional local-only extras (native ROM post-download conversion). build.rs
// emits the `has_ext_ops` cfg only when src/ext_ops.rs is present, so a public
// checkout without that file compiles without it and registers no extra command.
#[cfg(has_ext_ops)]
mod ext_ops;

// Nintendo Co., Ltd. USB vendor id. A Switch only enumerates as a USB *device*
// while running homebrew that opens usb:ds (HaulNX's MTP responder does), so a
// device on this vendor id is a strong "Switch plugged in over USB" signal.
const NINTENDO_VID: u16 = 0x057E;

// The device's always-on read-only inventory server port (HTTPSRV_INV_PORT on
// the console). Discovery probes this because it is up whenever the inventory
// server toggle is on, unlike the transfer server (8080) which opens per-session.
const INV_PORT: u16 = 8081;

// USB counterpart to Wi-Fi's X-Fs-Extract header (wifi_push's fs_extract
// param): MTP has no header mechanism, so a zip uploaded under this exact
// name anywhere in the SD Card object tells the device to unpack it into its
// parent folder instead of leaving it as a file -- see kBulkUploadMarker in
// mtp/responder.cpp, which this string must match exactly.
const BULK_UPLOAD_MARKER: &str = "__haulnx_bulk__.zip";

// USB counterpart to Wi-Fi's X-Art-Target header (wifi_push's art_target
// param): a file uploaded to the storage root named
// "<ART_PUSH_PREFIX><target>.png" tells the device to file it as that
// console's cover art instead of leaving it as a plain file -- see
// ParseArtPushName in mtp/responder.cpp, which this string must match
// exactly.
const ART_PUSH_PREFIX: &str = "__haulnx_art_";

/// Minimal bridge probe so the frontend can confirm it is running inside the
/// native shell (and pick up host/version info).
#[tauri::command]
fn app_info() -> serde_json::Value {
    // Cargo.toml's version must stay valid semver (Cargo itself enforces
    // this), so the "l" suffix requested for Lite builds can't live there --
    // append it only to what's DISPLAYED (index.html's #appVer). This is
    // independent of self_update_check, which reads CARGO_PKG_VERSION
    // directly and is unaffected -- it still compares the real "2.2.30"
    // against GitHub release tags (also bare semver).
    let version = if cfg!(feature = "lite") {
        format!("{}l", env!("CARGO_PKG_VERSION"))
    } else {
        env!("CARGO_PKG_VERSION").to_string()
    };
    json!({
        "shell": "tauri",
        "name": "HaulNX App Utility",
        "version": version,
        "platform": std::env::consts::OS,
        // Lite builds (see the `lite` feature in Cargo.toml) have no
        // ROM-acquisition/downloader feature; index.html hides the
        // Archive Collections / Downloads tabs and archive.org credentials
        // when this is true.
        "lite": cfg!(feature = "lite"),
    })
}

#[derive(Serialize)]
struct UsbDevice {
    vendor_id: u16,
    product_id: u16,
    product: Option<String>,
    serial: Option<String>,
}

/// Connected USB devices on Nintendo's vendor id — i.e. Switches currently
/// plugged in and exposing a USB interface. Empty when nothing is attached.
/// Enumeration is quick but run off the UI thread to keep the window smooth.
#[tauri::command]
async fn usb_switches() -> Vec<UsbDevice> {
    tauri::async_runtime::spawn_blocking(|| match nusb::list_devices() {
        Ok(devs) => devs
            .filter(|d| d.vendor_id() == NINTENDO_VID)
            .map(|d| UsbDevice {
                vendor_id: d.vendor_id(),
                product_id: d.product_id(),
                product: d.product_string().map(str::to_string),
                serial: d.serial_number().map(str::to_string),
            })
            .collect(),
        Err(_) => Vec::new(),
    })
    .await
    .unwrap_or_default()
}

#[derive(Serialize)]
struct NetProbe {
    reachable: bool,
    ms: u64,
}

/// Raw TCP reachability check for the Switch's inventory server over Wi-Fi,
/// free of the browser's CORS/opaque-response limits. Lets the UI show a live
/// "Switch on the network" state before (and independent of) the one-time code.
/// Runs off the UI thread so the connect timeout never stalls the window.
#[tauri::command]
async fn net_probe(host: String, port: u16) -> NetProbe {
    tauri::async_runtime::spawn_blocking(move || {
        let start = Instant::now();
        let reachable = (host.as_str(), port)
            .to_socket_addrs()
            .ok()
            .and_then(|mut it| it.next())
            .map(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(600)).is_ok())
            .unwrap_or(false);
        NetProbe {
            reachable,
            ms: start.elapsed().as_millis() as u64,
        }
    })
    .await
    .unwrap_or(NetProbe { reachable: false, ms: 0 })
}

#[derive(Clone, Serialize)]
struct DlProgress {
    id: String,
    received: u64,
    total: u64,
    done: bool,
    path: Option<String>,
    error: Option<String>,
}

/// Keep the saved file to a bare, safe basename inside the chosen folder — the
/// archive.org file "name" can carry subfolders ("apfix/rom.zip") or, in theory,
/// path tricks; we never want the download to escape the folder the user picked.
fn safe_name(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim()
        .trim_matches('.');
    let cleaned: String = base
        .chars()
        .map(|c| if "<>:\"|?*".contains(c) || (c as u32) < 0x20 { '_' } else { c })
        .collect();
    if cleaned.is_empty() {
        "download.bin".to_string()
    } else {
        cleaned
    }
}

/// Sanitize one path segment (a console name) into a safe single folder name, so
/// the auto-created per-console subfolder can never escape the chosen base folder
/// or split across levels. Empty in → empty out (the file lands in the base).
fn safe_component(name: &str) -> String {
    name.trim()
        .trim_matches('.')
        .chars()
        .map(|c| if "<>:\"|?*/\\".contains(c) || (c as u32) < 0x20 { '_' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Set of download ids the user has cancelled. The streaming loop checks this
/// between chunks and bails out (deleting its `.part`) when its id shows up.
fn cancelled_downloads() -> &'static Mutex<HashSet<String>> {
    static C: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Mark an in-progress download for cancellation. The `download_file` task notices
/// on its next chunk, stops, and deletes the partial file. Sentinel string used
/// internally so the cancel path skips the error toast.
const CANCELLED_SENTINEL: &str = "__cancelled__";

#[tauri::command]
fn cancel_download(id: String) {
    if let Ok(mut set) = cancelled_downloads().lock() {
        set.insert(id);
    }
}

/// Resolve a redirect `Location` against the URL it came from. archive.org sends
/// absolute URLs, but a relative one (root- or path-relative) would otherwise be
/// treated as a bare host and fail — so handle all three forms.
fn resolve_url(base: &str, loc: &str) -> String {
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return loc.to_string();
    }
    // scheme://host[:port] prefix of the base (everything up to the path).
    let scheme_end = base.find("://").map(|i| i + 3).unwrap_or(0);
    let host_end = base[scheme_end..]
        .find('/')
        .map(|i| scheme_end + i)
        .unwrap_or(base.len());
    let origin = &base[..host_end];
    if loc.starts_with('/') {
        format!("{origin}{loc}")
    } else {
        // Path-relative: resolve against the current path's directory.
        let dir_end = base.rfind('/').map(|i| i + 1).unwrap_or(host_end).max(host_end);
        format!("{}{}", &base[..dir_end], loc)
    }
}

/// Stream a URL to `dest_dir/filename`, bypassing the WebView's CORS/opaque
/// limits (rustls handles HTTPS with no OpenSSL DLL). Bytes land in a `.part`
/// file that is renamed into place only on success, so an aborted download never
/// leaves a truncated file looking complete. Progress is pushed to the frontend
/// as `dl://progress` events, throttled so a big ROM doesn't flood the bridge.
#[tauri::command]
async fn download_file(
    app: tauri::AppHandle,
    id: String,
    url: String,
    dest_dir: String,
    subdir: String,
    filename: String,
    access: Option<String>,
    secret: Option<String>,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let emit = |received: u64, total: u64, done: bool, path: Option<String>, error: Option<String>| {
            let _ = app.emit(
                "dl://progress",
                DlProgress { id: id.clone(), received, total, done, path, error },
            );
        };

        let run = || -> Result<String, String> {
            // Defense in depth for Lite builds (see the `lite` feature in
            // Cargo.toml): index.html never constructs an archive.org URL to
            // pass here (the Archive Collections / Downloads tabs are hidden
            // entirely), but refuse one anyway rather than trust the frontend.
            // Update downloads (GitHub release assets) are untouched.
            #[cfg(feature = "lite")]
            if url.contains(".archive.org/") || url.contains("//archive.org/") {
                return Err("Not available in this build.".to_string());
            }
            // The user picks a base folder once; each download drops into a
            // per-console subfolder (e.g. base/"Nintendo 64"), created on demand
            // so they never have to make the folders themselves. Each "/"-
            // separated piece is sanitized on its own rather than the whole
            // string at once, so a multi-level subdir -- the SD Card tab's
            // "download a folder" recursion walking its own subfolder tree --
            // creates real nested directories instead of being flattened into
            // one underscore-joined name; every other caller passes a single
            // component (no separator), so splitting it is a no-op for them.
            let mut dir = PathBuf::from(&dest_dir);
            for part in subdir.split(['/', '\\']) {
                let sub = safe_component(part);
                if !sub.is_empty() {
                    dir.push(&sub);
                }
            }
            std::fs::create_dir_all(&dir)
                .map_err(|e| format!("Couldn't create the download folder: {e}"))?;
            let final_path = dir.join(safe_name(&filename));
            let part_path = final_path.with_extension(format!(
                "{}part",
                final_path
                    .extension()
                    .map(|e| format!("{}.", e.to_string_lossy()))
                    .unwrap_or_default()
            ));

            // archive.org items can be access-restricted (per-file "private":true);
            // those need the S3 key as an `Authorization: LOW <access>:<secret>`
            // header. The /download/ URL 302-redirects to a data node on a
            // *different* archive.org host, and ureq (2.10+) strips auth headers
            // across a host change by default — so we follow redirects ourselves
            // and re-attach the key on every archive.org host (never leaking the
            // secret to a non-archive.org redirect target).
            let auth = match (&access, &secret) {
                (Some(a), Some(s)) if !a.is_empty() && !s.is_empty() => {
                    Some(format!("LOW {a}:{s}"))
                }
                _ => None,
            };
            // Use a connect timeout plus a per-read idle timeout rather than one
            // overall deadline: a big private ROM legitimately takes minutes to
            // stream, so an overall cap would abort it mid-download (surfacing as
            // os error 10060). The idle read timeout still fails a truly stalled
            // socket.
            let agent = ureq::builder()
                .timeout_connect(Duration::from_secs(30))
                .timeout_read(Duration::from_secs(120))
                .redirects(0)
                .build();
            let mut current = url.clone();
            let resp = {
                let mut hops = 0u8;
                loop {
                    let mut req = agent.get(&current);
                    if let Some(ref a) = auth {
                        if current.contains(".archive.org/") || current.contains("//archive.org/") {
                            req = req.set("Authorization", a);
                        }
                    }
                    match req.call() {
                        Ok(r) => {
                            let code = r.status();
                            if (300..400).contains(&code) {
                                let loc = r
                                    .header("location")
                                    .ok_or_else(|| format!("HTTP {code} with no redirect target."))?;
                                current = resolve_url(&current, loc);
                                hops += 1;
                                if hops > 8 {
                                    return Err("Too many redirects.".into());
                                }
                                continue;
                            }
                            break r;
                        }
                        Err(ureq::Error::Status(code, _)) => {
                            return Err(if code == 401 || code == 403 {
                                format!("HTTP {code} — this archive.org item is access-restricted; check your keys on the Credentials tab.")
                            } else {
                                format!("HTTP {code} from the server.")
                            });
                        }
                        Err(ureq::Error::Transport(t)) => {
                            return Err(format!("Network error: {t}"));
                        }
                    }
                }
            };
            let total: u64 = resp
                .header("Content-Length")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            let mut reader = resp.into_reader();
            let mut file = std::fs::File::create(&part_path)
                .map_err(|e| format!("Can't write to that folder: {e}"))?;
            let mut buf = vec![0u8; 128 * 1024];
            let mut received: u64 = 0;
            let mut last = Instant::now();
            emit(0, total, false, None, None);
            loop {
                // Bail out cleanly if the user hit ✕: drop the handle and remove
                // the half-written .part rather than leaving it behind.
                if cancelled_downloads().lock().map(|s| s.contains(&id)).unwrap_or(false) {
                    drop(file);
                    let _ = std::fs::remove_file(&part_path);
                    return Err(CANCELLED_SENTINEL.into());
                }
                let n = reader.read(&mut buf).map_err(|e| format!("Download interrupted: {e}"))?;
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n]).map_err(|e| format!("Disk write failed: {e}"))?;
                received += n as u64;
                if last.elapsed() >= Duration::from_millis(200) {
                    emit(received, total, false, None, None);
                    last = Instant::now();
                }
            }
            file.flush().ok();
            drop(file);
            // Replace any existing file at the destination with the fresh download.
            let _ = std::fs::remove_file(&final_path);
            std::fs::rename(&part_path, &final_path)
                .map_err(|e| format!("Couldn't finalize the file: {e}"))?;
            Ok(final_path.to_string_lossy().into_owned())
        };

        let result = run();
        // Finished or aborted — clear any cancel flag left for this id.
        if let Ok(mut set) = cancelled_downloads().lock() {
            set.remove(&id);
        }
        match result {
            Ok(path) => {
                emit(0, 0, true, Some(path.clone()), None);
                Ok(path)
            }
            // The user cancelled: its row is already gone, so stay silent (no toast).
            Err(e) if e == CANCELLED_SENTINEL => Err(e),
            Err(e) => {
                emit(0, 0, true, None, Some(e.clone()));
                Err(e)
            }
        }
    })
    .await
    .map_err(|_| "Download task failed to run.".to_string())?
}

/// Windows Portable Devices (WPD) access to the Switch's MTP object tree. The
/// device is owned by the OS MTP driver, so raw USB can't claim it — WPD drives
/// the same tree the responder exposes: read inventory.json / dl_sources.json,
/// download a game, and delete / rename / move within the managed folders.
///
/// In WPD/MTP the tree is DEVICE -> storage -> files, so files live one level
/// under the storage object, alongside the console folders. We resolve that
/// storage once on open and address everything by (folder name, file name).
#[cfg(target_os = "windows")]
mod wpd {
    use std::ffi::c_void;
    use std::path::Path;
    use windows::core::{PCWSTR, PROPVARIANT, PWSTR};
    use windows::Win32::Devices::PortableDevices::{
        IPortableDevice, IPortableDeviceContent, IPortableDeviceManager,
        IPortableDeviceProperties, IPortableDevicePropVariantCollection,
        IPortableDeviceResources, IPortableDeviceValues, PortableDevice, PortableDeviceManager,
        PortableDevicePropVariantCollection, PortableDeviceValues, WPD_CONTENT_TYPE_FOLDER,
        WPD_CONTENT_TYPE_GENERIC_FILE, WPD_OBJECT_CONTENT_TYPE, WPD_OBJECT_DATE_MODIFIED,
        WPD_OBJECT_FORMAT, WPD_OBJECT_FORMAT_UNSPECIFIED, WPD_OBJECT_NAME,
        WPD_OBJECT_ORIGINAL_FILE_NAME, WPD_OBJECT_PARENT_ID, WPD_OBJECT_SIZE, WPD_RESOURCE_DEFAULT,
    };
    use windows::Win32::Foundation::{S_FALSE, S_OK};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, IStream, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED, STGC_DEFAULT, STGM_READ,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    unsafe fn free_pwstr(p: PWSTR) {
        if !p.is_null() {
            CoTaskMemFree(Some(p.0 as *const c_void));
        }
    }
    // Child object-id strings of `parent` (COM-allocated PWSTRs freed as we go).
    unsafe fn children(content: &IPortableDeviceContent, parent: &str) -> Vec<String> {
        let mut ids = Vec::new();
        let w = wide(parent);
        if let Ok(en) = content.EnumObjects(0, PCWSTR(w.as_ptr()), None) {
            loop {
                let mut batch = [PWSTR::null(); 16];
                let mut fetched: u32 = 0;
                let _ = en.Next(&mut batch, &mut fetched);
                if fetched == 0 {
                    break;
                }
                for p in batch.iter().take(fetched as usize) {
                    if !p.is_null() {
                        ids.push(p.to_string().unwrap_or_default());
                        free_pwstr(*p);
                    }
                }
            }
        }
        ids
    }
    unsafe fn name_of(props: &IPortableDeviceProperties, id: &str) -> String {
        let w = wide(id);
        props
            .GetValues(PCWSTR(w.as_ptr()), None)
            .ok()
            .and_then(|v| {
                v.GetStringValue(&WPD_OBJECT_ORIGINAL_FILE_NAME)
                    .or_else(|_| v.GetStringValue(&WPD_OBJECT_NAME))
                    .ok()
            })
            .map(|p| {
                let s = p.to_string().unwrap_or_default();
                free_pwstr(p);
                s
            })
            .unwrap_or_default()
    }
    unsafe fn find_child(
        content: &IPortableDeviceContent,
        props: &IPortableDeviceProperties,
        parent: &str,
        name: &str,
    ) -> Option<String> {
        children(content, parent)
            .into_iter()
            .find(|id| name_of(props, id).eq_ignore_ascii_case(name))
    }

    /// An opened Switch: the WPD content/props/resources plus the storage
    /// container object that holds the console folders + inventory files.
    pub struct Device {
        _device: IPortableDevice,
        content: IPortableDeviceContent,
        props: IPortableDeviceProperties,
        resources: IPortableDeviceResources,
        root: String,
    }

    /// Find the Switch on WPD, open it, and resolve its storage container.
    pub fn open() -> Result<Device, String> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let manager: IPortableDeviceManager =
                CoCreateInstance(&PortableDeviceManager, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| format!("Windows Portable Devices unavailable: {e}"))?;
            let mut count: u32 = 0;
            manager
                .GetDevices(std::ptr::null_mut(), &mut count)
                .map_err(|e| format!("Couldn't list USB devices: {e}"))?;
            if count == 0 {
                return Err("No MTP device is connected — on the Switch, open Install from PC over USB.".into());
            }
            let mut ids: Vec<PWSTR> = vec![PWSTR::null(); count as usize];
            manager
                .GetDevices(ids.as_mut_ptr(), &mut count)
                .map_err(|e| format!("Couldn't list USB devices: {e}"))?;
            let mut devid: PWSTR = PWSTR::null();
            for &id in &ids {
                let s = id.to_string().unwrap_or_default().to_ascii_uppercase();
                if devid.is_null() && s.contains("VID_057E") && s.contains("PID_201D") {
                    devid = id;
                } else {
                    free_pwstr(id);
                }
            }
            if devid.is_null() {
                return Err("No Switch found over USB (open Install from PC \u{2192} USB on the Switch).".into());
            }

            let device: IPortableDevice =
                CoCreateInstance(&PortableDevice, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| format!("WPD device create failed: {e}"))?;
            let params: IPortableDeviceValues =
                CoCreateInstance(&PortableDeviceValues, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| format!("WPD values create failed: {e}"))?;
            let opened = device.Open(PCWSTR(devid.0), &params);
            free_pwstr(devid);
            opened.map_err(|e| format!("Couldn't open the Switch over USB: {e}"))?;

            let content: IPortableDeviceContent =
                device.Content().map_err(|e| format!("WPD content failed: {e}"))?;
            let props: IPortableDeviceProperties = content
                .Properties()
                .map_err(|e| format!("WPD properties failed: {e}"))?;
            let resources: IPortableDeviceResources = content
                .Transfer()
                .map_err(|e| format!("WPD transfer failed: {e}"))?;

            // Our responder exposes a single storage; its object is DEVICE's first
            // child and holds the console folders + inventory files.
            let root = children(&content, "DEVICE")
                .into_iter()
                .next()
                .ok_or_else(|| "The Switch exposed no storage over USB.".to_string())?;

            Ok(Device { _device: device, content, props, resources, root })
        }
    }

    impl Device {
        // A top-level object under the storage root (inventory.json,
        // dl_sources.json, a console folder, or the Inbox), by name.
        unsafe fn top(&self, name: &str) -> Option<String> {
            find_child(&self.content, &self.props, &self.root, name)
        }
        // A file inside a console folder / the Inbox, by folder + file name.
        unsafe fn file_in(&self, folder: &str, name: &str) -> Result<String, String> {
            let f = self
                .top(folder)
                .ok_or_else(|| format!("The {folder} folder isn't on the Switch."))?;
            find_child(&self.content, &self.props, &f, name)
                .ok_or_else(|| "That file isn't on the Switch anymore.".to_string())
        }
        // Resolve an arbitrary-depth path (component names, walked one child
        // lookup at a time from the storage root) to its object id -- the SD
        // Card tab's counterpart to `top`/`file_in`, which only ever handle
        // one or two fixed levels. `parts[0]` is a top-level object name (in
        // practice always "SD Card" -- see sd_fs_parts below), the rest are
        // ordinary subfolder/file names under it. Empty `parts` resolves to
        // the storage root itself.
        unsafe fn resolve(&self, parts: &[String]) -> Result<String, String> {
            let mut cur = self.root.clone();
            for part in parts {
                cur = find_child(&self.content, &self.props, &cur, part).ok_or_else(|| {
                    format!("\"{part}\" isn't on the Switch anymore.")
                })?;
            }
            Ok(cur)
        }
        // True if `id` is a folder (WPD_CONTENT_TYPE_FOLDER), for the SD Card
        // tab's directory listing -- every other object in this file is
        // either known in advance to be a folder (a console/Inbox top-level
        // entry) or a plain file, so nothing needed this distinction before.
        unsafe fn is_folder(&self, id: &str) -> bool {
            let w = wide(id);
            self.props
                .GetValues(PCWSTR(w.as_ptr()), None)
                .ok()
                .and_then(|v| v.GetGuidValue(&WPD_OBJECT_CONTENT_TYPE).ok())
                .map(|g| g == WPD_CONTENT_TYPE_FOLDER)
                .unwrap_or(false)
        }
        unsafe fn size_of(&self, id: &str) -> u64 {
            let w = wide(id);
            self.props
                .GetValues(PCWSTR(w.as_ptr()), None)
                .ok()
                .and_then(|v| v.GetUnsignedLargeIntegerValue(&WPD_OBJECT_SIZE).ok())
                .unwrap_or(0)
        }
        /// Unix epoch seconds for WPD_OBJECT_DATE_MODIFIED, or 0 if the driver
        /// doesn't report one. WPD exposes dates as a raw PROPVARIANT rather than
        /// through a typed getter (there's no GetDoubleValue/GetDateValue on
        /// IPortableDeviceValues), so this reads the union by hand: VT_DATE (an
        /// OLE Automation date -- days since 1899-12-30) is what Windows' MTP
        /// class driver normally reports here, with VT_FILETIME (100ns ticks
        /// since 1601-01-01) as a fallback for drivers that report it that way.
        unsafe fn date_modified_of(&self, id: &str) -> i64 {
            const VT_DATE: u16 = 7;
            const VT_FILETIME: u16 = 64;
            let w = wide(id);
            self.props
                .GetValues(PCWSTR(w.as_ptr()), None)
                .ok()
                .and_then(|v| v.GetValue(&WPD_OBJECT_DATE_MODIFIED).ok())
                .map(|pv| {
                    let inner = &pv.as_raw().Anonymous.Anonymous;
                    match inner.vt {
                        VT_DATE => ((inner.Anonymous.date - 25569.0) * 86400.0) as i64,
                        VT_FILETIME => {
                            let ft = inner.Anonymous.filetime;
                            (((ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64)
                                / 10_000_000) as i64
                                - 11_644_473_600
                        }
                        _ => 0,
                    }
                })
                .unwrap_or(0)
        }
        unsafe fn stream(&self, objid: &str) -> Result<(IStream, u32), String> {
            let w = wide(objid);
            let mut optimal: u32 = 0;
            let mut stream: Option<IStream> = None;
            self.resources
                .GetStream(
                    PCWSTR(w.as_ptr()),
                    &WPD_RESOURCE_DEFAULT,
                    STGM_READ.0 as u32,
                    &mut optimal,
                    &mut stream,
                )
                .map_err(|e| format!("Couldn't read over USB: {e}"))?;
            let stream = stream.ok_or_else(|| "USB read returned no stream.".to_string())?;
            Ok((stream, optimal))
        }
        /// Read a small named top-level file (inventory.json / dl_sources.json).
        pub fn read_text(&self, name: &str) -> Result<String, String> {
            unsafe {
                let id = self.top(name).ok_or_else(|| {
                    format!("{name} isn't published on the Switch yet — give it a second on the USB screen, then retry.")
                })?;
                let (stream, optimal) = self.stream(&id)?;
                let cap = if optimal > 0 { optimal as usize } else { 64 * 1024 };
                let mut buf = vec![0u8; cap];
                let mut out: Vec<u8> = Vec::new();
                loop {
                    let mut read: u32 = 0;
                    let hr = stream.Read(buf.as_mut_ptr() as *mut c_void, buf.len() as u32, Some(&mut read));
                    if read > 0 {
                        out.extend_from_slice(&buf[..read as usize]);
                    }
                    if read == 0 || (hr != S_OK && hr != S_FALSE) {
                        break;
                    }
                }
                String::from_utf8(out).map_err(|_| format!("{name} wasn't valid UTF-8 text."))
            }
        }
        /// Read a small binary named top-level file (console art, the debug
        /// bundle) plus its reported modified time, so a caller can compare
        /// against a locally cached copy before deciding to re-fetch at all —
        /// the USB mirror of GET /consoleart's Last-Modified. `mtime` is 0 if
        /// the driver reports none, in which case the caller should just
        /// trust whatever it just read.
        pub fn read_binary(&self, name: &str) -> Result<(Vec<u8>, i64), String> {
            unsafe {
                let id = self.top(name).ok_or_else(|| {
                    format!("{name} isn't published on the Switch yet — give it a second on the USB screen, then retry.")
                })?;
                let mtime = self.date_modified_of(&id);
                let (stream, optimal) = self.stream(&id)?;
                let cap = if optimal > 0 { optimal as usize } else { 64 * 1024 };
                let mut buf = vec![0u8; cap];
                let mut out: Vec<u8> = Vec::new();
                loop {
                    let mut read: u32 = 0;
                    let hr = stream.Read(buf.as_mut_ptr() as *mut c_void, buf.len() as u32, Some(&mut read));
                    if read > 0 {
                        out.extend_from_slice(&buf[..read as usize]);
                    }
                    if read == 0 || (hr != S_OK && hr != S_FALSE) {
                        break;
                    }
                }
                Ok((out, mtime))
            }
        }
        /// Stream a game file to a local path (chunked — games can be large).
        pub fn download(&self, folder: &str, name: &str, dest: &Path) -> Result<(), String> {
            use std::io::Write;
            unsafe {
                let id = self.file_in(folder, name)?;
                let (stream, optimal) = self.stream(&id)?;
                let mut f = std::fs::File::create(dest)
                    .map_err(|e| format!("Couldn't create the file: {e}"))?;
                let cap = if optimal > 0 { optimal as usize } else { 256 * 1024 };
                let mut buf = vec![0u8; cap];
                loop {
                    let mut read: u32 = 0;
                    let hr = stream.Read(buf.as_mut_ptr() as *mut c_void, buf.len() as u32, Some(&mut read));
                    if read > 0 {
                        f.write_all(&buf[..read as usize])
                            .map_err(|e| format!("Couldn't write the file: {e}"))?;
                    }
                    if read == 0 || (hr != S_OK && hr != S_FALSE) {
                        break;
                    }
                }
                Ok(())
            }
        }
        // A one-object id collection for Delete/Move. The object id goes in as a
        // string PROPVARIANT (the ergonomic ctor yields VT_BSTR; WPD reads the
        // wide string either way).
        unsafe fn id_collection(&self, objid: &str) -> Result<IPortableDevicePropVariantCollection, String> {
            let coll: IPortableDevicePropVariantCollection =
                CoCreateInstance(&PortableDevicePropVariantCollection, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| format!("WPD collection create failed: {e}"))?;
            let pv = PROPVARIANT::from(objid);
            coll.Add(&pv).map_err(|e| format!("WPD collection add failed: {e}"))?;
            Ok(coll)
        }
        /// Delete a file from a managed folder.
        pub fn delete(&self, folder: &str, name: &str) -> Result<(), String> {
            unsafe {
                let id = self.file_in(folder, name)?;
                let coll = self.id_collection(&id)?;
                let mut results: Option<IPortableDevicePropVariantCollection> = None;
                self.content
                    .Delete(0, &coll, &mut results)
                    .map_err(|e| format!("The Switch refused the delete: {e}"))
            }
        }
        /// Rename a file in place (SetObjectPropValue on ObjectFileName).
        pub fn rename(&self, folder: &str, name: &str, newname: &str) -> Result<(), String> {
            unsafe {
                let id = self.file_in(folder, name)?;
                let values: IPortableDeviceValues =
                    CoCreateInstance(&PortableDeviceValues, None, CLSCTX_INPROC_SERVER)
                        .map_err(|e| format!("WPD values create failed: {e}"))?;
                let nw = wide(newname);
                values
                    .SetStringValue(&WPD_OBJECT_ORIGINAL_FILE_NAME, PCWSTR(nw.as_ptr()))
                    .map_err(|e| format!("WPD set name failed: {e}"))?;
                let idw = wide(&id);
                self.props
                    .SetValues(PCWSTR(idw.as_ptr()), &values)
                    .map(|_| ())
                    .map_err(|e| format!("The Switch refused the rename: {e}"))
            }
        }
        /// Move a file to another managed folder (console or Inbox).
        pub fn move_to(&self, folder: &str, name: &str, dest_folder: &str) -> Result<(), String> {
            unsafe {
                let id = self.file_in(folder, name)?;
                let dest = self
                    .top(dest_folder)
                    .ok_or_else(|| format!("The {dest_folder} folder isn't on the Switch."))?;
                let coll = self.id_collection(&id)?;
                let destw = wide(&dest);
                let mut results: Option<IPortableDevicePropVariantCollection> = None;
                self.content
                    .Move(&coll, PCWSTR(destw.as_ptr()), &mut results)
                    .map_err(|e| format!("The Switch refused the move: {e}"))
            }
        }
        /// Push a local file onto the Switch (the WPD write path the read side
        /// lacked). Lands in the named console folder if it exists, else the
        /// Inbox, else the storage root; the responder's SendObject receives it.
        /// `progress(sent, total)` is called on a ~200ms throttle for the UI.
        pub fn push<F: Fn(u64, u64)>(
            &self,
            local: &Path,
            dest_folder: &str,
            progress: F,
        ) -> Result<(), String> {
            use std::io::Read;
            unsafe {
                let parent = self
                    .top(dest_folder)
                    .or_else(|| self.top("Inbox"))
                    .unwrap_or_else(|| self.root.clone());
                let fname = local
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or_else(|| "That file has no name.".to_string())?;
                let total = std::fs::metadata(local)
                    .map_err(|e| format!("Can't read the file: {e}"))?
                    .len();

                // Describe the object we're about to create: where it goes, how
                // big it is, its name, and that it's a plain file. Typed values,
                // so none of the VT_BSTR object-id caveat from the read path here.
                let values: IPortableDeviceValues =
                    CoCreateInstance(&PortableDeviceValues, None, CLSCTX_INPROC_SERVER)
                        .map_err(|e| format!("WPD values create failed: {e}"))?;
                let pw = wide(&parent);
                values
                    .SetStringValue(&WPD_OBJECT_PARENT_ID, PCWSTR(pw.as_ptr()))
                    .map_err(|e| format!("WPD set parent failed: {e}"))?;
                values
                    .SetUnsignedLargeIntegerValue(&WPD_OBJECT_SIZE, total)
                    .map_err(|e| format!("WPD set size failed: {e}"))?;
                let nw = wide(fname);
                values
                    .SetStringValue(&WPD_OBJECT_ORIGINAL_FILE_NAME, PCWSTR(nw.as_ptr()))
                    .map_err(|e| format!("WPD set name failed: {e}"))?;
                values
                    .SetStringValue(&WPD_OBJECT_NAME, PCWSTR(nw.as_ptr()))
                    .map_err(|e| format!("WPD set name failed: {e}"))?;
                values
                    .SetGuidValue(&WPD_OBJECT_CONTENT_TYPE, &WPD_CONTENT_TYPE_GENERIC_FILE)
                    .map_err(|e| format!("WPD set type failed: {e}"))?;
                values
                    .SetGuidValue(&WPD_OBJECT_FORMAT, &WPD_OBJECT_FORMAT_UNSPECIFIED)
                    .map_err(|e| format!("WPD set format failed: {e}"))?;

                let mut optimal: u32 = 0;
                let mut stream: Option<IStream> = None;
                self.content
                    .CreateObjectWithPropertiesAndData(
                        &values,
                        &mut stream,
                        &mut optimal,
                        std::ptr::null_mut(),
                    )
                    .map_err(|e| format!("The Switch refused the upload: {e}"))?;
                let stream = stream.ok_or_else(|| "USB upload returned no stream.".to_string())?;

                let mut file = std::fs::File::open(local)
                    .map_err(|e| format!("Can't open the file: {e}"))?;
                let cap = if optimal > 0 { optimal as usize } else { 256 * 1024 };
                let mut buf = vec![0u8; cap];
                let mut sent: u64 = 0;
                let mut last = std::time::Instant::now();
                loop {
                    let n = file.read(&mut buf).map_err(|e| format!("Disk read failed: {e}"))?;
                    if n == 0 {
                        break;
                    }
                    // IStream::Write is allowed to write fewer bytes than asked
                    // (the `written` out-param exists for exactly that case) --
                    // advance by what actually went out and re-offer the rest
                    // instead of dropping it. A silently short-written chunk here
                    // would leave the object's actual byte count under what
                    // SendObjectInfo already declared to the device, which the
                    // device has no way to detect from its end (it just keeps
                    // waiting on the USB endpoint for the remaining declared
                    // bytes that this call already told the disk-read loop it
                    // was done with) -- a truncated, never-finishing transfer
                    // that looks identical to a genuine hang.
                    let mut off = 0usize;
                    while off < n {
                        let mut written: u32 = 0;
                        stream
                            .Write(
                                buf[off..n].as_ptr() as *const c_void,
                                (n - off) as u32,
                                Some(&mut written),
                            )
                            .ok()
                            .map_err(|e| format!("USB write failed: {e}"))?;
                        if written == 0 {
                            return Err("USB write stalled (0 bytes accepted).".to_string());
                        }
                        off += written as usize;
                        sent += written as u64;
                    }
                    if last.elapsed() >= std::time::Duration::from_millis(200) {
                        progress(sent, total);
                        last = std::time::Instant::now();
                    }
                }
                stream
                    .Commit(STGC_DEFAULT)
                    .map_err(|e| format!("The Switch rejected the upload: {e}"))?;
                // The in-loop progress ticks are throttled to ~5 Hz, so the last
                // one usually lands a few percent short; emit a final 100% now
                // that Commit has returned, so the bar doesn't freeze near the end.
                progress(total, total);
                Ok(())
            }
        }

        /// Write a small named blob to the storage root (parent = root), for the
        /// config files the responder applies specially — currently the shared
        /// update manifest (update_sources.json), which SendObject validates and
        /// promotes to UPDSRC_PATH. Not for games (use `push`, which folders them).
        pub fn write_top(&self, name: &str, data: &[u8]) -> Result<(), String> {
            unsafe {
                let parent = self.root.clone();
                let total = data.len() as u64;
                let values: IPortableDeviceValues =
                    CoCreateInstance(&PortableDeviceValues, None, CLSCTX_INPROC_SERVER)
                        .map_err(|e| format!("WPD values create failed: {e}"))?;
                let pw = wide(&parent);
                values
                    .SetStringValue(&WPD_OBJECT_PARENT_ID, PCWSTR(pw.as_ptr()))
                    .map_err(|e| format!("WPD set parent failed: {e}"))?;
                values
                    .SetUnsignedLargeIntegerValue(&WPD_OBJECT_SIZE, total)
                    .map_err(|e| format!("WPD set size failed: {e}"))?;
                let nw = wide(name);
                values
                    .SetStringValue(&WPD_OBJECT_ORIGINAL_FILE_NAME, PCWSTR(nw.as_ptr()))
                    .map_err(|e| format!("WPD set name failed: {e}"))?;
                values
                    .SetStringValue(&WPD_OBJECT_NAME, PCWSTR(nw.as_ptr()))
                    .map_err(|e| format!("WPD set name failed: {e}"))?;
                values
                    .SetGuidValue(&WPD_OBJECT_CONTENT_TYPE, &WPD_CONTENT_TYPE_GENERIC_FILE)
                    .map_err(|e| format!("WPD set type failed: {e}"))?;
                values
                    .SetGuidValue(&WPD_OBJECT_FORMAT, &WPD_OBJECT_FORMAT_UNSPECIFIED)
                    .map_err(|e| format!("WPD set format failed: {e}"))?;

                let mut optimal: u32 = 0;
                let mut stream: Option<IStream> = None;
                self.content
                    .CreateObjectWithPropertiesAndData(
                        &values,
                        &mut stream,
                        &mut optimal,
                        std::ptr::null_mut(),
                    )
                    .map_err(|e| format!("The Switch refused the upload: {e}"))?;
                let stream = stream.ok_or_else(|| "USB upload returned no stream.".to_string())?;

                let mut off = 0usize;
                while off < data.len() {
                    let end = std::cmp::min(off + 256 * 1024, data.len());
                    let mut written: u32 = 0;
                    stream
                        .Write(
                            data[off..end].as_ptr() as *const c_void,
                            (end - off) as u32,
                            Some(&mut written),
                        )
                        .ok()
                        .map_err(|e| format!("USB write failed: {e}"))?;
                    off += written.max(1) as usize;
                }
                stream
                    .Commit(STGC_DEFAULT)
                    .map_err(|e| format!("The Switch rejected the upload: {e}"))?;
                Ok(())
            }
        }

        // ---- SD Card tab: arbitrary-depth browse/write/delete over the whole
        // card (Prefs.sd_full_access), via the responder's synthetic "SD Card"
        // top-level object. Every method below takes `parts` already including
        // that leading "SD Card" component (see sd_fs_parts) so `resolve`
        // needs no special-casing versus the console-folder methods above. ----

        /// List one folder's children for the SD Card tab.
        pub fn list(&self, parts: &[String]) -> Result<Vec<super::SdEntry>, String> {
            unsafe {
                let parent = self.resolve(parts)?;
                let mut out = Vec::new();
                for id in children(&self.content, &parent) {
                    let name = name_of(&self.props, &id);
                    if name.is_empty() {
                        continue;
                    }
                    let is_dir = self.is_folder(&id);
                    let size = if is_dir { 0 } else { self.size_of(&id) };
                    let mtime = self.date_modified_of(&id);
                    out.push(super::SdEntry { name, is_dir, size, mtime });
                }
                Ok(out)
            }
        }
        /// Stream a file at an arbitrary path to a local path (mirrors
        /// `download`, but resolved by full path instead of folder+name).
        pub fn download_path(&self, parts: &[String], dest: &Path) -> Result<(), String> {
            use std::io::Write;
            unsafe {
                let id = self.resolve(parts)?;
                let (stream, optimal) = self.stream(&id)?;
                let mut f = std::fs::File::create(dest)
                    .map_err(|e| format!("Couldn't create the file: {e}"))?;
                let cap = if optimal > 0 { optimal as usize } else { 256 * 1024 };
                let mut buf = vec![0u8; cap];
                loop {
                    let mut read: u32 = 0;
                    let hr = stream.Read(buf.as_mut_ptr() as *mut c_void, buf.len() as u32, Some(&mut read));
                    if read > 0 {
                        f.write_all(&buf[..read as usize])
                            .map_err(|e| format!("Couldn't write the file: {e}"))?;
                    }
                    if read == 0 || (hr != S_OK && hr != S_FALSE) {
                        break;
                    }
                }
                Ok(())
            }
        }
        /// Delete a file or folder (recursively) at an arbitrary path.
        pub fn delete_path(&self, parts: &[String]) -> Result<(), String> {
            unsafe {
                let id = self.resolve(parts)?;
                let coll = self.id_collection(&id)?;
                let mut results: Option<IPortableDevicePropVariantCollection> = None;
                self.content
                    .Delete(0, &coll, &mut results)
                    .map_err(|e| format!("The Switch refused the delete: {e}"))
            }
        }
        /// Rename the file or folder at an arbitrary path in place.
        pub fn rename_path(&self, parts: &[String], newname: &str) -> Result<(), String> {
            unsafe {
                let id = self.resolve(parts)?;
                let values: IPortableDeviceValues =
                    CoCreateInstance(&PortableDeviceValues, None, CLSCTX_INPROC_SERVER)
                        .map_err(|e| format!("WPD values create failed: {e}"))?;
                let nw = wide(newname);
                values
                    .SetStringValue(&WPD_OBJECT_ORIGINAL_FILE_NAME, PCWSTR(nw.as_ptr()))
                    .map_err(|e| format!("WPD set name failed: {e}"))?;
                let idw = wide(&id);
                self.props
                    .SetValues(PCWSTR(idw.as_ptr()), &values)
                    .map(|_| ())
                    .map_err(|e| format!("The Switch refused the rename: {e}"))
            }
        }
        /// Create a new, empty folder under an arbitrary-depth parent.
        pub fn mkdir_path(&self, parent_parts: &[String], name: &str) -> Result<(), String> {
            unsafe {
                let parent = self.resolve(parent_parts)?;
                let values: IPortableDeviceValues =
                    CoCreateInstance(&PortableDeviceValues, None, CLSCTX_INPROC_SERVER)
                        .map_err(|e| format!("WPD values create failed: {e}"))?;
                let pw = wide(&parent);
                values
                    .SetStringValue(&WPD_OBJECT_PARENT_ID, PCWSTR(pw.as_ptr()))
                    .map_err(|e| format!("WPD set parent failed: {e}"))?;
                let nw = wide(name);
                values
                    .SetStringValue(&WPD_OBJECT_ORIGINAL_FILE_NAME, PCWSTR(nw.as_ptr()))
                    .map_err(|e| format!("WPD set name failed: {e}"))?;
                values
                    .SetStringValue(&WPD_OBJECT_NAME, PCWSTR(nw.as_ptr()))
                    .map_err(|e| format!("WPD set name failed: {e}"))?;
                values
                    .SetGuidValue(&WPD_OBJECT_CONTENT_TYPE, &WPD_CONTENT_TYPE_FOLDER)
                    .map_err(|e| format!("WPD set type failed: {e}"))?;
                let mut cookie = PWSTR::null();
                self.content
                    .CreateObjectWithPropertiesOnly(&values, &mut cookie)
                    .map_err(|e| format!("The Switch refused to create the folder: {e}"))?;
                free_pwstr(cookie);
                Ok(())
            }
        }
        /// Push a local file onto an arbitrary-depth destination folder (the SD
        /// Card tab's upload). Mirrors `push`, but the parent is resolved by
        /// full path instead of a single managed-folder name / Inbox fallback,
        /// and there is no such fallback here -- an unresolvable destination
        /// folder is an error, not a silent redirect to Inbox.
        /// `remote_name` overrides the uploaded object's own name (both
        /// WPD_OBJECT_ORIGINAL_FILE_NAME and WPD_OBJECT_NAME) independent of
        /// `local`'s actual basename -- None uses `local`'s name, same as
        /// before this parameter existed. Needed for the bulk-upload marker
        /// (see usb_fs_upload_bulk): the local temp zip is named for
        /// uniqueness (haulnx-<tag>-<pid>-<stamp>.zip), but the device only
        /// recognizes the batch by its exact remote name.
        pub fn upload_path<F: Fn(u64, u64)>(
            &self,
            dir_parts: &[String],
            local: &Path,
            remote_name: Option<&str>,
            progress: F,
        ) -> Result<(), String> {
            use std::io::Read;
            unsafe {
                let parent = self.resolve(dir_parts)?;
                let local_name = local
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or_else(|| "That file has no name.".to_string())?;
                let fname = remote_name.unwrap_or(local_name);
                let total = std::fs::metadata(local)
                    .map_err(|e| format!("Can't read the file: {e}"))?
                    .len();

                let values: IPortableDeviceValues =
                    CoCreateInstance(&PortableDeviceValues, None, CLSCTX_INPROC_SERVER)
                        .map_err(|e| format!("WPD values create failed: {e}"))?;
                let pw = wide(&parent);
                values
                    .SetStringValue(&WPD_OBJECT_PARENT_ID, PCWSTR(pw.as_ptr()))
                    .map_err(|e| format!("WPD set parent failed: {e}"))?;
                values
                    .SetUnsignedLargeIntegerValue(&WPD_OBJECT_SIZE, total)
                    .map_err(|e| format!("WPD set size failed: {e}"))?;
                let nw = wide(fname);
                values
                    .SetStringValue(&WPD_OBJECT_ORIGINAL_FILE_NAME, PCWSTR(nw.as_ptr()))
                    .map_err(|e| format!("WPD set name failed: {e}"))?;
                values
                    .SetStringValue(&WPD_OBJECT_NAME, PCWSTR(nw.as_ptr()))
                    .map_err(|e| format!("WPD set name failed: {e}"))?;
                values
                    .SetGuidValue(&WPD_OBJECT_CONTENT_TYPE, &WPD_CONTENT_TYPE_GENERIC_FILE)
                    .map_err(|e| format!("WPD set type failed: {e}"))?;
                values
                    .SetGuidValue(&WPD_OBJECT_FORMAT, &WPD_OBJECT_FORMAT_UNSPECIFIED)
                    .map_err(|e| format!("WPD set format failed: {e}"))?;

                let mut optimal: u32 = 0;
                let mut stream: Option<IStream> = None;
                self.content
                    .CreateObjectWithPropertiesAndData(
                        &values,
                        &mut stream,
                        &mut optimal,
                        std::ptr::null_mut(),
                    )
                    .map_err(|e| format!("The Switch refused the upload: {e}"))?;
                let stream = stream.ok_or_else(|| "USB upload returned no stream.".to_string())?;

                let mut file = std::fs::File::open(local)
                    .map_err(|e| format!("Can't open the file: {e}"))?;
                let cap = if optimal > 0 { optimal as usize } else { 256 * 1024 };
                let mut buf = vec![0u8; cap];
                let mut sent: u64 = 0;
                let mut last = std::time::Instant::now();
                loop {
                    let n = file.read(&mut buf).map_err(|e| format!("Disk read failed: {e}"))?;
                    if n == 0 {
                        break;
                    }
                    let mut off = 0usize;
                    while off < n {
                        let mut written: u32 = 0;
                        stream
                            .Write(
                                buf[off..n].as_ptr() as *const c_void,
                                (n - off) as u32,
                                Some(&mut written),
                            )
                            .ok()
                            .map_err(|e| format!("USB write failed: {e}"))?;
                        if written == 0 {
                            return Err("USB write stalled (0 bytes accepted).".to_string());
                        }
                        off += written as usize;
                        sent += written as u64;
                    }
                    if last.elapsed() >= std::time::Duration::from_millis(200) {
                        progress(sent, total);
                        last = std::time::Instant::now();
                    }
                }
                stream
                    .Commit(STGC_DEFAULT)
                    .map_err(|e| format!("The Switch rejected the upload: {e}"))?;
                progress(total, total);
                Ok(())
            }
        }
    }
}

/// Read inventory.json over USB (Windows only — the companion is a Windows exe).
#[tauri::command]
async fn usb_inventory() -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(|| -> Result<String, String> {
            wpd::open()?.read_text("inventory.json")
        })
        .await
        .map_err(|_| "USB inventory task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err("USB inventory is only available on Windows.".into())
    }
}

/// Read the saved collection (dl_sources.json) over USB.
#[tauri::command]
async fn usb_collection() -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(|| -> Result<String, String> {
            wpd::open()?.read_text("dl_sources.json")
        })
        .await
        .map_err(|_| "USB collection task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err("USB is only available on Windows.".into())
    }
}

/// Read the device's saved archive.org credentials (credentials.json) over USB,
/// so the companion can adopt them on connect. 404 on the device (no creds saved
/// yet) surfaces as an error the caller treats as "nothing to pull".
#[tauri::command]
async fn usb_credentials() -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(|| -> Result<String, String> {
            wpd::open()?.read_text("credentials.json")
        })
        .await
        .map_err(|_| "USB credentials task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err("USB is only available on Windows.".into())
    }
}

/// Read the shared emulator/app update manifest (update_sources.json) over USB,
/// so the companion can pull and adopt the same list the Wi-Fi server serves.
/// An error (not published yet) is treated by the caller as "nothing to pull".
#[tauri::command]
async fn usb_update_sources() -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(|| -> Result<String, String> {
            wpd::open()?.read_text("update_sources.json")
        })
        .await
        .map_err(|_| "USB update-sources task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err("USB is only available on Windows.".into())
    }
}

/// Push the shared update manifest to the Switch over USB. The JSON is written to
/// the storage root as update_sources.json; the responder validates it and, only
/// if it parses as a sources list, promotes it to the live manifest (the USB
/// mirror of the Wi-Fi push). `json` is the manifest text built by the frontend.
#[tauri::command]
async fn usb_push_update_sources(json: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
            wpd::open()?.write_top("update_sources.json", json.as_bytes())
        })
        .await
        .map_err(|_| "USB update-sources push task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = json;
        Err("USB is only available on Windows.".into())
    }
}

/// Read the rolling debug-log bundle over USB (the USB mirror of GET
/// /debug_bundle.txt) — same text the caller then hands to save_device_log,
/// so a bug report never needs a manual SD-card copy regardless of transport.
#[tauri::command]
async fn usb_debug_log() -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(|| -> Result<String, String> {
            wpd::open()?.read_text("debug_bundle.txt")
        })
        .await
        .map_err(|_| "USB debug-log task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err("USB is only available on Windows.".into())
    }
}

/// Read the Switch's own live Queue-tab snapshot over USB (the USB mirror of
/// GET /queue_status.json), so the Downloads tab can show the device's own
/// transfers alongside this app's PC-side ones.
#[tauri::command]
async fn usb_queue_status() -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(|| -> Result<String, String> {
            wpd::open()?.read_text("queue_status.json")
        })
        .await
        .map_err(|_| "USB queue-status task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err("USB is only available on Windows.".into())
    }
}

/// Shared by usb_boxart_search/usb_boxart_pick: stage `json` to a temp file
/// and upload it as "boxart_request.json" at the storage root (see
/// ParseArtPushName's sibling handling in mtp/responder.cpp for why a JSON
/// blob, not a marker filename -- free-text search queries can't safely ride
/// in an MTP object name). Blocking; callers already run inside
/// spawn_blocking.
#[cfg(target_os = "windows")]
fn usb_boxart_send_request(json: String) -> Result<(), String> {
    let tmp = std::env::temp_dir().join(format!("haulnx-boxreq-{}.json", std::process::id()));
    std::fs::write(&tmp, json).map_err(|e| format!("Can't stage the request: {e}"))?;
    let res = wpd::open().and_then(|d| {
        d.upload_path(&[], &tmp, Some("boxart_request.json"), |_, _| {})
    });
    let _ = std::fs::remove_file(&tmp);
    res
}

/// Kick off a box-art search over USB -- the USB mirror of GET boxartsearch.
/// Fire-and-forget like its Wi-Fi counterpart: the caller polls
/// usb_boxart_status for the outcome.
#[tauri::command]
async fn usb_boxart_search(target: String, query: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(move || {
            usb_boxart_send_request(json!({"kind":"search","target":target,"query":query}).to_string())
        })
        .await
        .map_err(|_| "USB box-art search task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (target, query);
        Err("USB is only available on Windows.".into())
    }
}

/// Commit a box-art search candidate over USB -- the USB mirror of POST
/// boxartpick. Fire-and-forget; the caller polls usb_boxart_status.
#[tauri::command]
async fn usb_boxart_pick(target: String, index: i32) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(move || {
            usb_boxart_send_request(json!({"kind":"pick","target":target,"index":index}).to_string())
        })
        .await
        .map_err(|_| "USB box-art pick task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (target, index);
        Err("USB is only available on Windows.".into())
    }
}

/// Read the box-art search/pick status over USB -- the USB mirror of GET
/// boxartsearch_status/boxartpick_status.
#[tauri::command]
async fn usb_boxart_status() -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(|| -> Result<String, String> {
            wpd::open()?.read_text("boxart_status.json")
        })
        .await
        .map_err(|_| "USB box-art status task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err("USB is only available on Windows.".into())
    }
}

/// Read one search candidate's thumbnail over USB -- the USB mirror of GET
/// boxartthumb?p=<i>. Returns a `data:` URL ready for an <img src>.
#[tauri::command]
async fn usb_boxart_thumb(index: i32) -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
            let name = format!("boxart_thumb_{index}.png");
            let (bytes, _mtime) = wpd::open()?.read_binary(&name)?;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            Ok(format!("data:image/png;base64,{b64}"))
        })
        .await
        .map_err(|_| "USB box-art thumb task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = index;
        Err("USB is only available on Windows.".into())
    }
}

/// Read one console's current cover art over USB (the USB mirror of GET
/// /consoleart) plus its reported modified time, so the box-art cache on the
/// JS side can skip re-pulling art it already has cached. Returns a `data:`
/// URL ready for an <img src> and the epoch-seconds mtime (0 if unknown).
#[tauri::command]
async fn usb_consoleart(target: String) -> Result<(String, i64), String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(move || -> Result<(String, i64), String> {
            let name = format!("art_{target}.png");
            let (bytes, mtime) = wpd::open()?.read_binary(&name)?;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            Ok((format!("data:image/png;base64,{b64}"), mtime))
        })
        .await
        .map_err(|_| "USB console-art task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = target;
        Err("USB is only available on Windows.".into())
    }
}

/// Download a game file over USB into dest_dir/<folder>/<name>.
#[tauri::command]
async fn usb_download(folder: String, name: String, dest_dir: String) -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
            let mut dir = PathBuf::from(&dest_dir);
            let sub = safe_component(&folder);
            if !sub.is_empty() {
                dir.push(&sub);
            }
            std::fs::create_dir_all(&dir)
                .map_err(|e| format!("Couldn't create the download folder: {e}"))?;
            let final_path = dir.join(safe_name(&name));
            let tmp = final_path.with_extension("part");
            wpd::open()?.download(&folder, &name, &tmp)?;
            std::fs::rename(&tmp, &final_path)
                .map_err(|e| format!("Couldn't finalize the file: {e}"))?;
            Ok(final_path.to_string_lossy().into_owned())
        })
        .await
        .map_err(|_| "USB download task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (folder, name, dest_dir);
        Err("USB is only available on Windows.".into())
    }
}

/// Delete a file over USB from a managed folder.
#[tauri::command]
async fn usb_delete(folder: String, name: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(move || wpd::open()?.delete(&folder, &name))
            .await
            .map_err(|_| "USB delete task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (folder, name);
        Err("USB is only available on Windows.".into())
    }
}

/// Rename a file over USB.
#[tauri::command]
async fn usb_rename(folder: String, name: String, new_name: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(move || wpd::open()?.rename(&folder, &name, &new_name))
            .await
            .map_err(|_| "USB rename task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (folder, name, new_name);
        Err("USB is only available on Windows.".into())
    }
}

/// Move a file to another managed folder over USB.
#[tauri::command]
async fn usb_move(folder: String, name: String, dest_folder: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(move || wpd::open()?.move_to(&folder, &name, &dest_folder))
            .await
            .map_err(|_| "USB move task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (folder, name, dest_folder);
        Err("USB is only available on Windows.".into())
    }
}

#[derive(Clone, Serialize)]
struct PushProgress {
    id: String,
    sent: u64,
    total: u64,
    done: bool,
    error: Option<String>,
}

/// One entry in an SD Card tab directory listing (USB/WPD side) — mirrors the
/// Wi-Fi inventory server's fs_list JSON shape (name/dir/size/mtime) so the
/// frontend treats both transports identically. mtime is Unix epoch seconds,
/// 0 if the WPD driver didn't report one (folders always report 0, same as
/// the Wi-Fi side's build_fs_list_json in source/httpsrv.c).
#[derive(Clone, Serialize)]
pub struct SdEntry {
    name: String,
    is_dir: bool,
    size: u64,
    mtime: i64,
}

/// Every SD Card tab USB command resolves through the responder's synthetic
/// "SD Card" top-level object (see mtp/responder.cpp) — prepend it once here
/// so the frontend only ever deals in plain paths relative to sdmc:/, the
/// same shape the Wi-Fi fs_* routes use.
fn sd_fs_parts(parts: &[String]) -> Vec<String> {
    let mut full = Vec::with_capacity(parts.len() + 1);
    full.push("SD Card".to_string());
    full.extend_from_slice(parts);
    full
}

// ---- SD Card tab (USB/WPD side) -------------------------------------------
// Wi-Fi hits the inventory server's fs_* routes directly from the frontend
// (plain fetch, no Rust needed); over USB there's no HTTP to fetch, so these
// mirror them one-for-one through wpd::Device's *_path methods. `parts` is
// always relative to sdmc:/ root — sd_fs_parts prepends the responder's
// synthetic "SD Card" top-level object once here, so the frontend's path
// model never has to know which transport it's talking to.

/// List one SD Card folder's contents over USB.
#[tauri::command]
async fn usb_fs_list(parts: Vec<String>) -> Result<Vec<SdEntry>, String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(move || wpd::open()?.list(&sd_fs_parts(&parts)))
            .await
            .map_err(|_| "USB listing task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = parts;
        Err("USB is only available on Windows.".into())
    }
}

/// Download a file at an arbitrary SD card path to the PC over USB. `subdir`
/// (relative to `dest_dir`, "/"-separated) mirrors `download_file`'s own
/// param -- normally empty (a plain single-file download lands straight in
/// dest_dir), but the SD Card tab's "download a folder" recursion passes the
/// walked subfolder chain here so USB gets the same nested-directory mirror
/// as the Wi-Fi side, one real WPD download per file.
#[tauri::command]
async fn usb_fs_download(parts: Vec<String>, dest_dir: String, subdir: String) -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
            let name = parts.last().cloned().unwrap_or_default();
            if name.is_empty() {
                return Err("Nothing selected to download.".into());
            }
            let mut dir = PathBuf::from(&dest_dir);
            for part in subdir.split(['/', '\\']) {
                let sub = safe_component(part);
                if !sub.is_empty() {
                    dir.push(&sub);
                }
            }
            std::fs::create_dir_all(&dir)
                .map_err(|e| format!("Couldn't create the download folder: {e}"))?;
            let final_path = dir.join(safe_name(&name));
            let tmp = final_path.with_extension("part");
            wpd::open()?.download_path(&sd_fs_parts(&parts), &tmp)?;
            std::fs::rename(&tmp, &final_path)
                .map_err(|e| format!("Couldn't finalize the file: {e}"))?;
            Ok(final_path.to_string_lossy().into_owned())
        })
        .await
        .map_err(|_| "USB download task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (parts, dest_dir, subdir);
        Err("USB is only available on Windows.".into())
    }
}

/// Cap on how much of a file the SD Card tab's preview dialog will read, both
/// here and in the Wi-Fi fs_get path (index.html truncates the same way) — a
/// config file the user actually wants to eyeball before editing is nowhere
/// near this size; it exists so clicking Preview on something unexpectedly
/// large doesn't stall on a multi-hundred-MB download just to show text.
const PREVIEW_MAX_BYTES: usize = 512 * 1024;

/// Read a small text file at an arbitrary SD card path over USB, for the SD
/// Card tab's Preview button. WPD has no in-memory read — download_path is
/// the only primitive — so this stages to a throwaway temp file (unique per
/// call so two previews in flight can't collide) and reads it back, capped at
/// PREVIEW_MAX_BYTES and deleted either way once read. Lossy UTF-8 so a
/// stray non-UTF8 byte shows as U+FFFD instead of failing the whole preview.
#[tauri::command]
async fn usb_fs_read_text(parts: Vec<String>) -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
            let name = parts.last().cloned().unwrap_or_default();
            if name.is_empty() {
                return Err("Nothing selected to preview.".into());
            }
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let tmp = std::env::temp_dir()
                .join(format!("haulnx-preview-{}-{}.tmp", std::process::id(), stamp));
            let result = (|| -> Result<String, String> {
                wpd::open()?.download_path(&sd_fs_parts(&parts), &tmp)?;
                let bytes = std::fs::read(&tmp)
                    .map_err(|e| format!("Couldn't read the downloaded file: {e}"))?;
                let capped = if bytes.len() > PREVIEW_MAX_BYTES {
                    &bytes[..PREVIEW_MAX_BYTES]
                } else {
                    &bytes[..]
                };
                Ok(String::from_utf8_lossy(capped).into_owned())
            })();
            let _ = std::fs::remove_file(&tmp);
            result
        })
        .await
        .map_err(|_| "USB preview task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = parts;
        Err("USB is only available on Windows.".into())
    }
}

/// Delete a file or folder (recursively) at an arbitrary SD card path.
#[tauri::command]
async fn usb_fs_delete(parts: Vec<String>) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(move || wpd::open()?.delete_path(&sd_fs_parts(&parts)))
            .await
            .map_err(|_| "USB delete task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = parts;
        Err("USB is only available on Windows.".into())
    }
}

/// Rename the file or folder at an arbitrary SD card path.
#[tauri::command]
async fn usb_fs_rename(parts: Vec<String>, new_name: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(move || {
            wpd::open()?.rename_path(&sd_fs_parts(&parts), &new_name)
        })
        .await
        .map_err(|_| "USB rename task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (parts, new_name);
        Err("USB is only available on Windows.".into())
    }
}

/// Create a new folder under an arbitrary SD card path.
#[tauri::command]
async fn usb_fs_mkdir(parts: Vec<String>, name: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(move || {
            wpd::open()?.mkdir_path(&sd_fs_parts(&parts), &name)
        })
        .await
        .map_err(|_| "USB mkdir task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (parts, name);
        Err("USB is only available on Windows.".into())
    }
}

/// Upload a local file into an arbitrary SD card folder over USB.
#[tauri::command]
async fn usb_fs_upload(app: tauri::AppHandle, id: String, path: String, parts: Vec<String>) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(move || {
            let emit = |sent: u64, total: u64, done: bool, error: Option<String>| {
                let _ = app.emit(
                    "push://progress",
                    PushProgress { id: id.clone(), sent, total, done, error },
                );
            };
            let total_hint = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            emit(0, total_hint, false, None);
            let res = wpd::open().and_then(|d| {
                d.upload_path(&sd_fs_parts(&parts), Path::new(&path), None, |sent, total| {
                    emit(sent, total, false, None)
                })
            });
            match res {
                Ok(()) => {
                    emit(0, 0, true, None);
                    Ok(())
                }
                Err(e) => {
                    emit(0, 0, true, Some(e.clone()));
                    Err(e)
                }
            }
        })
        .await
        .map_err(|_| "USB upload task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (app, id, path, parts);
        Err("USB is only available on Windows.".into())
    }
}

/// Upload a zip into an arbitrary SD card folder over USB and have the device
/// unpack it there in one shot -- the USB counterpart of wifi_push's
/// fs_extract flag (see BULK_UPLOAD_MARKER). Used for the same batched pushes
/// Wi-Fi uses it for: a whole folder, or several loose files at once, instead
/// of one usb_fs_upload per item.
#[tauri::command]
async fn usb_fs_upload_bulk(app: tauri::AppHandle, id: String, path: String, parts: Vec<String>) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(move || {
            let emit = |sent: u64, total: u64, done: bool, error: Option<String>| {
                let _ = app.emit(
                    "push://progress",
                    PushProgress { id: id.clone(), sent, total, done, error },
                );
            };
            let total_hint = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            emit(0, total_hint, false, None);
            let res = wpd::open().and_then(|d| {
                d.upload_path(
                    &sd_fs_parts(&parts),
                    Path::new(&path),
                    Some(BULK_UPLOAD_MARKER),
                    |sent, total| emit(sent, total, false, None),
                )
            });
            match res {
                Ok(()) => {
                    emit(0, 0, true, None);
                    Ok(())
                }
                Err(e) => {
                    emit(0, 0, true, Some(e.clone()));
                    Err(e)
                }
            }
        })
        .await
        .map_err(|_| "USB bulk upload task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (app, id, path, parts);
        Err("USB is only available on Windows.".into())
    }
}

/// Push new cover art for console `target` over USB -- the USB counterpart of
/// wifi_push's `art_target` flag (X-Art-Target). Uploads straight to the
/// storage root (no folder to resolve) under the ART_PUSH_PREFIX-prefixed
/// name the device recognizes (see ParseArtPushName in mtp/responder.cpp,
/// which this format string must match exactly); no progress needed, a cover
/// image is small.
#[tauri::command]
async fn usb_push_console_art(target: String, path: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(move || {
            let remote = format!("{ART_PUSH_PREFIX}{target}.png");
            wpd::open()?.upload_path(&[], Path::new(&path), Some(&remote), |_, _| {})
        })
        .await
        .map_err(|_| "USB console-art push task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (target, path);
        Err("USB is only available on Windows.".into())
    }
}

/// Percent-encode a filename for the X-Filename header (the device percent-decodes
/// it before sanitising). Encodes everything outside the unreserved set, so any
/// byte the decoder must reconstruct arrives as %XX.
fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{:02X}", b));
        }
    }
    out
}

/// Recursive total size of everything under `path`. Used for a folder-per-game
/// entry in list_local_roms below (a PS Vita NoNpDRM dump, or any other format
/// that ships a game as a directory tree rather than one file) so it can report
/// one size the same way a plain ROM file does. Best-effort: an unreadable
/// subtree just doesn't count rather than failing the whole listing.
fn local_dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for e in rd.filter_map(|e| e.ok()) {
            if let Ok(t) = e.file_type() {
                if t.is_dir() {
                    stack.push(e.path());
                } else if t.is_file() {
                    total += e.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        }
    }
    total
}

/// List a local ROM library folder for the Library tab: its immediate console
/// subfolders (each with its game files) plus any loose files at the root.
/// Skips hidden and in-progress (`.part`) files. Returns JSON as a string.
///
/// A console subfolder's own immediate children can be either files (the
/// common case) or directories -- some formats (PS Vita's NoNpDRM dumps, for
/// instance) ship a game as a folder tree of arbitrary depth and internal
/// naming, with no single file that stands for the whole game, alongside
/// consoles where a game is still just one file (e.g. a Vita game distributed
/// as a single `.vpk`). Each such directory is listed as one entry with
/// `"dir": true` and its recursive total size, so it shows up in the Library
/// tab as one game -- same treatment multi-file sets (.cue+.bin) already get
/// on the device's own Installed browser -- rather than being invisible or
/// exploded into its internal pieces. The frontend and the push/delete/rename
/// commands below key off that flag to operate on the whole subtree.
#[tauri::command]
async fn list_local_roms(dir: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        let root = PathBuf::from(&dir);
        if !root.is_dir() {
            return Err("That folder doesn't exist. Pick your ROMs folder.".into());
        }
        let skip = |n: &str| n.starts_with('.') || n.ends_with(".part");
        let entry = |p: &Path| -> Option<serde_json::Value> {
            let name = p.file_name()?.to_string_lossy().into_owned();
            if skip(&name) {
                return None;
            }
            let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            Some(json!({ "name": name, "size": size, "path": p.to_string_lossy() }))
        };
        let read_sorted = |d: &Path| -> Vec<std::fs::DirEntry> {
            let mut v: Vec<_> = std::fs::read_dir(d)
                .map(|rd| rd.filter_map(|e| e.ok()).collect())
                .unwrap_or_default();
            v.sort_by_key(|e| e.file_name());
            v
        };

        let mut folders: Vec<serde_json::Value> = Vec::new();
        let mut loose: Vec<serde_json::Value> = Vec::new();
        for e in read_sorted(&root) {
            let path = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                if name.starts_with('.') {
                    continue;
                }
                let files: Vec<serde_json::Value> = read_sorted(&path)
                    .iter()
                    .filter_map(|f| {
                        let fp = f.path();
                        match f.file_type() {
                            Ok(t) if t.is_file() => entry(&fp),
                            Ok(t) if t.is_dir() => {
                                let gname = f.file_name().to_string_lossy().into_owned();
                                if skip(&gname) {
                                    return None;
                                }
                                let size = local_dir_size(&fp);
                                Some(json!({
                                    "name": gname, "size": size,
                                    "path": fp.to_string_lossy(), "dir": true
                                }))
                            }
                            _ => None,
                        }
                    })
                    .collect();
                folders.push(json!({ "name": name, "files": files }));
            } else if let Some(v) = entry(&path) {
                loose.push(v);
            }
        }
        Ok(json!({ "root": dir, "folders": folders, "loose": loose }).to_string())
    })
    .await
    .map_err(|_| "Library listing task failed to run.".to_string())?
}

/// Create any of `keys` that don't already exist as a subfolder of `root` --
/// the PC-side mirror of the NRO's own config_seed_rom_folders (source/config.c),
/// which creates a folder per known console under roms_root() on every boot.
/// The desktop Library tab (list_local_roms above) only ever listed whatever
/// subfolders happened to exist, so a console added to the app after the user
/// last picked their ROMs folder would list here (via CONSOLE_NAMES) but never
/// get an actual folder on disk -- called from renderLibrary on every refresh,
/// not just the initial folder pick, so it backfills existing installs too.
/// create_dir_all is a no-op for a folder that already exists, so this is safe
/// to call unconditionally every time.
#[tauri::command]
async fn ensure_console_folders(root: String, keys: Vec<String>) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let base = PathBuf::from(&root);
        if !base.is_dir() {
            return Err("That folder doesn't exist. Pick your ROMs folder.".into());
        }
        for key in keys {
            if key.is_empty() || key.contains(['/', '\\']) || key == "." || key == ".." {
                continue; // defensive: never used with untrusted input today, but stay off path tricks
            }
            std::fs::create_dir_all(base.join(&key))
                .map_err(|e| format!("Couldn't create {key}: {e}"))?;
        }
        Ok(())
    })
    .await
    .map_err(|_| "Folder setup task failed to run.".to_string())?
}

/// One entry from a recursive local folder walk (SD Card tab's "Upload
/// folder"). `rel` is forward-slash-separated and relative to the walked
/// root, regardless of host OS, so the frontend can split on "/" and append
/// straight onto an sdmc: path.
#[derive(Clone, Serialize)]
struct WalkEntry {
    rel: String,
    is_dir: bool,
    size: u64,
}

/// Recursively list every file and folder under a local directory (blocking —
/// call from inside spawn_blocking). Skips hidden (dotfile) entries and
/// in-progress `.part` files, same convention as list_local_roms. Each
/// directory's own entry precedes anything found inside it (a child directory
/// is queued for later expansion only after its own entry is recorded), so a
/// caller can walk the result once, front to back, and always see/create a
/// folder before it needs to put something inside it. Shared by
/// fs_walk_local_dir (the SD Card tab's per-file upload) and zip_local_dir
/// (its whole-folder-as-one-push counterpart) so they can never disagree on
/// what a folder push actually contains.
fn walk_local_dir(root: &Path) -> Result<Vec<WalkEntry>, String> {
    if !root.is_dir() {
        return Err("That folder doesn't exist.".into());
    }
    let skip = |n: &str| n.starts_with('.') || n.ends_with(".part");
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((abs, rel_prefix)) = stack.pop() {
        let mut children: Vec<_> = std::fs::read_dir(&abs)
            .map_err(|e| format!("Couldn't read {}: {e}", abs.display()))?
            .filter_map(|e| e.ok())
            .collect();
        children.sort_by_key(|e| e.file_name());
        for e in children {
            let name = e.file_name().to_string_lossy().into_owned();
            if skip(&name) {
                continue;
            }
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let rel = if rel_prefix.is_empty() {
                name
            } else {
                format!("{rel_prefix}/{name}")
            };
            if is_dir {
                out.push(WalkEntry { rel: rel.clone(), is_dir: true, size: 0 });
                stack.push((e.path(), rel));
            } else {
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                out.push(WalkEntry { rel, is_dir: false, size });
            }
        }
    }
    Ok(out)
}

/// Recursively list every file and folder under a local directory, for the SD
/// Card tab's "Upload folder" preview / per-file fallback path. Unlike
/// list_local_roms (a fixed two-level scan tuned to a ROMs library layout),
/// this walks to arbitrary depth so an upload can mirror any local folder
/// structure.
#[tauri::command]
async fn fs_walk_local_dir(dir: String) -> Result<Vec<WalkEntry>, String> {
    tauri::async_runtime::spawn_blocking(move || walk_local_dir(&PathBuf::from(&dir)))
        .await
        .map_err(|_| "Folder scan task failed to run.".to_string())?
}

/// A unique temp path for a batched-push zip, under the same haulnx-<tag>-
/// <pid>-<stamp> naming convention used elsewhere for one-off scratch files
/// (see e.g. usb_fs_read_text's preview temp).
fn temp_zip_path(tag: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("haulnx-{tag}-{}-{}.zip", std::process::id(), stamp))
}

/// Write `entries` (zip-relative path -> source file, None source = an empty
/// directory entry) to a new zip at `out_path`. Shared by fs_zip_local_dir
/// (a whole folder tree) and fs_zip_local_paths (an arbitrary flat file
/// list) so both batched-push flows agree on how the archive gets built.
/// Deflate unconditionally: cheap on the PC side, and it shrinks the one HTTP
/// body that now has to cross Wi-Fi, so there's no reason to guess per-file
/// whether compressing already-compressed game assets is "worth it".
fn write_zip(out_path: &Path, entries: &[(String, Option<PathBuf>)]) -> Result<(), String> {
    let file =
        std::fs::File::create(out_path).map_err(|e| format!("Can't create the zip: {e}"))?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (rel, src) in entries {
        match src {
            None => {
                zip.add_directory(rel.as_str(), options)
                    .map_err(|e| format!("Zip failed on {rel}: {e}"))?;
            }
            Some(p) => {
                zip.start_file(rel.as_str(), options)
                    .map_err(|e| format!("Zip failed on {rel}: {e}"))?;
                let mut f =
                    std::fs::File::open(p).map_err(|e| format!("Can't read {rel}: {e}"))?;
                std::io::copy(&mut f, &mut zip)
                    .map_err(|e| format!("Zip failed on {rel}: {e}"))?;
            }
        }
    }
    zip.finish().map_err(|e| format!("Couldn't finish the zip: {e}"))?;
    Ok(())
}

/// Bundle a whole local folder into one zip, for the SD Card tab's "Upload
/// folder": pushing a folder as one archive/one connection instead of one
/// wifi_push per file. A folder with hundreds of files used to mean hundreds
/// of back-to-back HTTP connections against the Switch's single-client
/// inventory server -- each paying its own connect/accept overhead and able
/// to race the brief window right after one push finishes before the next is
/// accepted (see wifi_push's retry comment) -- which is exactly what made a
/// large folder (a Ghostship/romhack-style asset pack, hundreds of small
/// files) look like it was hammering the console. Returns the temp zip's
/// path; the caller pushes it with wifi_push's fs_extract flag set and
/// deletes it afterward.
#[tauri::command]
async fn fs_zip_local_dir(dir: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        let root = PathBuf::from(&dir);
        let walked = walk_local_dir(&root)?;
        let entries: Vec<(String, Option<PathBuf>)> = walked
            .into_iter()
            .map(|e| {
                if e.is_dir {
                    (e.rel, None)
                } else {
                    let p = root.join(&e.rel);
                    (e.rel, Some(p))
                }
            })
            .collect();
        let out_path = temp_zip_path("sdfolder");
        write_zip(&out_path, &entries)?;
        Ok(out_path.to_string_lossy().into_owned())
    })
    .await
    .map_err(|_| "Folder zip task failed to run.".to_string())?
}

/// Bundle an arbitrary list of local files into one zip, flat (each file's
/// own basename, no relative-path nesting) -- for a batch of loose files that
/// would otherwise be one wifi_push per file against the Switch's
/// single-client server: the SD Card tab's "Add files" picker and the DAT
/// Files tab's "Push all" both use this instead of fs_zip_local_dir since
/// neither has (or needs) a single common local folder. Duplicate basenames
/// collide in the resulting zip (last one wins) since there's no folder
/// structure here to disambiguate them.
#[tauri::command]
async fn fs_zip_local_paths(paths: Vec<String>) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        let mut entries = Vec::with_capacity(paths.len());
        for p in &paths {
            let path = PathBuf::from(p);
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| format!("\"{}\" has no file name.", path.display()))?
                .to_string();
            entries.push((name, Some(path)));
        }
        let out_path = temp_zip_path("batch");
        write_zip(&out_path, &entries)?;
        Ok(out_path.to_string_lossy().into_owned())
    })
    .await
    .map_err(|_| "Batch zip task failed to run.".to_string())?
}

/// Rename a local library file or folder in place. `new_name` is treated as a
/// bare basename (any path parts are stripped) so the item can't be moved out
/// of its folder. Returns the new full path. A folder-per-game entry (see
/// list_local_roms) is just a directory here -- std::fs::rename handles both
/// file and directory renames the same way, so the only change from a
/// file-only version is the existence check up front.
#[tauri::command]
async fn rename_local(path: String, new_name: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        let src = PathBuf::from(&path);
        if !src.exists() {
            return Err("That no longer exists.".into());
        }
        let base = Path::new(&new_name)
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "Enter a valid name.".to_string())?;
        let dst = src
            .parent()
            .ok_or_else(|| "That has no parent folder.".to_string())?
            .join(base);
        if dst == src {
            return Ok(dst.to_string_lossy().into_owned());
        }
        if dst.exists() {
            return Err("Something with that name already exists here.".into());
        }
        std::fs::rename(&src, &dst).map_err(|e| format!("Rename failed: {e}"))?;
        Ok(dst.to_string_lossy().into_owned())
    })
    .await
    .map_err(|_| "Rename task failed to run.".to_string())?
}

/// Delete a local library file, or a whole folder-per-game directory (see
/// list_local_roms) with everything under it.
#[tauri::command]
async fn delete_local(path: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let p = PathBuf::from(&path);
        if p.is_dir() {
            return std::fs::remove_dir_all(&p).map_err(|e| format!("Delete failed: {e}"));
        }
        if !p.is_file() {
            return Err("That file no longer exists.".into());
        }
        std::fs::remove_file(&p).map_err(|e| format!("Delete failed: {e}"))
    })
    .await
    .map_err(|_| "Delete task failed to run.".to_string())?
}

/// Read a local text file whole (used for the Settings "auto-load collection"
/// path — a plain user-chosen file, same trust level as list_local_roms/
/// delete_local/rename_local above, which already read/write arbitrary local
/// paths this app's own user picked).
#[tauri::command]
async fn read_text_file(path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        let p = PathBuf::from(&path);
        if !p.is_file() {
            return Err("That file no longer exists.".into());
        }
        std::fs::read_to_string(&p).map_err(|e| format!("Couldn't read that file: {e}"))
    })
    .await
    .map_err(|_| "Read task failed to run.".to_string())?
}

/// Join an archive-entry name onto `dest`, refusing anything that would escape it
/// (`..`, absolute paths, drive letters) — the manual zip-slip guard used by the
/// 7z/rar extractors (the `zip` crate has `enclosed_name` for this). Returns None
/// for an unsafe or empty (directory-only) name.
fn safe_join(dest: &Path, name: &str) -> Option<PathBuf> {
    let mut out = dest.to_path_buf();
    for comp in name.split(['/', '\\']) {
        match comp {
            "" | "." => continue,
            ".." => return None,
            c if c.contains(':') => return None,
            c => out.push(c),
        }
    }
    if out == dest {
        None
    } else {
        Some(out)
    }
}

fn extract_zip(archive: &Path, dest: &Path) -> Result<Vec<PathBuf>, String> {
    let file = std::fs::File::open(archive).map_err(|e| format!("Can't open the archive: {e}"))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("Not a readable zip: {e}"))?;
    let mut files = Vec::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| format!("Bad zip entry: {e}"))?;
        // enclosed_name() returns None for any path that would escape dest.
        let out = match entry.enclosed_name() {
            Some(rel) => dest.join(rel),
            None => return Err(format!("Unsafe path in archive: {}", entry.name())),
        };
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| format!("Can't create folder: {e}"))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("Can't create folder: {e}"))?;
        }
        let mut w =
            std::fs::File::create(&out).map_err(|e| format!("Can't write {}: {e}", out.display()))?;
        std::io::copy(&mut entry, &mut w).map_err(|e| format!("Extract failed: {e}"))?;
        files.push(out);
    }
    Ok(files)
}

fn extract_7z(archive: &Path, dest: &Path) -> Result<Vec<PathBuf>, String> {
    let mut reader = sevenz_rust::SevenZReader::open(archive, sevenz_rust::Password::empty())
        .map_err(|e| format!("Not a readable 7z: {e}"))?;
    let dest = dest.to_path_buf();
    let mut files: Vec<PathBuf> = Vec::new();
    // The closure must hand a sevenz_rust::Error back to stop on error, so instead
    // we record our own failure out-of-band and halt iteration with Ok(false).
    let mut fail: Option<String> = None;
    reader
        .for_each_entries(|entry, rd| {
            let out = match safe_join(&dest, entry.name()) {
                Some(p) => p,
                None => {
                    fail = Some(format!("Unsafe path in archive: {}", entry.name()));
                    return Ok(false);
                }
            };
            if entry.is_directory() {
                if let Err(e) = std::fs::create_dir_all(&out) {
                    fail = Some(format!("Can't create folder: {e}"));
                    return Ok(false);
                }
                return Ok(true);
            }
            if let Some(parent) = out.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    fail = Some(format!("Can't create folder: {e}"));
                    return Ok(false);
                }
            }
            let mut w = match std::fs::File::create(&out) {
                Ok(w) => w,
                Err(e) => {
                    fail = Some(format!("Can't write {}: {e}", out.display()));
                    return Ok(false);
                }
            };
            if let Err(e) = std::io::copy(rd, &mut w) {
                fail = Some(format!("Extract failed: {e}"));
                return Ok(false);
            }
            files.push(out);
            Ok(true)
        })
        .map_err(|e| format!("7z extract failed: {e}"))?;
    if let Some(e) = fail {
        return Err(e);
    }
    Ok(files)
}

fn extract_rar(archive: &Path, dest: &Path) -> Result<Vec<PathBuf>, String> {
    let mut open = unrar::Archive::new(archive)
        .open_for_processing()
        .map_err(|e| format!("Not a readable rar: {e}"))?;
    let mut files = Vec::new();
    while let Some(header) = open
        .read_header()
        .map_err(|e| format!("Bad rar entry: {e}"))?
    {
        let entry = header.entry();
        let is_file = entry.is_file();
        // Guard against path traversal before letting unrar write.
        let rel = entry.filename.to_string_lossy().into_owned();
        let out = safe_join(dest, &rel);
        if is_file {
            if out.is_none() {
                return Err(format!("Unsafe path in archive: {rel}"));
            }
            open = header
                .extract_with_base(dest)
                .map_err(|e| format!("rar extract failed: {e}"))?;
            if let Some(p) = out {
                files.push(p);
            }
        } else {
            open = header.skip().map_err(|e| format!("rar read failed: {e}"))?;
        }
    }
    Ok(files)
}

/// One-click "extract & delete": unpack a .zip/.7z/.rar into its own folder, then
/// remove the archive — but only once every entry has landed. Guards against
/// path-traversal entries that would escape the target. Returns JSON
/// `{count, files}` — the number of files extracted plus their absolute paths,
/// so the download pipeline can chain optional post-extract steps onto exactly
/// what this extraction produced.
#[tauri::command]
async fn unzip_local(path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        let arc_path = PathBuf::from(&path);
        if !arc_path.is_file() {
            return Err("That file no longer exists.".into());
        }
        let ext = arc_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let dest = arc_path
            .parent()
            .ok_or_else(|| "That file has no folder.".to_string())?
            .to_path_buf();
        let files = match ext.as_str() {
            "zip" => extract_zip(&arc_path, &dest)?,
            "7z" => extract_7z(&arc_path, &dest)?,
            "rar" => extract_rar(&arc_path, &dest)?,
            _ => return Err("That isn't a .zip, .7z, or .rar file.".into()),
        };
        let count = files.len() as u32;
        let paths: Vec<String> = files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        // Everything landed — safe to drop the archive now.
        std::fs::remove_file(&arc_path)
            .map_err(|e| format!("Extracted, but couldn't delete the archive: {e}"))?;
        Ok(json!({ "count": count, "files": paths }).to_string())
    })
    .await
    .map_err(|_| "Extract task failed to run.".to_string())?
}

/// Push a local game to the Switch over Wi-Fi: a raw-body POST with an
/// X-Filename header to the inventory server, which streams it into the inbox
/// (the same path the browser ROM transfer uses). Plain HTTP on the LAN, so we
/// speak it over a raw socket with an explicit Content-Length — the receiver
/// reads exactly that many bytes and does not handle chunked encoding. Progress
/// is pushed as `push://progress` events.
#[tauri::command]
async fn wifi_push(
    app: tauri::AppHandle,
    id: String,
    path: String,
    base_url: String,
    // When set, the device installs the body as that app (overwrites its .nro
    // under sdmc:/switch) instead of dropping it in the inbox — used by the
    // Emulators-tab one-click update. Empty/absent = a normal game push.
    app_target: Option<String>,
    // The exact device path of the .nro to overwrite (sdmc:/switch/.../x.nro).
    // Takes precedence over app_target on the device, so two same-named apps in
    // different folders update independently. Empty/absent = fall back to name.
    app_path: Option<String>,
    // True for a fresh install (Emulators-tab "Install") — the app isn't on the
    // device yet, so app_path names a NEW sdmc:/switch/<file>.nro. Sent as
    // X-App-Install so the device writes it instead of rejecting a missing update
    // target. Absent/false = an update or a normal game push.
    app_install: Option<bool>,
    // True for a verification DAT push (DAT Files tab). Sent as X-Dat so the
    // device buffers it and files it into its dats folder by the DAT's own header
    // instead of dropping it in the inbox. Absent/false = a game or app push.
    is_dat: Option<bool>,
    // True for the DAT Files tab's "Push all": path is a zip of several DAT
    // files (see fs_zip_local_paths), batched into one push instead of one
    // wifi_push per DAT for the same single-client-server reason a folder
    // push is (see fs_extract below). Sent as X-Dat-Bulk so the device
    // streams and unpacks it, then files every entry the way a single X-Dat
    // push already does. Mutually exclusive with is_dat -- never set both.
    dat_bulk: Option<bool>,
    // The Library tab's console folder a game push came from (e.g. "switch"),
    // mirroring what usb_push's dest_folder already gets from the WPD side.
    // Sent as X-Dest-Folder so the device files it straight into that console
    // (extracting an archive on arrival) instead of the inbox. Empty/absent =
    // inbox, same as before this existed.
    dest_folder: Option<String>,
    // The SD Card tab's exact destination path (percent-encoded on the wire
    // as X-Fs-Path), only honored by the device when Prefs.sd_full_access is
    // on. The device also needs X-Filename (already sent below, from the
    // local file's own name) alongside it — see httpsrv.c's fs_put handling.
    // Empty/absent = every other kind of push, unchanged.
    fs_path: Option<String>,
    // Only meaningful alongside fs_path: the body is a zip of a whole folder
    // (see fs_zip_local_dir) rather than one file. Sent as X-Fs-Extract so the
    // device unpacks it into fs_path (a directory in this case, not a file)
    // on its own background extract worker instead of moving the raw zip
    // there. This is what lets the SD Card tab's "Upload folder" send
    // hundreds of files as one push instead of one wifi_push per file —
    // each of which used to pay its own connect/accept round trip against the
    // Switch's single-client server and could race the brief window right
    // after the prior push finished before the next was accepted. Absent/
    // false = fs_path names an exact file, unchanged.
    fs_extract: Option<bool>,
    // The Console Art dialog's push: the short console key (e.g. "switch")
    // this image is cover art for. Sent as X-Art-Target so the device buffers
    // it and writes it into the box-art cache under that console instead of
    // dropping it in the inbox or matching it against app_target. Empty/absent
    // = every other kind of push, unchanged.
    art_target: Option<String>,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let done = |error: Option<String>| {
            let _ = app.emit(
                "push://progress",
                PushProgress { id: id.clone(), sent: 0, total: 0, done: true, error },
            );
        };
        // One transfer attempt. Err carries (message, retryable): a connection
        // reset before/at the start of the body -- the multi-file-queue failure
        // the Switch's single-client server exhibits, where a manual retry a few
        // seconds later has always worked -- is retryable; an HTTP rejection
        // (too big, stale code) or a local file error is not.
        let attempt = || -> Result<(), (String, bool)> {
            let p = Path::new(&path);
            let name = p
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| ("That file has no name.".to_string(), false))?
                .to_string();
            let total = std::fs::metadata(p)
                .map_err(|e| (format!("Can't read the file: {e}"), false))?
                .len();

            // "http://IP:PORT/CODE" → host, port, "/CODE".
            let rest = base_url.strip_prefix("http://").unwrap_or(&base_url);
            let (authority, req_path) = match rest.find('/') {
                Some(i) => (&rest[..i], &rest[i..]),
                None => (rest, "/"),
            };
            let (host, port) = match authority.rsplit_once(':') {
                Some((h, pt)) => (h.to_string(), pt.parse::<u16>().unwrap_or(8081)),
                None => (authority.to_string(), 8081u16),
            };

            let mut stream = TcpStream::connect((host.as_str(), port))
                .map_err(|e| (format!("Couldn't reach the Switch: {e}"), true))?;
            stream.set_write_timeout(Some(Duration::from_secs(30))).ok();
            stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
            let app_hdr = match app_target.as_deref() {
                Some(t) if !t.is_empty() => format!("X-App-Target: {}\r\n", pct_encode(t)),
                _ => String::new(),
            };
            let path_hdr = match app_path.as_deref() {
                Some(t) if !t.is_empty() => format!("X-App-Path: {}\r\n", pct_encode(t)),
                _ => String::new(),
            };
            let install_hdr = if app_install.unwrap_or(false) {
                "X-App-Install: 1\r\n"
            } else {
                ""
            };
            let dat_hdr = if is_dat.unwrap_or(false) { "X-Dat: 1\r\n" } else { "" };
            let dat_bulk_hdr = if dat_bulk.unwrap_or(false) { "X-Dat-Bulk: 1\r\n" } else { "" };
            let folder_hdr = match dest_folder.as_deref() {
                Some(f) if !f.is_empty() => format!("X-Dest-Folder: {}\r\n", pct_encode(f)),
                _ => String::new(),
            };
            let fs_path_hdr = match fs_path.as_deref() {
                Some(p) if !p.is_empty() => format!("X-Fs-Path: {}\r\n", pct_encode(p)),
                _ => String::new(),
            };
            let fs_extract_hdr = if fs_extract.unwrap_or(false) { "X-Fs-Extract: 1\r\n" } else { "" };
            let art_hdr = match art_target.as_deref() {
                Some(t) if !t.is_empty() => format!("X-Art-Target: {}\r\n", pct_encode(t)),
                _ => String::new(),
            };
            let head = format!(
                "POST {req_path} HTTP/1.1\r\nHost: {host}:{port}\r\nX-Filename: {}\r\n\
                 {app_hdr}{path_hdr}{install_hdr}{dat_hdr}{dat_bulk_hdr}{folder_hdr}{fs_path_hdr}{fs_extract_hdr}{art_hdr}Content-Type: application/octet-stream\r\nContent-Length: {total}\r\n\
                 Connection: close\r\n\r\n",
                pct_encode(&name)
            );
            stream
                .write_all(head.as_bytes())
                .map_err(|e| (format!("Couldn't send to the Switch: {e}"), true))?;

            let mut file =
                std::fs::File::open(p).map_err(|e| (format!("Can't open the file: {e}"), false))?;
            let mut buf = vec![0u8; 128 * 1024];
            let mut sent: u64 = 0;
            let mut last = Instant::now();
            let _ = app.emit(
                "push://progress",
                PushProgress { id: id.clone(), sent: 0, total, done: false, error: None },
            );
            loop {
                let n = file.read(&mut buf).map_err(|e| (format!("Disk read failed: {e}"), false))?;
                if n == 0 {
                    break;
                }
                stream
                    .write_all(&buf[..n])
                    .map_err(|e| (format!("The Switch dropped the transfer: {e}"), true))?;
                sent += n as u64;
                if last.elapsed() >= Duration::from_millis(200) {
                    let _ = app.emit(
                        "push://progress",
                        PushProgress { id: id.clone(), sent, total, done: false, error: None },
                    );
                    last = Instant::now();
                }
            }
            stream.flush().ok();

            // Read the status line — the receiver's response is tiny.
            let mut resp = [0u8; 1024];
            let n = stream.read(&mut resp).unwrap_or(0);
            let code = String::from_utf8_lossy(&resp[..n])
                .split_whitespace()
                .nth(1)
                .and_then(|c| c.parse::<u16>().ok())
                .unwrap_or(0);
            if !(200..400).contains(&code) {
                // An HTTP answer -- even an error one -- means the device accepted
                // the connection and read the whole body, so a retry would reach
                // the same verdict. Not retryable. (code 0 = no status line came
                // back, but by then the body was already fully sent; re-sending
                // would just duplicate it on-device, so don't.)
                let msg = match code {
                    413 => "The Switch refused it — the file is over the receiver's limit.".to_string(),
                    403 => "The Switch refused it — reconnect (the code may be stale).".to_string(),
                    0 => "The Switch closed the connection without answering.".to_string(),
                    _ => format!("The Switch refused it (HTTP {code})."),
                };
                return Err((msg, false));
            }
            Ok(())
        };
        // The Switch's single-client inventory server has a brief window right
        // after one file completes where it isn't yet accepting the next
        // connection; a queued push landing in that window is reset before any
        // response (os error 10054 on the body write). A manual retry a few
        // seconds later has always worked, so retry a reset automatically with a
        // short backoff (0.5s, 1s, 2s) before surfacing it. Only connection-level
        // failures are retried -- attempt() flags them -- so an HTTP rejection or
        // a local file error still fails at once. See wifi-push-extract-blocking.
        let mut last_err = String::new();
        for tryn in 0..4u32 {
            if tryn > 0 {
                std::thread::sleep(Duration::from_millis(500u64 << (tryn - 1)));
                let _ = app.emit(
                    "push://progress",
                    PushProgress { id: id.clone(), sent: 0, total: 0, done: false, error: None },
                );
            }
            match attempt() {
                Ok(()) => {
                    done(None);
                    return Ok(());
                }
                Err((e, retryable)) => {
                    last_err = e;
                    if !retryable {
                        break;
                    }
                }
            }
        }
        done(Some(last_err.clone()));
        Err(last_err)
    })
    .await
    .map_err(|_| "Wi-Fi push task failed to run.".to_string())?
}

/// Push a local game to the Switch over USB (WPD write path). Lands in the named
/// console folder if present, else the Inbox. Progress as `push://progress`.
#[tauri::command]
async fn usb_push(
    app: tauri::AppHandle,
    id: String,
    path: String,
    dest_folder: String,
) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(move || {
            let emit = |sent: u64, total: u64, done: bool, error: Option<String>| {
                let _ = app.emit(
                    "push://progress",
                    PushProgress { id: id.clone(), sent, total, done, error },
                );
            };
            let total_hint = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            emit(0, total_hint, false, None);
            let res = wpd::open().and_then(|d| {
                d.push(Path::new(&path), &dest_folder, |sent, total| {
                    emit(sent, total, false, None)
                })
            });
            match res {
                Ok(()) => {
                    emit(0, 0, true, None);
                    Ok(())
                }
                Err(e) => {
                    emit(0, 0, true, Some(e.clone()));
                    Err(e)
                }
            }
        })
        .await
        .map_err(|_| "USB push task failed to run.".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (app, id, path, dest_folder);
        Err("USB is only available on Windows.".into())
    }
}

/// Open the system file manager with `path` selected, so "save to folder" can
/// jump the user straight to what they just downloaded.
#[tauri::command]
fn reveal_path(path: String) -> Result<(), String> {
    let p = Path::new(&path);
    if !p.exists() {
        return Err("That file is no longer there.".into());
    }
    #[cfg(target_os = "windows")]
    {
        // A folder: open it. A file: open its parent with the file selected.
        let mut cmd = std::process::Command::new("explorer");
        if p.is_dir() {
            cmd.arg(p);
        } else {
            cmd.arg("/select,").arg(p);
        }
        cmd.spawn().map_err(|e| e.to_string())?;
    }
    #[cfg(not(target_os = "windows"))]
    {
        let dir = if p.is_dir() { p } else { p.parent().unwrap_or(p) };
        std::process::Command::new("xdg-open")
            .arg(dir)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Save the debug bundle the desktop just pulled from the Switch (Wi-Fi:
/// GET <deviceBase>/debug_bundle.txt — the JS side fetches it directly, same
/// as it already does for dl_sources.json/update_sources.json) into the app's
/// own data folder, so a bug report never needs a manual SD-card copy. Always
/// the same filename: each pull overwrites the last, since only the most
/// recent sync matters for debugging. Returns the path it landed at.
#[tauri::command]
fn save_device_log(app: tauri::AppHandle, text: String) -> Result<String, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Couldn't find the app data folder: {e}"))?
        .join("device-logs");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
    let dest = dir.join("debug_bundle.txt");
    std::fs::write(&dest, text).map_err(|e| format!("Couldn't write the log: {e}"))?;
    Ok(dest.to_string_lossy().into_owned())
}

/// Read a local image file (from the Console Art dialog's file picker) and
/// return it as a `data:` URL. The picker only ever returns a path -- WebView2
/// doesn't expose a real filesystem path off a plain `<input type=file>` --
/// but the crop step needs actual pixels in a canvas, not just a path, so the
/// bytes have to come back from here.
#[tauri::command]
fn read_image_b64(path: String) -> Result<String, String> {
    let bytes = std::fs::read(&path).map_err(|e| format!("Couldn't read that file: {e}"))?;
    let mime = match Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" | "jfif" => "image/jpeg",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "gif" => "image/gif",
        _ => "image/png",
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:{mime};base64,{b64}"))
}

/// Write the Console Art dialog's cropped/resized PNG (built client-side on a
/// <canvas>, handed over as base64) to a temp file so it can be pushed with
/// the existing wifi_push command exactly like a user-picked file -- wifi_push
/// only knows how to stream a real path on disk. Always the same filename:
/// only one crop is ever in flight per push.
#[tauri::command]
fn save_console_art_temp(png_b64: String) -> Result<String, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(png_b64.as_bytes())
        .map_err(|e| format!("Bad image data: {e}"))?;
    let dest = std::env::temp_dir().join("haulnx_console_art_push.png");
    std::fs::write(&dest, &bytes).map_err(|e| format!("Couldn't write the cropped image: {e}"))?;
    Ok(dest.to_string_lossy().into_owned())
}

/// Read the durable settings mirror at %APPDATA%/com.haulnx.apputility/settings.json,
/// scoped by `identifier` (not by exe path or version), so every build of this
/// app -- private/public, any folder, any version -- reads the same file. This
/// backstops localStorage (the WebView2 profile), which is scoped by whatever
/// user context the exe happened to run under and isn't a place we want to be
/// the *only* copy of someone's saved credentials/preferences. Returns "{}" if
/// the file doesn't exist yet -- never an error for that, same convention as
/// self_update_check's "no update" case.
#[tauri::command]
fn settings_load(app: tauri::AppHandle) -> Result<String, String> {
    let path = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Couldn't find the app data folder: {e}"))?
        .join("settings.json");
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("{}".to_string()),
        Err(e) => Err(format!("Couldn't read {}: {e}", path.display())),
    }
}

/// Mirror the current settings blob (the JS side snapshots its own localStorage
/// keys into one JSON object) to the same file settings_load reads. Called
/// on a debounce after any localStorage write -- see the LS wrapper in
/// index.html -- so the file always trails the in-page copy by well under a
/// second, not on every keystroke.
#[tauri::command]
fn settings_save(app: tauri::AppHandle, json: String) -> Result<(), String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Couldn't find the app data folder: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
    let path = dir.join("settings.json");
    std::fs::write(&path, json).map_err(|e| format!("Couldn't write {}: {e}", path.display()))
}

/// Same pattern as settings_load/settings_save, for a separate file: a durable
/// local replica of the archive-collections doc (the same shape as an exported
/// dl_sources.json / the console's live copy), so an Import, an auto-load, or a
/// pull from the Switch is never only sitting in memory. Kept as its own file
/// rather than folded into settings.json so it can be opened/diffed on its own
/// terms -- it's already a well-known filename/shape to anyone using Import/Export.
#[tauri::command]
fn dlsources_load(app: tauri::AppHandle) -> Result<String, String> {
    let path = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Couldn't find the app data folder: {e}"))?
        .join("dl_sources.json");
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("{}".to_string()),
        Err(e) => Err(format!("Couldn't read {}: {e}", path.display())),
    }
}

#[tauri::command]
fn dlsources_save(app: tauri::AppHandle, json: String) -> Result<(), String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Couldn't find the app data folder: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
    let path = dir.join("dl_sources.json");
    std::fs::write(&path, json).map_err(|e| format!("Couldn't write {}: {e}", path.display()))
}

/// Open an http(s) URL in the user's default browser. The WebView swallows
/// `target="_blank"` navigations, so every external link routes through here.
#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("Only http(s) links can be opened.".into());
    }
    #[cfg(target_os = "windows")]
    {
        // explorer.exe, not `cmd /C start`: explorer takes the URL as a literal
        // argument with no shell reparsing. cmd.exe re-tokenizes its whole command
        // line itself, and Rust's Windows arg-quoting only guards spaces/quotes —
        // it doesn't shield &, |, ^, % from cmd's own parser, so a URL containing
        // any of those (even an ordinary "?a=1&b=2" query string) could be split
        // into multiple commands and run whatever followed the &.
        std::process::Command::new("explorer")
            .arg(&url)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new("xdg-open")
            .arg(&url)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ─── Wi-Fi device discovery ────────────────────────────────────────────────
// Find the PC's own LAN IPv4 without an interface-enumeration crate: a UDP
// socket "connected" to a public address picks the outbound interface but sends
// nothing, so its local_addr is the address the OS would route from. None when
// the PC has no usable network.
fn primary_ipv4() -> Option<[u8; 4]> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    match s.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(v4) => Some(v4.octets()),
        _ => None,
    }
}

/// True if `host:port` answers `GET /logo.png` like HaulNX's inventory server:
/// a tokenless 200 serving a PNG. The logo is served in every mode before any
/// token gate (see httpsrv.c), so it positively identifies a HaulNX device and
/// tells it apart from any other box that merely has the port open. Short
/// timeouts — this runs across a whole /24.
fn is_haulnx(host: &str, port: u16) -> bool {
    let addr = match (host, port).to_socket_addrs().ok().and_then(|mut i| i.next()) {
        Some(a) => a,
        None => return false,
    };
    let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_millis(350)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    let req = format!("GET /logo.png HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 512];
    let n = match stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => return false,
    };
    let head = &buf[..n];
    let text = String::from_utf8_lossy(head);
    let ok200 = text.starts_with("HTTP/1.") && text.contains(" 200");
    ok200
        && (text.to_ascii_lowercase().contains("image/png")
            || head.windows(4).any(|w| w == b"\x89PNG"))
}

#[derive(Serialize)]
struct FoundDevice {
    ip: String,
}

/// Scan the PC's local /24 for HaulNX's inventory server so the user never has
/// to read an IP off the TV. Probes every host on the subnet in parallel
/// (bounded to ~64 sockets at once) and keeps the ones that answer /logo.png
/// like HaulNX. Runs off the UI thread; a couple of seconds on a quiet /24.
#[tauri::command]
async fn discover_device(port: Option<u16>) -> Result<Vec<FoundDevice>, String> {
    let port = port.unwrap_or(INV_PORT);
    tauri::async_runtime::spawn_blocking(move || {
        let base = primary_ipv4().ok_or_else(|| "No local network connection found.".to_string())?;
        let mut found = Vec::new();
        // Chunked fan-out keeps the socket count bounded with no extra crates.
        for chunk in (1u16..=254).collect::<Vec<_>>().chunks(64) {
            let handles: Vec<_> = chunk
                .iter()
                .map(|&h| {
                    let ip = format!("{}.{}.{}.{}", base[0], base[1], base[2], h);
                    std::thread::spawn(move || if is_haulnx(&ip, port) { Some(ip) } else { None })
                })
                .collect();
            for hd in handles {
                if let Ok(Some(ip)) = hd.join() {
                    found.push(FoundDevice { ip });
                }
            }
        }
        Ok(found)
    })
    .await
    .map_err(|_| "Discovery task failed to run.".to_string())?
}

// ─── Companion self-update ─────────────────────────────────────────────────
// The desktop app ships as GitHub release assets tagged `app-utility-vX.Y.Z` —
// its own channel, kept apart from the device's `vX.Y.Z` NRO releases in the
// same repo, so the two version lines never collide. The check and the download
// run in Rust (ureq/rustls) to sidestep the WebView's CORS wall and the
// browser's own download UX.
// Lite builds have their own repo/release history (release-lite.sh), so the Lite
// exe must check there, not the full-build repo.
#[cfg(feature = "lite")]
const SELF_REPO: &str = "digdat0/HaulNX-lite";
#[cfg(not(feature = "lite"))]
const SELF_REPO: &str = "digdat0/HaulNX";
const SELF_TAG_PREFIX: &str = "app-utility-v";

/// Loose semver key for comparing an installed version against a release tag.
/// Parses up to three dotted leading numbers; anything else (pre-release tails)
/// is ignored, which is fine for "is this newer" ordering.
fn ver_key(s: &str) -> (u64, u64, u64) {
    let mut it = s
        .trim()
        .trim_start_matches('v')
        .split(['.', '-', '+', ' '])
        .filter_map(|p| p.parse::<u64>().ok());
    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
}

#[derive(Serialize, Default)]
struct SelfUpdate {
    current: String,
    latest: Option<String>,
    tag: Option<String>,
    notes: Option<String>,
    html_url: Option<String>,
    asset_url: Option<String>,
    asset_name: Option<String>,
}

/// Check the release channel for a newer companion build. Returns the current
/// version always, plus the newest `app-utility-v*` release's details when one
/// beats it (with a Windows installer asset attached). `latest` is None when the
/// app is up to date or the channel has no builds yet — never an error for that.
#[tauri::command]
async fn self_update_check(token: Option<String>) -> Result<SelfUpdate, String> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let agent = ureq::builder()
            .timeout_connect(Duration::from_secs(15))
            .timeout_read(Duration::from_secs(20))
            .build();
        let mut req = agent
            .get(&format!("https://api.github.com/repos/{SELF_REPO}/releases?per_page=30"))
            .set("User-Agent", "HaulNX-App-Utility")
            .set("Accept", "application/vnd.github+json");
        if let Some(t) = token.as_deref() {
            if !t.is_empty() {
                req = req.set("Authorization", &format!("Bearer {t}"));
            }
        }
        let body = match req.call() {
            Ok(r) => r.into_string().map_err(|e| e.to_string())?,
            Err(ureq::Error::Status(c, _)) => return Err(format!("GitHub returned HTTP {c}.")),
            Err(e) => return Err(format!("Couldn't reach GitHub: {e}")),
        };
        let rels: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
        let cur_key = ver_key(&current);
        let mut out = SelfUpdate { current: current.clone(), ..Default::default() };
        let mut best: Option<(u64, u64, u64)> = None;
        if let Some(arr) = rels.as_array() {
            for rel in arr {
                if rel.get("draft").and_then(|v| v.as_bool()).unwrap_or(false)
                    || rel.get("prerelease").and_then(|v| v.as_bool()).unwrap_or(false)
                {
                    continue;
                }
                let tag = rel.get("tag_name").and_then(|v| v.as_str()).unwrap_or("");
                if tag.is_empty() {
                    continue;
                }
                // release.sh tags a unified release as the bare VERSION (e.g. "2.1.10"),
                // shared with the NRO — there's no separate "app-utility-v*" channel in
                // practice. Accept that bare form, plus "vX.Y.Z" and the old prefixed
                // scheme, so a real tag is never silently skipped.
                let ver = tag
                    .strip_prefix(SELF_TAG_PREFIX)
                    .or_else(|| tag.strip_prefix('v'))
                    .unwrap_or(tag);
                let key = ver_key(ver);
                if key <= cur_key || best.map_or(false, |b| key <= b) {
                    continue;
                }
                // Only surface a release we can actually install: a Windows binary.
                let asset = rel.get("assets").and_then(|a| a.as_array()).and_then(|arr| {
                    arr.iter().find(|a| {
                        let n = a
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_ascii_lowercase();
                        n.ends_with(".exe") || n.ends_with(".msi")
                    })
                });
                let (aurl, aname) = match asset {
                    Some(a) => (
                        a.get("browser_download_url").and_then(|v| v.as_str()).map(str::to_string),
                        a.get("name").and_then(|v| v.as_str()).map(str::to_string),
                    ),
                    None => continue,
                };
                best = Some(key);
                out.latest = Some(ver.trim().trim_start_matches('v').to_string());
                out.tag = Some(tag.to_string());
                out.notes = rel.get("body").and_then(|v| v.as_str()).map(str::to_string);
                out.html_url = rel.get("html_url").and_then(|v| v.as_str()).map(str::to_string);
                out.asset_url = aurl;
                out.asset_name = aname;
            }
        }
        Ok(out)
    })
    .await
    .map_err(|_| "Update check task failed to run.".to_string())?
}

/// Download the update to the OS temp dir. A `.msi` is handed to Windows'
/// installer UI (the running app stays up; the installer replaces it once the
/// user confirms in its window). Anything else — our normal release asset,
/// `HaulNX-AppUtility.exe`, a plain portable binary with no installer — is
/// applied in place: rename the running exe aside, copy the new one over its
/// path, relaunch it, then exit. Windows allows renaming (though not deleting)
/// a running executable's file, since the loader keeps the old data reachable
/// through the already-open image section — that's what makes this safe without
/// a helper process. Progress rides `selfupd://progress` events. User-initiated
/// only.
#[tauri::command]
async fn self_update_run(
    app: tauri::AppHandle,
    url: String,
    name: Option<String>,
) -> Result<String, String> {
    if !url.starts_with("https://") {
        return Err("Refusing a non-HTTPS update URL.".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let emit = |received: u64, total: u64, done: bool, error: Option<String>| {
            let _ = app.emit(
                "selfupd://progress",
                json!({"received": received, "total": total, "done": done, "error": error}),
            );
        };
        let fname = safe_name(name.as_deref().unwrap_or("HaulNX-App-Utility-Setup.exe"));
        let dest = std::env::temp_dir().join(&fname);
        let agent = ureq::builder()
            .timeout_connect(Duration::from_secs(30))
            .timeout_read(Duration::from_secs(120))
            .redirects(8) // GitHub redirects asset URLs to its object CDN.
            .build();
        emit(0, 0, false, None);
        let resp = match agent.get(&url).set("User-Agent", "HaulNX-App-Utility").call() {
            Ok(r) => r,
            Err(ureq::Error::Status(c, _)) => {
                let e = format!("GitHub returned HTTP {c}.");
                emit(0, 0, true, Some(e.clone()));
                return Err(e);
            }
            Err(e) => {
                let e = format!("Download failed: {e}");
                emit(0, 0, true, Some(e.clone()));
                return Err(e);
            }
        };
        let total: u64 = resp.header("Content-Length").and_then(|s| s.parse().ok()).unwrap_or(0);
        let mut reader = resp.into_reader();
        let mut file = match std::fs::File::create(&dest) {
            Ok(f) => f,
            Err(e) => {
                let e = format!("Can't write the installer: {e}");
                emit(0, 0, true, Some(e.clone()));
                return Err(e);
            }
        };
        let mut buf = vec![0u8; 128 * 1024];
        let mut received: u64 = 0;
        let mut last = Instant::now();
        loop {
            let n = match reader.read(&mut buf) {
                Ok(n) => n,
                Err(e) => {
                    let e = format!("Download interrupted: {e}");
                    emit(0, 0, true, Some(e.clone()));
                    return Err(e);
                }
            };
            if n == 0 {
                break;
            }
            if file.write_all(&buf[..n]).is_err() {
                let e = "Disk write failed.".to_string();
                emit(0, 0, true, Some(e.clone()));
                return Err(e);
            }
            received += n as u64;
            if last.elapsed() >= Duration::from_millis(200) {
                emit(received, total, false, None);
                last = Instant::now();
            }
        }
        file.flush().ok();
        drop(file);
        let total = total.max(received);

        let is_installer =
            dest.extension().map(|e| e.eq_ignore_ascii_case("msi")).unwrap_or(false);
        if is_installer {
            let path = dest.to_string_lossy().into_owned();
            emit(received, total, true, None);
            // Hand off to the installer; the user drives its UI from here.
            #[cfg(target_os = "windows")]
            {
                // explorer.exe, not `cmd /C start` — see open_url's comment for why.
                std::process::Command::new("explorer")
                    .arg(&path)
                    .spawn()
                    .map_err(|e| format!("Downloaded, but couldn't launch the installer: {e}"))?;
            }
            return Ok(path);
        }

        // Portable exe: replace ourselves in place, on Windows only — everywhere
        // else this build ships as a Windows-only companion (see open_url's
        // xdg-open branch, which is unreachable dead code kept only for parity).
        #[cfg(not(target_os = "windows"))]
        {
            return Err("Self-replace is only implemented on Windows.".into());
        }
        #[cfg(target_os = "windows")]
        {
            let cur = std::env::current_exe()
                .map_err(|e| format!("Couldn't find my own exe path: {e}"))?;
            let old = cur.with_file_name(format!(
                "{}.old",
                cur.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
            ));
            let _ = std::fs::remove_file(&old); // stale leftover from a prior update, if any

            // Rename the *running* exe aside — allowed on Windows even while its
            // image is mapped and executing, unlike a delete or in-place write.
            std::fs::rename(&cur, &old).map_err(|e| {
                format!(
                    "Couldn't update in place ({e}). Try running HaulNX-AppUtility.exe from a \
                     folder you have write access to (not Program Files)."
                )
            })?;

            // Copy (not rename) across into the original path — the temp download
            // may be on a different drive than the install location.
            if let Err(e) = std::fs::copy(&dest, &cur) {
                let _ = std::fs::remove_file(&cur); // clean up any partial copy
                let _ = std::fs::rename(&old, &cur); // restore the working exe
                let e = format!("Couldn't install the update ({e}). The app is unchanged.");
                emit(received, total, true, Some(e.clone()));
                return Err(e);
            }
            let _ = std::fs::remove_file(&dest);

            let _ = app.emit(
                "selfupd://progress",
                json!({"received": received, "total": total, "done": true, "restarting": true}),
            );
            // Give the progress event a moment to reach the webview before the
            // process (and the webview with it) goes away.
            std::thread::sleep(Duration::from_millis(300));

            if let Err(e) = std::process::Command::new(&cur).spawn() {
                // The new exe is in place either way — just couldn't auto-launch it.
                return Err(format!(
                    "Updated, but couldn't relaunch automatically: {e}\n\nOpen \
                     HaulNX-AppUtility.exe yourself to finish."
                ));
            }
            app.exit(0);
            Ok(cur.to_string_lossy().into_owned())
        }
    })
    .await
    .map_err(|_| "Update task failed to run.".to_string())?
}

// Best-effort cleanup of the `<name>.exe.old` self_update_run leaves behind
// next to the exe. It can't delete that file at update time (its own old
// process may still be exiting and holding it), so each new launch sweeps for
// one and clears it once nothing else has it open; a locked leftover is
// silently skipped and retried on the next launch.
fn cleanup_old_self_update() {
    if let Ok(cur) = std::env::current_exe() {
        if let Some(name) = cur.file_name().map(|n| n.to_string_lossy().into_owned()) {
            let old = cur.with_file_name(format!("{name}.old"));
            let _ = std::fs::remove_file(old);
        }
    }
}

fn main() {
    cleanup_old_self_update();
    // WebView2's disk HTTP cache persists in %LOCALAPPDATA%\<identifier>\EBWebView
    // across app restarts and even across a full exe rebuild (it's keyed by the
    // resource's virtual URL, not by binary content or version) -- without this,
    // shipping a frontend fix can silently keep serving the old index.html/JS to
    // every user until someone thinks to manually clear that folder. Disabling
    // the HTTP cache outright is the supported fix (must be set before the
    // webview is created) and costs nothing here: this app has no heavy
    // repeat-fetch assets worth caching.
    std::env::set_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", "--disable-http-cache");
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            app_info,
            usb_switches,
            net_probe,
            download_file,
            cancel_download,
            usb_inventory,
            usb_collection,
            usb_credentials,
            usb_update_sources,
            usb_push_update_sources,
            usb_debug_log,
            usb_consoleart,
            usb_queue_status,
            usb_download,
            usb_delete,
            usb_rename,
            usb_move,
            usb_push,
            usb_fs_list,
            usb_fs_download,
            usb_fs_read_text,
            fs_walk_local_dir,
            fs_zip_local_dir,
            fs_zip_local_paths,
            usb_fs_delete,
            usb_fs_rename,
            usb_fs_mkdir,
            usb_fs_upload,
            usb_fs_upload_bulk,
            usb_push_console_art,
            usb_boxart_search,
            usb_boxart_pick,
            usb_boxart_status,
            usb_boxart_thumb,
            list_local_roms,
            ensure_console_folders,
            rename_local,
            delete_local,
            read_text_file,
            unzip_local,
            #[cfg(has_ext_ops)]
            ext_ops::convert_rom,
            wifi_push,
            read_image_b64,
            save_console_art_temp,
            discover_device,
            self_update_check,
            self_update_run,
            reveal_path,
            open_url,
            save_device_log,
            settings_load,
            settings_save,
            dlsources_load,
            dlsources_save
        ])
        .run(tauri::generate_context!())
        .expect("error while running HaulNX App Utility");
}
