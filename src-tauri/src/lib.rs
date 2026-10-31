//! Metaphyuni玄熊猫P1管理器 — a native control panel for the Metaphyuni P1 Ultra 4K.
//!
//! Everything the UI shows comes from this module: the receiver is opened once
//! (see `hid.rs`), the 192-byte configuration block is decoded (`protocol.rs`),
//! and writes go out as `SetEEPROM` frames followed by a commit frame, which is
//! the only write form this firmware actually persists.
//!
//! Nothing is cached between calls, so a value the UI shows is a value the device
//! reported. The only state kept is the open handle.

mod ble;
mod env;
mod hid;
mod protocol;

use std::sync::atomic::{AtomicU16, AtomicU8, Ordering};
use std::sync::Mutex;
use std::thread::sleep;
use std::time::Duration;

use serde::Serialize;

use hid::{Battery, Link};
use hidapi::HidApi;
use protocol::{
    decode, encode, Snapshot, StatePatch, ACTIVE_STAGE_ADDR, CONFIG_END, KEY_CLASS_COMBO,
    KEY_CLASS_MACRO,
};

/// One open handle. Serialised on purpose: the receiver has a single command
/// channel and answers one request at a time, so concurrent reads would race.
static LINK: Mutex<Option<Link>> = Mutex::new(None);

/// When the cached handle was last thrown away and reopened.
///
/// Reopening means `HidApi::new()` — a walk over every HID device on the
/// machine. That is fine once in a while, but a mouse that is switched off or
/// sitting on the Bluetooth channel makes *every* call fail, so without a floor
/// on this interval the app would re-enumerate the whole HID tree as fast as the
/// UI polls.
static LAST_RECONNECT: Mutex<Option<std::time::Instant>> = Mutex::new(None);

const RECONNECT_FLOOR: Duration = Duration::from_secs(2);

fn with_link<T>(f: impl Fn(&Link) -> Result<T, String>) -> Result<T, String> {
    with_link_inner(f, true)
}

/// Same as `with_link` but keeps the cached handle when the call fails.
///
/// Used by everything the UI polls. A silent mouse is a normal state, not a
/// reason to reopen the device, and the reconnecting variant is still reachable
/// through the explicit user actions (read, apply, forget).
fn with_link_poll<T>(f: impl Fn(&Link) -> Result<T, String>) -> Result<T, String> {
    with_link_inner(f, false)
}

fn with_link_inner<T>(
    f: impl Fn(&Link) -> Result<T, String>,
    reconnect_on_error: bool,
) -> Result<T, String> {
    let mut guard = LINK
        .lock()
        .map_err(|_| "internal error: the device lock was poisoned".to_string())?;
    if guard.is_none() {
        *guard = Some(Link::connect()?);
    }
    let err = match guard.as_ref() {
        Some(link) => match f(link) {
            Ok(value) => return Ok(value),
            Err(e) => {
                if !reconnect_on_error {
                    return Err(e);
                }
                e
            }
        },
        None => {
            let e = "no device handle".to_string();
            if !reconnect_on_error {
                return Err(e);
            }
            e
        }
    };

    // The cached handle is stale (the receiver was replugged); open it again —
    // but not more often than the floor, or a silent mouse turns every failed
    // call into a full device enumeration.
    {
        let mut last = LAST_RECONNECT
            .lock()
            .map_err(|_| "internal error: the reconnect clock was poisoned".to_string())?;
        let now = std::time::Instant::now();
        if let Some(then) = *last {
            if now.duration_since(then) < RECONNECT_FLOOR {
                return Err(err);
            }
        }
        *last = Some(now);
    }

    *guard = None;
    let link = Link::connect()?;
    let result = f(&link);
    *guard = Some(link);
    result
}

// ── shapes the UI consumes ───────────────────────────────────────────────

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub product_id: u16,
    /// Maker, separate from the model: "玄熊猫".
    pub brand: String,
    pub name: String,
    pub mode: String,
    pub receiver: bool,
    /// Fastest report rate this product carries, straight from the catalogue.
    pub max_rate_hz: u32,
    /// Rates that may actually be written to this endpoint, in Hz. Narrower
    /// than `max_rate_hz`: those are the ones measured on it.
    pub report_rates: Vec<u32>,
    pub vendor_id: u16,
    /// Mouse firmware version, when the receiver answers `GetVersion`.
    pub firmware_mouse: Option<String>,
    /// Receiver/dongle firmware version.
    pub firmware_dongle: Option<String>,
    /// The mouse MCU the vendor config names for this model, e.g. "CX52850P".
    /// Firmware images are matched against it, so the UI shows it.
    pub mcu_mouse: &'static str,
    /// The receiver MCU, e.g. "CX52650N".
    pub mcu_dongle: &'static str,
    /// The receiver's own ids. A firmware image carries the CID/MID it is built
    /// for, so the UI needs ours to compare against.
    pub cid: Option<u8>,
    pub mid: Option<u8>,
    /// The 4K receiver's light, present only when one answered 0x15.
    /// `None` is the honest answer for a 1K receiver or a wired mouse: the UI
    /// hides the control instead of showing three empty swatches.
    pub dongle_rgb: Option<DongleRgbView>,
    /// Device type byte from the 0x01 id block. Information only — see
    /// [`protocol::DEVICE_TYPE_INDEX`] for why nothing branches on it.
    pub device_type: Option<u8>,
    /// Whether long range mode is on. `None` when the device does not answer
    /// 0x17, which is exactly how the driver decides not to offer it.
    pub long_range: Option<bool>,
    /// Live onboard profile index. `None` means one profile and no picker.
    pub current_config: Option<u8>,
    /// How many profiles the picker should offer, 0 when there is no picker.
    pub profile_count: usize,
}

/// The receiver light as the UI sees it.
///
/// Colours travel as `#rrggbb` so the front end can hand them to a colour
/// input without doing arithmetic of its own.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DongleRgbView {
    pub mode: u8,
    pub mode_label: &'static str,
    pub color1: String,
    pub color2: String,
    pub color3: String,
}

impl DongleRgbView {
    fn of(rgb: protocol::DongleRgb) -> Self {
        Self {
            mode: rgb.mode,
            mode_label: protocol::DongleRgb::mode_label(rgb.mode),
            color1: format!("#{}", protocol::DongleRgb::hex(rgb.color1)),
            color2: format!("#{}", protocol::DongleRgb::hex(rgb.color2)),
            color3: format!("#{}", protocol::DongleRgb::hex(rgb.color3)),
        }
    }
}

#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub online: bool,
    /// Full battery state when command 0x04 answered.
    pub battery: Option<Battery>,
    /// Remaining charge, kept for callers that only want the percentage.
    pub battery_level: Option<u8>,
    /// Active DPI stage, 1-based, as the device stores it.
    pub dpi_index: Option<u8>,
    /// CPI of the active stage.
    pub dpi: Option<u32>,
    /// Raw byte at 0x0000 (see `report_rate_label` in the UI).
    pub rate_byte: Option<u8>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Applied {
    pub snapshot: Snapshot,
    /// Records actually written (0 when the patch changed nothing).
    pub frames: usize,
}

/// Full 192-byte block as hex, for backup/export.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BlockDump {
    pub hex: String,
    pub snapshot: Snapshot,
}

// ── commands ─────────────────────────────────────────────────────────────

#[tauri::command]
fn device_info() -> Result<DeviceInfo, String> {
    with_link(|link| {
        // One version comes over the wire, the other from the USB descriptor:
        //   0x12 -> the mouse firmware (needs the mouse awake)
        //   bcdDevice -> the receiver's own firmware
        // 0x12 is from the vendor DLL's own export table (`CS_UsbServer_
        // ReadVersion`), confirmed against hardware.
        //
        // No wake attempt here on purpose: opening must be instant so the window
        // appears at once. A sleeping mouse simply leaves its version empty, and
        // the UI offers a wake button instead of blocking for ten seconds.
        let firmware_mouse = read_version(link, protocol::cmd::VERSION_MOUSE);
        // The receiver's own firmware is its USB device release number. There is
        // no protocol command for it: the vendor's export table has ReadVersion
        // (0x12) for the mouse and GetSlaveVersion (0xb3) for whatever sits on
        // the far end of the link — the receiver never describes itself. So the
        // revision comes from bcdDevice, which is where every USB device says
        // its own firmware, and it is available without waking anything.
        let firmware_dongle = format_release(link.release);
        let product = identity(link.product_id);
        let ids = link.identity();
        // Remember the ids so a firmware image can be checked against them
        // later without another round trip.
        let cid = ids.map(|i| {
            LAST_CID.store(i.cid, Ordering::Relaxed);
            i.cid
        });
        let mid = ids.map(|i| {
            LAST_MID.store(i.mid, Ordering::Relaxed);
            i.mid
        });
        LAST_PID.store(link.product_id, Ordering::Relaxed);
        // Probed, not looked up: only a 4K receiver answers, and it costs one
        // round trip on the way in. A receiver without the light simply leaves
        // this `None`, so no catalogue entry has to guess.
        let dongle_rgb = link.read_dongle_rgb().map(DongleRgbView::of);
        // Same "probe, not look up" rule as the light: a device that has no
        // long-range toggle or no profiles simply stays silent, and the UI
        // hides the control. Nothing in the catalogue has to guess which models
        // have them.
        let long_range = link.long_range();
        let current_config = link.current_config();
        let profile_count = if current_config.is_some() { protocol::ONBOARD_PROFILES } else { 0 };
        Ok(DeviceInfo {
            product_id: link.product_id,
            // Brand and model travel separately so the UI can compose them:
            // a P1 Pro Max receiver makes the header say so.
            brand: product.brand.to_string(),
            name: product.model.to_string(),
            mode: product.mode.to_string(),
            receiver: product.receiver,
            max_rate_hz: product.max_rate_hz,
            report_rates: product.rates.to_vec(),
            vendor_id: protocol::VENDOR_ID,
            firmware_mouse,
            firmware_dongle,
            mcu_mouse: product.mcu_mouse,
            mcu_dongle: product.mcu_dongle,
            cid,
            mid,
            dongle_rgb,
            device_type: ids.map(|i| i.device_type),
            long_range,
            current_config,
            profile_count,
        })
    })
}

/// PID -> the catalogue row. All naming decisions live in
/// [`protocol::PRODUCTS`] now; this only adapts them to the old tuple shape
/// the info struct is built from.
fn identity(pid: u16) -> &'static protocol::Product {
    protocol::product_for(pid)
}

/// Ask the device for one of its version counters. Read-only and best effort:
/// a receiver that does not answer simply leaves the field empty.
fn read_version(link: &Link, cmd: u8) -> Option<String> {
    let reply = link.command(cmd, 0x0000, &[], 500)?;
    if reply.len() < 2 {
        return None;
    }
    // The bytes are printed as hex digits, the way the vendor app shows them:
    // 0x0121 -> "v1.21", 0x0126 -> "v1.26".
    let value = ((reply[0] as u16) << 8) | reply[1] as u16;
    // 0x0000 is what a device that does not carry this counter answers; showing
    // it as "v0.00" reads like a broken read rather than an absent one.
    if value == 0 {
        return None;
    }
    Some(format!("v{:x}.{:02x}", value >> 8, value & 0xff))
}

// ── firmware images ──────────────────────────────────────────────────────
//
// The vendor's upgrade path is entirely file-driven. An image is a `.bin` whose
// header carries the IC name, the CID/MID it is for, its version, the HID paths
// of the bootloader, AND the three commands used to drive it
// (`resetToUpdateModeCmd`, `prepareDownLoadCmd`, `dataDownLoadCmd`). The DLL
// reads the commands out of the file rather than knowing them itself, which is
// why `FindBootDevices` takes the whole image as an argument.
//
// Recovered from `Mouse Drive Beta.exe` (a .NET binary) — `UpgradeFileHeader`
// is a sequential-layout struct marshalled straight out of the file.

/// A vendor firmware image, parsed far enough to say what it is and whether it
/// belongs on the device that is plugged in.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FirmwareImage {
    pub file_name: String,
    pub file_size: usize,
    /// Version as the header stores it, rendered the way the app shows versions.
    pub version: String,
    /// 1 = mouse, 2 = receiver/dongle. Header field `DeciveType` (sic).
    pub device_type: u8,
    pub device_type_label: &'static str,
    pub cid: u8,
    pub mid: u8,
    pub ic_name: String,
    pub product_name: String,
    pub sensor_name: String,
    pub firmware_length: usize,
    pub header_length: u32,
    pub head_crc: u32,
    /// HID path of the bootloader's input endpoint, from the header. This is
    /// how `FindBootDevices` recognises the device once it has rebooted.
    pub boot_input_endpoint: String,
    pub boot_output_endpoint: String,
    /// The command that puts the device into update mode. It lives in the file,
    /// so there is no single correct hard-coded value across products.
    pub reset_to_update_mode_cmd: String,
    pub prepare_download_cmd: String,
    pub data_download_cmd: String,
    /// Does this image match the device that is currently connected?
    pub compatible: bool,
    /// Plain-language reason when it does not. Flashing an image meant for
    /// another IC is the classic way to brick one of these.
    pub compatibility: String,
}

/// Read a NUL-padded fixed string out of the header.
///
/// The image is a `.bin`, so these are raw bytes and a copy-pasted file can
/// contain almost anything — every read is bounds-checked and lossy-decoded
/// rather than trusted.
fn header_str(bytes: &[u8], range: std::ops::Range<usize>) -> String {
    let slice = bytes.get(range).unwrap_or(&[]);
    let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    String::from_utf8_lossy(&slice[..end]).trim().to_string()
}

/// Show one of the header's command slots.
///
/// The vendor's own code hands these fields to functions taking `string`, so in
/// at least some images they hold hex as text ("08 0d 00 …"). They may also be
/// raw frames. Print whichever it is rather than guessing: readable bytes are
/// shown as text with the hex beside them.
fn cmd_display(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let slice = &bytes[..end];
    if slice.is_empty() {
        return "（空）".to_string();
    }
    // Only the first 16 bytes are echoed: a 64-byte slot full of hex text is
    // unreadable, and the point is to recognise the shape, not to copy it out.
    if slice.iter().all(|&b| (0x20..0x7f).contains(&b)) {
        format!(
            "{}   [hex {}]",
            String::from_utf8_lossy(slice),
            hex(&slice[..slice.len().min(16)])
        )
    } else {
        hex(&slice[..slice.len().min(16)])
    }
}

/// Parse a vendor firmware image's header and judge it against the device
/// that is plugged in.
///
/// Takes the bytes rather than a path: the WebView deliberately does not hand
/// out real file paths, and only the header is needed anyway, so the UI reads
/// the first kilobyte and sends that.
fn parse_image(name: String, size: u64, bytes: &[u8]) -> Result<FirmwareImage, String> {
    if bytes.len() < MIN_HEADER {
        return Err(format!(
            "这个文件只有 {} 字节，不是完整的固件包（固件头本身就要 {} 字节以上）。",
            bytes.len(),
            MIN_HEADER
        ));
    }

    let u32_at = |off: usize| -> u32 {
        u32::from_le_bytes([
            *bytes.get(off).unwrap_or(&0),
            *bytes.get(off + 1).unwrap_or(&0),
            *bytes.get(off + 2).unwrap_or(&0),
            *bytes.get(off + 3).unwrap_or(&0),
        ])
    };
    // Render one of the 64-byte command slots, text or raw bytes as the case
    // may be.
    let cmd_at = |off: usize| cmd_display(bytes.get(off..off + 64).unwrap_or(&[]));

    // UpgradeFileHeader, sequential layout:
    //   headCRC, headLength, fwLength, nextFileAddress, version  (5 × u32)
    //   DeciveType, Cid, Mid                                     (3 × u8)
    //   then eight 64-byte NUL-padded strings.
    let head_crc = u32_at(0);
    let header_length = u32_at(4);
    let fw_length = u32_at(8) as usize;
    let version = u32_at(16);
    let device_type = bytes[20];
    let cid = bytes[21];
    let mid = bytes[22];
    // The nine fixed strings follow the 20-byte scalar block, 64 bytes each,
    // in the order the marshalled struct declares them.
    const STR_BASE: usize = 23;
    let str_at = |i: usize| header_str(&bytes, STR_BASE + i * 64..STR_BASE + (i + 1) * 64);
    let ic_name = str_at(1);

    // The version word is packed BCD the same way the device reports its own.
    let version_text = format!("v{:x}.{:02x}", version >> 8, version & 0xff);

    // The type field is not to be trusted. The marshalled struct implies 1 and
    // 2, but the one image family we could actually measure — Pulsar, built by
    // the same ODM toolchain — carries 210 and 211 instead. There is no
    // Metaphyuni image to settle which encoding this product uses.
    let device_type_label = match device_type {
        1 | 210 => "鼠标",
        2 | 211 => "接收器",
        _ => "未知类型",
    };

    // Judge it against whatever is plugged in right now. A device that is not
    // answering still gets a partial check: the IC name alone catches the
    // dangerous case of flashing a receiver image to a mouse.
    //
    // The IC name decides the target, not the type field — the IC is what the
    // header states unambiguously, and matching either of this product's two
    // chips tells us which end the image is for.
    let product = protocol::product_for(LAST_PID.load(Ordering::Relaxed));
    let as_mouse = !ic_name.is_empty() && ic_name.eq_ignore_ascii_case(product.mcu_mouse);
    let as_dongle = !ic_name.is_empty() && ic_name.eq_ignore_ascii_case(product.mcu_dongle);
    let ic_ok = as_mouse || as_dongle;
    // Which end the image is for, for the readout. When the IC matches neither
    // chip there is nothing to name, and the message says so instead.
    let target_label = if as_dongle {
        "接收器"
    } else if as_mouse {
        "鼠标"
    } else {
        "（未知端）"
    };
    let seen_cid = LAST_CID.load(Ordering::Relaxed);
    let seen_mid = LAST_MID.load(Ordering::Relaxed);
    // 0 means "never read", not "cid 0" — in that case say so instead of
    // failing the check on a value we simply do not have.
    let ids_known = seen_cid != 0 || seen_mid != 0;
    let cid_ok = !ids_known || cid == seen_cid;
    let mid_ok = !ids_known || mid == seen_mid;

    let (compatible, compatibility) = if !ic_ok {
        (
            false,
            format!(
                "不适用：这个固件是给 {ic_name} 的，而「{}」的鼠标用 {}、接收器用 {}。",
                product.model, product.mcu_mouse, product.mcu_dongle
            ),
        )
    } else if !cid_ok || !mid_ok {
        (
            false,
            format!(
                "不适用：固件要求 CID {cid} / MID {mid}，本机读到的是 CID {seen_cid} / MID {seen_mid}。"
            ),
        )
    } else if ids_known {
        (
            true,
            format!(
                "与当前设备匹配：{target_label}固件（{ic_name} · CID {cid} · MID {mid}）。",
            ),
        )
    } else {
        (
            true,
            format!(
                "{target_label}芯片一致（{ic_name}）。还没有读到本机的 CID/MID，无法核对 ID —— 读一次设备信息后再确认。"
            ),
        )
    };

    Ok(FirmwareImage {
        file_name: name,
        file_size: size as usize,
        version: version_text,
        device_type,
        device_type_label,
        cid,
        mid,
        ic_name,
        product_name: str_at(10),
        sensor_name: str_at(9),
        firmware_length: fw_length,
        header_length,
        head_crc,
        boot_input_endpoint: str_at(2),
        boot_output_endpoint: str_at(3),
        reset_to_update_mode_cmd: cmd_at(STR_BASE + 6 * 64),
        prepare_download_cmd: cmd_at(STR_BASE + 7 * 64),
        data_download_cmd: cmd_at(STR_BASE + 8 * 64),
        compatible,
        compatibility,
    })
}

/// The upgrade header is 23 scalars plus eleven 64-byte strings. Anything
/// shorter cannot be a firmware image.
const MIN_HEADER: usize = 23 + 11 * 64;

#[tauri::command]
fn parse_firmware(name: String, size: u64, header: Vec<u8>) -> Result<FirmwareImage, String> {
    parse_image(name, size, &header)
}

// ── flashing: the vendor's own 32-bit upgrade engine ────────────────────
//
// The CompX upgrade stack (HIDUsb.dll) ships 32-bit only, and this
// executable is 64-bit, so the flash runs in a small helper process that
// loads the vendor DLL and streams one JSON event per line on stdout.
// Everything is embedded in this binary and staged into a temp dir on
// first use; the vendor DLL itself performs every flash step, which keeps
// the wire protocol exactly the one the devices were built against.

const HELPER_BIN: &[u8] = include_bytes!("../resources/upgrade-helper.exe");
const VENDOR_DLL: &[u8] = include_bytes!("../resources/HIDUsb.dll");
/// Not the real debug runtime. The vendor's HIDUsb.dll asks ucrtbased.dll for
/// ten plain C functions, every one of which the OS already provides in
/// ucrtbase.dll — so this is a 5 KB forwarder standing in for the 1.7 MB debug
/// runtime the vendor shipped. See `helper/ucrtbased.def` for the reasoning and
/// the one thing that makes it safe.
const CRT_UCRTBASED: &[u8] = include_bytes!("../resources/ucrtbased.dll");
const CRT_MSVCP140: &[u8] = include_bytes!("../resources/msvcp140.dll");
const CRT_VCRUNTIME140: &[u8] = include_bytes!("../resources/vcruntime140.dll");

/// Write the helper payload if it is not already staged with the right size.
fn stage_file(dir: &std::path::Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let path = dir.join(name);
    let stale = std::fs::metadata(&path)
        .map(|m| m.len() as usize != bytes.len())
        .unwrap_or(true);
    if stale {
        std::fs::write(&path, bytes).map_err(|e| format!("无法释放 {name}：{e}"))?;
    }
    Ok(())
}

fn stage_upgrade_env() -> Result<std::path::PathBuf, String> {
    let dir = std::env::temp_dir().join("metaphyuni-hub-fw");
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法准备升级工作目录：{e}"))?;
    stage_file(&dir, "upgrade-helper.exe", HELPER_BIN)?;
    stage_file(&dir, "HIDUsb.dll", VENDOR_DLL)?;
    stage_file(&dir, "ucrtbased.dll", CRT_UCRTBASED)?;
    stage_file(&dir, "msvcp140.dll", CRT_MSVCP140)?;
    stage_file(&dir, "vcruntime140.dll", CRT_VCRUNTIME140)?;
    // vcruntime140_1.dll was staged too until measurements showed the load
    // self-test passes without it: nothing in HIDUsb.dll's own imports, nor in
    // msvcp140.dll's, asks for it. CHECKED, not assumed — see the `load` mode
    // in upgrade-helper.c, which is how that was established.
    Ok(dir)
}

/// What `firmware_probe` hands back: the vendor DLL's own answer to "is
/// there exactly one device waiting in boot mode for this image?"
#[derive(serde::Serialize)]
struct FwProbe {
    boot_count: u32,
    boot_devices: Vec<String>,
    logs: Vec<String>,
}

/// Run the helper with the given mode and collect its stdout lines.
/// `on_line` receives each JSON line as it arrives; for probe there is
/// nothing to stream, for flash the caller forwards them as UI events.
fn run_helper(
    dir: &std::path::Path,
    mode: &str,
    bin_path: &std::path::Path,
    timeout_ms: Option<u32>,
    mut on_line: impl FnMut(String),
) -> Result<i32, String> {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(dir.join("upgrade-helper.exe"));
    cmd.arg(mode).arg(bin_path);
    if let Some(ms) = timeout_ms {
        cmd.arg(ms.to_string());
    }
    cmd.current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("无法启动升级辅助进程：{e}"))?;
    if let Some(out) = child.stdout.take() {
        let reader = BufReader::new(out);
        for line in reader.lines() {
            match line {
                Ok(l) if !l.trim().is_empty() => on_line(l),
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
    let status = child
        .wait()
        .map_err(|e| format!("等待升级辅助进程结束失败：{e}"))?;
    Ok(status.code().unwrap_or(-1))
}

/// Parse a staged image one more time and refuse anything the parser did not
/// judge compatible. The header bytes are what the vendor DLL will act on, so
/// the check has to pass before that DLL ever sees them.
///
/// The verdict is `parse_image`'s own rather than a second test: duplicating a
/// safety check is how the two copies end up disagreeing, and the parser's is
/// the stricter one — it also compares CID/MID when they are known.
fn verify_image(bytes: &[u8]) -> Result<FirmwareImage, String> {
    let img = parse_image("firmware.bin".into(), bytes.len() as u64, bytes)?;
    if !img.compatible {
        return Err(format!("拒绝刷入：{}", img.compatibility));
    }
    Ok(img)
}

/// Ask the vendor engine which boot devices match this image. Read-only:
/// the DLL parses the header and lists devices, nothing is written.
#[tauri::command]
fn firmware_probe(bytes: Vec<u8>) -> Result<FwProbe, String> {
    verify_image(&bytes)?;
    let dir = stage_upgrade_env()?;
    let bin_path = dir.join("staged.bin");
    std::fs::write(&bin_path, &bytes).map_err(|e| format!("无法写入暂存固件：{e}"))?;

    let mut boot_count = 0u32;
    let mut boot_devices = Vec::new();
    let mut logs = Vec::new();
    let code = run_helper(&dir, "probe", &bin_path, None, |line| {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
            match v.get("ev").and_then(|e| e.as_str()) {
                Some("boot") => {
                    boot_count = v.get("count").and_then(|c| c.as_u64()).unwrap_or(0) as u32;
                    if let Some(arr) = v.get("devices").and_then(|d| d.as_array()) {
                        boot_devices = arr
                            .iter()
                            .filter_map(|s| s.as_str().map(|s| s.to_string()))
                            .collect();
                    }
                }
                Some("logs") => {
                    if let Some(arr) = v.get("lines").and_then(|d| d.as_array()) {
                        logs = arr
                            .iter()
                            .filter_map(|s| s.as_str().map(|s| s.to_string()))
                            .collect();
                    }
                }
                _ => {}
            }
        }
    })?;
    if code != 0 && boot_count == 0 && logs.is_empty() {
        return Err(format!("升级辅助进程异常退出（exit {code}）。"));
    }
    Ok(FwProbe {
        boot_count,
        boot_devices,
        logs,
    })
}

/// Flash a verified image. Streams each helper line as a `fw-event` payload
/// (already parsed JSON) so the UI can show live progress. The heavy lifting,
/// including entering boot mode, is inside the vendor DLL.
#[tauri::command]
fn firmware_flash(
    window: tauri::Window,
    bytes: Vec<u8>,
) -> Result<serde_json::Value, String> {
    use tauri::Emitter;
    verify_image(&bytes)?;
    let dir = stage_upgrade_env()?;
    let bin_path = dir.join("staged.bin");
    std::fs::write(&bin_path, &bytes).map_err(|e| format!("无法写入暂存固件：{e}"))?;

    let mut final_value = serde_json::json!({ "ev": "result", "ok": false });
    let mut saw_result = false;
    let code = run_helper(&dir, "flash", &bin_path, Some(120_000), |line| {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
            if v.get("ev").and_then(|e| e.as_str()) == Some("result") {
                final_value = v.clone();
                saw_result = true;
            }
            let _ = window.emit("fw-event", v);
        }
    })?;
    if code != 0 && !saw_result {
        final_value = serde_json::json!({ "ev": "result", "ok": false, "exit": code });
    }
    let _ = std::fs::remove_file(&bin_path);
    Ok(final_value)
}

/// USB bcdDevice -> "v1.26". The low byte is BCD, so 0x26 is 26 and not 38 —
/// which happens to make the same formatting the mouse version uses correct.
fn format_release(release: u16) -> Option<String> {
    // A release number of 0 means the descriptor did not carry one; showing
    // "v0.00" would read like a broken device rather than a silent one.
    if release == 0 {
        return None;
    }
    Some(format!("v{:x}.{:02x}", release >> 8, release & 0xff))
}

/// `full` asks for the battery too. The 600 ms poll passes `false`: the battery
/// costs an extra command round trip, and paying it on every tick is what made
/// an unresponsive mouse feel like a hung app.
#[tauri::command]
fn status(full: Option<bool>) -> Status {
    with_link_poll(|link| {
        let full = full.unwrap_or(true);
        // One 10-byte read carries the active stage (byte 4) and the rate byte.
        // One attempt during a poll: a silent device pays the 470 ms timeout
        // either way, and the next tick retries soon enough.
        let block = link.read_block(0x0000, 10, if full { 2 } else { 1 });
        let battery = if full { link.battery() } else { None };
        let dpi_index = block.as_ref().map(|b| b[ACTIVE_STAGE_ADDR as usize]);
        // The 10-byte header read stops at 0x0009, so the stage record needs its
        // own frame — reading it out of the header block would silently give 50 CPI.
        let dpi = match dpi_index {
            Some(idx) if idx >= 1 => {
                let at = protocol::DPI_STAGE_BASE + (idx as usize - 1) * 4;
                link.read_block(at as u16, 4, 2).map(|rec| {
                    protocol::record_bytes_to_dpi(rec[0], rec[2])
                })
            }
            _ => None,
        };
        if block.is_none() && battery.is_none() {
            return Err("silent".to_string());
        }
        Ok(Status {
            online: true,
            battery_level: battery.as_ref().map(|b| b.level),
            battery,
            dpi_index,
            dpi,
            rate_byte: block.as_ref().map(|b| b[0]),
        })
    })
    .unwrap_or_default()
}

/// What the receiver tells us about the current link.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    /// `wired`, `wireless` or `unknown` — see `connection()` for how it is chosen.
    pub link: String,
    /// True when the mouse itself answered a read.
    pub online: bool,
    /// Receiver chip id, used as the payload of the pairing command.
    pub cid: Option<u8>,
    /// Receiver model id.
    pub mid: Option<u8>,
    /// 1 = pairing, 2 = bonded, 0 = idle.
    pub pair_state: Option<u8>,
    pub product_id: u16,
}

/// Bluetooth-mode view of the mouse. This does not need the 2.4 GHz receiver:
/// in Bluetooth mode the mouse is a HID-over-GATT device of its own, so the
/// scan runs independently — and it is the only way to see the mouse at all
/// while the bottom switch is on the Bluetooth position.
#[tauri::command]
fn bluetooth() -> Result<Vec<ble::BleDevice>, String> {
    ble::scan()
}

#[tauri::command]
fn connection() -> Result<Connection, String> {
    with_link(|link| {
        let ids = link.identity();
        let cid = ids.map(|i| i.cid);
        let mid = ids.map(|i| i.mid);
        let pair_state = link.pair_state();
        // If the mouse answers a plain read it is talking to this receiver, so
        // the 2.4 GHz link is the live one. Silence is ambiguous — the mouse
        // may be asleep, or sitting on another mode — so it is reported as
        // unknown rather than guessed.
        let online = link.read_block(0x0000, 2, 1).is_some();
        let link_kind = match (link.product_id, online) {
            (0xf5ab, _) => "wired",
            (_, true) => "wireless",
            _ => "unknown",
        };
        Ok(Connection {
            link: link_kind.to_string(),
            online,
            cid,
            mid,
            pair_state,
            product_id: link.product_id,
        })
    })
}

/// What `pair_begin` hands back, so the UI can be honest about what is coming.
#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
pub struct PairStart {
    /// The CID given to the receiver; 0 when the receiver never told us one.
    pub cid: u8,
    /// The MID, its twin. Both go into the 0x05 payload.
    pub mid: u8,
    /// Whether the mouse was answering before the pairing window opened. The
    /// UI warns on this, because opening the window drops the live link.
    pub was_online: bool,
}

/// One poll inside the pairing window.
#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
pub struct PairPoll {
    /// The receiver's own window report `[state][seconds_left]`, from 0x06.
    ///
    /// Measured on a P1 Pro Max receiver:
    ///  * `00 00` — nothing bonded, no window
    ///  * `01 17` — window open, 23 s left, counting down
    ///  * `02 00` — bonded, mouse back on the link
    ///  * `03 00` — bonded and idle (what it reports before a re-pair)
    pub state: u8,
    /// Seconds the receiver itself says are left in the window. It is the one
    /// honest clock in this process, so the UI counts with it.
    pub left: u8,
    /// Whether the mouse currently answers the receiver at all. This is the
    /// fallback signal: if the receiver never reports a bond but the mouse
    /// starts answering again, the bond was made and the receiver stayed quiet.
    pub online: bool,
}

/// Open the pairing window and return immediately.
///
/// The old `start_pair` sent 0x05, waited 300 ms and declared failure. That is
/// far shorter than the gesture it asks the user to perform — three buttons for
/// three seconds, then a handshake — so it could only ever answer "no".
/// The window is opened here and closed by time on the UI side, which polls
/// `pair_poll` while it stays open.
#[tauri::command]
fn pair_begin() -> Result<PairStart, String> {
    with_link(|link| {
        let ids = link.identity();
        let (cid, mid) = ids.map(|i| (i.cid, i.mid)).unwrap_or((0, 0));
        let was_online = link.mouse_online();
        link.enter_pair(cid, mid)?;
        Ok(PairStart { cid, mid, was_online })
    })
}

/// What the last successful `device_info` saw. A firmware image declares the
/// CID/MID and IC it is built for, so these are what a candidate image is
/// checked against. 0 means "not read yet" in both counters.
static LAST_PID: AtomicU16 = AtomicU16::new(0);
static LAST_CID: AtomicU8 = AtomicU8::new(0);
static LAST_MID: AtomicU8 = AtomicU8::new(0);

/// Poll the pairing window.
///
/// Both halves go out every time: 0x06 is the receiver's own countdown, and
/// asking the mouse whether it is there is the independent second tell. The UI
/// closes the window on either, so neither may be sampled less often than the
/// other — a bond can land inside one 600 ms tick.
#[tauri::command]
fn pair_poll() -> Result<PairPoll, String> {
    with_link_poll(|link| {
        let raw = link.pair_state_raw().unwrap_or_default();
        Ok(PairPoll {
            state: raw.first().copied().unwrap_or(0),
            left: raw.get(1).copied().unwrap_or(0),
            online: link.mouse_online(),
        })
    })
}

/// Shown whenever the receiver will not answer before a read or a write.
///
/// One wording for every path that needs it, because a sleeping mouse is the
/// same situation whether the user pressed Read, Apply or Import — and six
/// copies of the sentence is six places for it to drift.
const WAKE_ERR: &str = "接收器没有响应。请动一下鼠标把它唤醒，然后再试一次。";

/// Wake the receiver before a deliberate read or write.
///
/// `read_all` passes its own count: the boot path knocks once so the window
/// appears at once, while a deliberate read knocks six times.
fn require_awake(link: &Link, tries: usize) -> Result<(), String> {
    if link.ensure_reads(tries) {
        Ok(())
    } else {
        Err(WAKE_ERR.to_string())
    }
}

/// Read the shortcut slots a class-5 button points at and decode them.
///
/// Only the slots actually referenced are read: one slot is 32 bytes, i.e. four
/// frames, and reading all sixteen would add fifty-odd round trips to every
/// refresh for a binding almost nobody has set. Failures are swallowed on
/// purpose — a slot the receiver will not answer for shows as unlabelled
/// rather than failing the whole read.
/// Refresh the labels behind class-5 bindings.
///
/// Slot `i` belongs to button `i` — see the note on [`SHORTCUT_BASE`] — so the
/// six relevant slots are read instead of all sixteen. Failures are swallowed
/// on purpose: a slot the receiver will not answer for shows as unlabelled
/// rather than failing the whole read.
fn attach_shortcuts(link: &Link, mut snap: Snapshot) -> Snapshot {
    let bound: Vec<usize> = snap
        .buttons
        .iter()
        .enumerate()
        .filter(|(_, b)| b.cls == KEY_CLASS_COMBO && b.valid)
        .map(|(i, _)| i)
        .collect();
    for index in bound {
        let base = protocol::SHORTCUT_BASE + index * protocol::SHORTCUT_STRIDE;
        let addr = base as u16;
        let label = link
            .read_range(addr, addr + protocol::SHORTCUT_STRIDE as u16)
            .ok()
            .and_then(|raw| protocol::decode_shortcut_slot(&raw));
        if let Some(text) = label.clone() {
            snap.buttons[index].action = format!("组合键 · {text}");
        }
        snap
            .shortcuts
            .push(protocol::ShortcutRef { slot: index as u8, label });
    }
    snap
}

#[tauri::command]
fn read_all(quick: Option<bool>) -> Result<Snapshot, String> {
    with_link(|link| {
        // `quick` is the boot path: one handshake attempt is enough to decide
        // whether the mouse is there, and it keeps the window from waiting on a
        // device that may be switched off. A deliberate read still knocks six
        // times.
        require_awake(link, if quick.unwrap_or(false) { 1 } else { 6 })?;
        let block = read_block_retrying(link)?;
        Ok(attach_macros(link, attach_shortcuts(link, decode(&block))))
    })
}

/// Read one combination slot and decode it, for diagnostics.
///
/// Opens its own handle rather than taking the cached one: it runs right after
/// a diagnostic write, and a fresh connection is the honest way to ask whether
/// the bytes are really in flash.
fn combo_probe_read(index: usize) -> Option<String> {
    let link = Link::connect().ok()?;
    let addr = (protocol::SHORTCUT_BASE + index * protocol::SHORTCUT_STRIDE) as u16;
    let raw = link
        .read_range(addr, addr + protocol::SHORTCUT_STRIDE as u16)
        .ok()?;
    protocol::decode_shortcut_slot(&raw)
}

/// Put one combination slot back the way it ships, so a diagnostic run leaves
/// no binding behind.
fn combo_probe_erase(index: usize) -> Result<(), String> {
    with_link(|link| {
        let addr = (protocol::SHORTCUT_BASE + index * protocol::SHORTCUT_STRIDE) as u16;
        let before = link.read_range(addr, addr + protocol::SHORTCUT_STRIDE as u16).ok();
        link.write_chunks(addr, &[0xffu8; protocol::SHORTCUT_STRIDE], before.as_deref())?;
        Ok(())
    })
}

/// Read the 192-byte block, tolerating the receiver's habit of answering a
/// long wake with one all-zero buffer.
fn read_block_retrying(link: &Link) -> Result<Vec<u8>, String> {
    let mut last: Option<Vec<u8>> = None;
    for _ in 0..3 {
        let block = link.read_range(0, CONFIG_END as u16)?;
        if block[0] != 0 || block[1] != 0 || block[2] != 0 {
            return Ok(block);
        }
        last = Some(block);
        sleep(Duration::from_millis(400));
        let _ = link.ensure_reads(2);
    }
    Err(match last {
        Some(_) => "读到的配置全为 0：鼠标正在待机。请动一下鼠标再试。".to_string(),
        None => "读取失败".to_string(),
    })
}

/// Push a block to flash and read it back.
///
/// Shared by `apply` and `import_block`: both stage records, both send the
/// commit frame, and both have to wait out the standby the receiver enters
/// while it writes. Returns the frame count and the block the device reports
/// afterwards, which is the only proof any write landed.
fn commit(link: &Link, next: &[u8], before: &[u8]) -> Result<(usize, Vec<u8>), String> {
    let frames = link.write_config(next, before)?;
    // The receiver goes into standby while it writes flash, so the read-back
    // needs a real wake rather than a quick poll.
    link.ensure_reads(12);
    let after = match read_block_retrying(link) {
        Ok(block) => block,
        Err(e) => return Err(format!("已写入 {frames} 帧，但读回失败：{e}")),
    };
    Ok((frames, after))
}

#[tauri::command]
fn apply(patch: StatePatch) -> Result<Applied, String> {
    with_link(|link| {
        require_awake(link, 6)?;
        let before = read_block_retrying(link)?;
        let mut next = before.clone();
        encode(&mut next, &patch);

        let changed: Vec<usize> = (0..next.len())
            .step_by(4)
            .filter(|a| next[*a..*a + 4] != before[*a..*a + 4])
            .collect();
        if changed.is_empty() {
            return Ok(Applied { snapshot: decode(&before), frames: 0 });
        }

        let (frames, after) = commit(link, &next, &before)?;
        let stuck: Vec<usize> = changed
            .iter()
            .copied()
            .filter(|a| after[*a..*a + 4] != next[*a..*a + 4])
            .collect();
        if !stuck.is_empty() {
            return Err(format!(
                "设备确认了 {frames} 帧，但配置没有变化（首个未生效的地址 0x{:04x}）",
                stuck[0]
            ));
        }

        // The active DPI stage is the one record a third-party tool warns gets
        // overwritten from the mouse's own RAM copy within 5-10 s, which would
        // make the write look applied and quietly do nothing.
        //
        // Measured here instead of assumed: writing a stage and reading it back
        // once a second for fourteen seconds never saw it move. So the check is
        // one early sample, not a hold — it costs 2.5 s on the rare occasion
        // this field is written, and catches a fast revert without making the
        // button feel broken.
        if patch.active_stage.is_some() {
            let want = decode(&after).active_stage;
            sleep(Duration::from_millis(2500));
            if let Ok(later) = read_block_retrying(link) {
                let now = decode(&later).active_stage;
                if now != want {
                    return Err(format!(
                        "设备没有保留这个档位：写入后是第 {} 档，2.5 秒后回到第 {} 档。\
                         本机测试没有复现回弹，但既然出现了，就说明这个固件会用自身的值覆盖这一项 —— \
                         请用鼠标上的 DPI 键切换，或者改档位数值本身。",
                        want + 1,
                        now + 1
                    ));
                }
            }
        }
        Ok(Applied { snapshot: decode(&after), frames })
    })
}

/// Bind one physical button.
///
/// Deliberately not part of `apply`: a bad binding is the one setting you
/// cannot nudge back from the page that changed it. If every button stops
/// being left click, nothing on screen can be clicked any more, so the write
/// is refused here rather than undone afterwards.
#[tauri::command]
fn set_button(index: u8, action: String) -> Result<Applied, String> {
    with_link(|link| {
        require_awake(link, 6)?;
        let idx = index as usize;
        if idx >= protocol::BUTTON_COUNT {
            return Err(format!(
                "按键序号超出范围（0..{}）",
                protocol::BUTTON_COUNT - 1
            ));
        }
        // The UI builds its list from `capabilities`, so a mismatch here means
        // a stale page rather than a user typing something odd.
        let binding = protocol::find_button_action(action.trim())
            .ok_or_else(|| format!("不支持的按键功能：{action}"))?;

        let before = read_block_retrying(link)?;
        let mut next = before.clone();
        protocol::encode_button(&mut next, idx, binding.cls, binding.param);
        if next == before {
            return Ok(Applied { snapshot: decode(&before), frames: 0 });
        }
        if !protocol::has_left_click(&next) {
            return Err(
                "这样改完之后没有任何按键是左键，鼠标将无法操作。请先把某个按键设成左键。".to_string(),
            );
        }
        let (frames, after) = commit(link, &next, &before)?;
        if protocol::read_record(&after, protocol::key_record_addr(idx)).0 != binding.cls {
            return Err(format!(
                "设备确认了 {frames} 帧，但 0x{:04x} 没有变成写入的值",
                protocol::key_record_addr(idx)
            ));
        }
        Ok(Applied { snapshot: decode(&after), frames })
    })
}

/// Bind one physical button to a keyboard combination or a multimedia key.
///
/// The keyboard part takes two writes and neither alone does anything, which
/// makes the ordering here load-bearing: the 32-byte combination slot goes
/// first, then the button record is pointed at it (class 5). Written the other
/// way round the button would announce a combination whose slot still held
/// whatever was there before.
///
/// Verified back after the write — the slot is read once more and decoded, so
/// a combination the mouse stored differently from how it was encoded is
/// reported rather than silently accepted. That check cannot tell whether the
/// firmware *accepts* this checksum convention; only pressing the button can,
/// which is why the UI says so.
#[tauri::command]
fn set_button_combo(
    index: u8,
    modifiers: u8,
    usage: u16,
    consumer: bool,
) -> Result<Applied, String> {
    with_link(|link| {
        require_awake(link, 6)?;
        let idx = index as usize;
        if idx >= protocol::BUTTON_COUNT {
            return Err(format!(
                "按键序号超出范围（0..{}）",
                protocol::BUTTON_COUNT - 1
            ));
        }
        // Six buttons and sixteen slots, so this is a sanity check rather than
        // something a caller can trip over today.
        if idx >= protocol::SHORTCUT_SLOTS {
            return Err("这个按键没有对应的组合键槽".to_string());
        }
        if modifiers & !protocol::MODIFIER_MASK != 0 {
            return Err("修饰键只支持 Ctrl / Shift / Alt / Win".to_string());
        }
        let known = if consumer {
            protocol::OFFERED_CONSUMER.iter().any(|(u, _)| *u == usage)
        } else {
            protocol::OFFERED_KEYS.iter().any(|(u, _)| *u == usage)
        };
        if !known {
            return Err(format!("不支持的按键 0x{usage:04x}"));
        }

        let events = protocol::shortcut_events(modifiers, usage, consumer);
        let slot = protocol::encode_shortcut_slot(&events)
            .ok_or_else(|| "这个组合太长，32 字节的槽放不下".to_string())?;
        let want = protocol::shortcut_target_label(modifiers, usage, consumer)
            .ok_or_else(|| "无法解读刚生成的组合，已放弃写入".to_string())?;

        let addr = (protocol::SHORTCUT_BASE + idx * protocol::SHORTCUT_STRIDE) as u16;
        let before_slot = link.read_range(addr, addr + protocol::SHORTCUT_STRIDE as u16).ok();
        link.write_chunks(addr, &slot, before_slot.as_deref())?;

        let before = read_block_retrying(link)?;
        let mut next = before.clone();
        protocol::encode_button(&mut next, idx, protocol::KEY_CLASS_COMBO, 0x0000);
        // Nothing about this binding takes the left click away, so the guard is
        // a cheap re-check rather than the reason for its own existence.
        if !protocol::has_left_click(&next) {
            return Err("这样改完之后没有任何按键是左键，鼠标将无法操作。".to_string());
        }
        let (frames, after) = commit(link, &next, &before)?;
        if protocol::read_record(&after, protocol::key_record_addr(idx)).0
            != protocol::KEY_CLASS_COMBO
        {
            return Err(format!(
                "设备确认了 {frames} 帧，但 0x{:04x} 没有变成组合键类别",
                protocol::key_record_addr(idx)
            ));
        }

        // Read the slot back and decode it: this is the only signal available
        // about whether the mouse kept what was written.
        let got = link
            .read_range(addr, addr + protocol::SHORTCUT_STRIDE as u16)
            .ok()
            .and_then(|raw| protocol::decode_shortcut_slot(&raw));
        let mut snap = attach_shortcuts(link, protocol::decode(&after));
        if got.as_deref() != Some(want.as_str()) {
            snap.shortcuts
                .push(protocol::ShortcutRef { slot: index, label: got.clone() });
            return Err(format!(
                "写入的设备解答是“{}”，期望的是“{want}”。\\
                 组合已写入并让 {name} 生效；若按下无反应，说明本机固件不是这套编码。",
                got.as_deref().unwrap_or("(空槽)"),
                name = protocol::BUTTON_NAMES[idx],
            ));
        }
        Ok(Applied { snapshot: snap, frames })
    })
}

/// Name a class-6 binding by reading the macro region it points at.
///
/// Slot `i` belongs to button `i`, the same convention the combinations use.
/// Skipped entirely for buttons that are not bound to a macro — the region is
/// 384 bytes, which is 39 frames of reading.
fn attach_macros(link: &Link, mut snap: Snapshot) -> Snapshot {
    let bound: Vec<usize> = snap
        .buttons
        .iter()
        .enumerate()
        .filter(|(_, b)| b.cls == KEY_CLASS_MACRO && b.valid)
        .map(|(i, _)| i)
        .collect();
    for index in bound {
        let base = protocol::MACRO_BASE + index * protocol::MACRO_STRIDE;
        let view = link
            .read_range(base as u16, (base + protocol::MACRO_STRIDE) as u16)
            .ok()
            .and_then(|raw| protocol::decode_macro(index as u8, &raw));
        if let Some(v) = &view {
            let label = if v.name.is_empty() { "未命名".to_string() } else { v.name.clone() };
            snap.buttons[index].action = format!("宏 · {} （{} 步）", label, v.steps.len());
        }
        snap.macros.push(view);
    }
    snap
}

/// Read one macro region and decode it.
///
/// A button bound to a macro points at it with class 6; the region itself sits
/// at `0x0300 + 0x180 × slot`. Reading costs 39 frames, so this is offered as
/// an explicit action rather than folded into every configuration read.
#[tauri::command]
fn read_macro(slot: u8) -> Result<Option<protocol::MacroView>, String> {
    with_link(|link| {
        require_awake(link, 6)?;
        if slot as usize >= protocol::MACRO_SLOTS {
            return Err(format!("宏槽超出范围（0..{}）", protocol::MACRO_SLOTS - 1));
        }
        let base = protocol::MACRO_BASE + slot as usize * protocol::MACRO_STRIDE;
        let raw = link
            .read_range(base as u16, (base + protocol::MACRO_STRIDE) as u16)?;
        Ok(protocol::decode_macro(slot, &raw))
    })
}

/// Write a macro and bind a button to it.
///
/// Order matters for the same reason it does with a combination: the region
/// has to hold something before a button points at it. After both writes the
/// region is read back and decoded, so a device that stored the events
/// differently from how they were encoded is reported instead of accepted.
///
/// The layout is the one a tool for the Areson revision of this family
/// documents, and it agrees field for field with the constants a CompX-side
/// project publishes for ATK hardware. It is no longer a guess: bound to the
/// forward button, a two-step F24 macro was pressed and Windows received F24
/// twice, 250 ms apart, in the order the steps were written.
#[tauri::command]
fn set_button_macro(
    index: u8,
    name: String,
    steps: Vec<protocol::MacroStep>,
    repeat: u8,
) -> Result<Applied, String> {
    with_link(|link| {
        require_awake(link, 6)?;
        let idx = index as usize;
        if idx >= protocol::BUTTON_COUNT {
            return Err(format!(
                "按键序号超出范围（0..{}）",
                protocol::BUTTON_COUNT - 1
            ));
        }
        if idx >= protocol::MACRO_SLOTS {
            return Err("这个按键没有对应的宏槽".to_string());
        }
        let legal_repeat = (1..=0xfd).contains(&repeat) || repeat == 0xfe || repeat == 0xff;
        if !legal_repeat {
            return Err("循环次数只能是 1–253、按住循环或按任意键停止".to_string());
        }

        let region = protocol::encode_macro(name.trim(), &steps)?;
        let base = protocol::MACRO_BASE + idx * protocol::MACRO_STRIDE;
        let before_region = link
            .read_range(base as u16, (base + protocol::MACRO_STRIDE) as u16)
            .ok();
        link.write_chunks(base as u16, &region, before_region.as_deref())?;

        let before = read_block_retrying(link)?;
        let mut next = before.clone();
        let param = ((idx as u16) << 8) | repeat as u16;
        protocol::encode_button(&mut next, idx, protocol::KEY_CLASS_MACRO, param);
        if !protocol::has_left_click(&next) {
            return Err("这样改完之后没有任何按键是左键，鼠标将无法操作。".to_string());
        }
        let (frames, after) = commit(link, &next, &before)?;
        let (cls, hi, lo, _) = protocol::read_record(&after, protocol::key_record_addr(idx));
        if cls != protocol::KEY_CLASS_MACRO || hi as usize != idx || lo != repeat {
            return Err(format!(
                "设备确认了 {frames} 帧，但 0x{:04x} 没有变成指向宏 {} 的记录",
                protocol::key_record_addr(idx),
                idx
            ));
        }

        let stored = link
            .read_range(base as u16, (base + protocol::MACRO_STRIDE) as u16)
            .ok()
            .and_then(|raw| protocol::decode_macro(idx as u8, &raw));
        let want_events = protocol::macro_event_count(&steps);
        match &stored {
            Some(view) if view.events == want_events && view.checksum_ok => {}
            other => {
                return Err(format!(
                    "宏已写入并绑定到{}，但回读不一致（写入 {want_events} 个事件，读回 {:?}）。\
                     若按下无反应，把该键改回普通动作即可。",
                    protocol::BUTTON_NAMES[idx],
                    other.as_ref().map(|v| v.events)
                ))
            }
        }
        Ok(Applied { snapshot: attach_macros(link, protocol::decode(&after)), frames })
    })
}

#[tauri::command]
fn export_block() -> Result<BlockDump, String> {
    with_link(|link| {
        require_awake(link, 6)?;
        let block = read_block_retrying(link)?;
        let hex = block.iter().map(|b| format!("{b:02x}")).collect::<String>();
        Ok(BlockDump { hex, snapshot: decode(&block) })
    })
}

/// Write a block produced by `export_block`. Used to restore a backup.
///
/// The one command that takes bytes from outside and puts them straight into
/// flash, so it is also the one that checks them first: `block_is_sane`
/// refuses anything that is not shaped like this device's configuration, and
/// refuses a block with no left click even though the file is the user's own.
/// Writing it anyway would leave a mouse that moves but cannot press anything,
/// which is not a state this app should help anyone reach.
#[tauri::command]
fn import_block(hex: String) -> Result<Applied, String> {
    let cleaned: String = hex.chars().filter(|c| !c.is_whitespace()).collect();
    if cleaned.len() != CONFIG_END * 2 {
        return Err(format!(
            "配置块必须是 {} 字节（{} 个十六进制字符），收到 {} 个字符",
            CONFIG_END,
            CONFIG_END * 2,
            cleaned.len()
        ));
    }
    let mut next = Vec::with_capacity(CONFIG_END);
    for i in 0..CONFIG_END {
        let byte = u8::from_str_radix(&cleaned[i * 2..i * 2 + 2], 16)
            .map_err(|_| format!("第 {} 个字节不是有效的十六进制", i))?;
        next.push(byte);
    }

    protocol::block_is_sane(&next)?;

    with_link(|link| {
        require_awake(link, 6)?;
        let before = read_block_retrying(link)?;
        let (frames, after) = commit(link, &next, &before)?;
        Ok(Applied { snapshot: decode(&after), frames })
    })
}

#[tauri::command]
fn wake() -> bool {
    with_link(|link| Ok(link.ensure_reads(10))).unwrap_or(false)
}

/// Open the Windows Bluetooth page.
///
/// Pairing a Bluetooth mouse ends in an OS dialog that no app can drive, so the
/// best this program can do is take the user straight there instead of saying
/// "go find it yourself". Uses `cmd /c start` because `ms-settings:` is a shell
/// URI, not an executable.
#[tauri::command]
fn open_bluetooth_settings() -> Result<(), String> {
    std::process::Command::new("cmd")
        .args(["/c", "start", "ms-settings:bluetooth"])
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("无法打开系统蓝牙设置：{e}"))
}

/// Set the report rate, e.g. 1000 Hz.
///
/// Deliberately separate from `apply`: the rate lives at 0x0000 as a two-byte
/// value that the device applies immediately, while everything else in the
/// configuration goes through four-byte records plus a commit frame. Mixing
/// the two would mean a DPI edit also stamps the rate, so the rate gets its
/// own entry point, its own read-back and its own error message.
#[tauri::command]
fn set_report_rate(hz: u32) -> Result<RateResult, String> {
    with_link(|link| {
        let allowed = protocol::rates_for(link.product_id);
        if !allowed.contains(&hz) {
            return Err(format!(
                "当前端点没有开放 {hz} Hz：只开放已实测的 {}",
                allowed
                    .iter()
                    .map(|r| r.to_string())
                    .collect::<Vec<_>>()
                    .join(" / ")
            ));
        }
        require_awake(link, 6)?;
        link.set_report_rate(hz)?;
        // Report what the device actually stored, not what we asked for.
        let byte = link
            .read_block(0x0000, 2, 3)
            .and_then(|v| v.first().copied())
            .ok_or_else(|| "回报率写入后设备停止应答".to_string())?;
        Ok(RateResult { rate_hz: protocol::code_to_rate(byte), byte })
    })
}

/// What the device stored after a report-rate write.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RateResult {
    /// The rate the stored code means. `None` for a code nobody measured.
    pub rate_hz: Option<u32>,
    /// The raw byte at 0x0000.
    pub byte: u8,
}

// `report_rates` used to be a command of its own. It went unused once
// `device_info` started carrying `report_rates` in the same round trip, and a
// second door to the same answer is only a second thing to keep in step.

/// Read the receiver light again and again until it answers.
///
/// Right after a write the receiver is storing the payload and answers nothing
/// for a few hundred milliseconds — measured: reading immediately gets no
/// reply at all, while a fresh process reading the very same value gets it in
/// 124 ms. So a write is only reported once the value really came back.
fn read_rgb_patiently(link: &Link) -> Option<protocol::DongleRgb> {
    for attempt in 0..5 {
        if attempt > 0 {
            sleep(Duration::from_millis(150));
        }
        if let Some(v) = link.read_dongle_rgb() {
            return Some(v);
        }
    }
    None
}


/// Write the 4K receiver's light (mode + three colours).
///
/// The mode and colours are stored as one ten-byte payload in a single frame;
/// there is no commit step and nothing to undo, so the UI reads the old value
/// first and can put it back.
#[tauri::command]
fn set_dongle_rgb(mode: u8, color1: String, color2: String, color3: String) -> Result<DongleRgbView, String> {
    fn parse(hex: &str) -> Result<[u8; 3], String> {
        let s = hex.trim().trim_start_matches('#');
        if s.len() != 6 {
            return Err(format!("颜色 {hex} 不是 #rrggbb 格式"));
        }
        let mut out = [0u8; 3];
        for i in 0..3 {
            out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
                .map_err(|_| format!("颜色 {hex} 含有非十六进制字符"))?;
        }
        Ok(out)
    }
    let rgb = protocol::DongleRgb {
        mode,
        color1: parse(&color1)?,
        color2: parse(&color2)?,
        color3: parse(&color3)?,
    };
    with_link(|link| {
        require_awake(link, 6)?;
        link.set_dongle_rgb(rgb)?;
        // Report what the receiver stored, the same discipline as the report
        // rate: never claim a write landed without reading it back.
        read_rgb_patiently(link)
            .map(DongleRgbView::of)
            .ok_or_else(|| "写入已发出，但接收器之后没有应答读取。请稍等一下再刷新。".to_string())
    })
}

// ── advanced settings ────────────────────────────────────────────────────
//
// Long range mode, onboard profiles and the factory reset. All three answer
// with the accept/reject flag, and all three are checked against it, because a
// refused write comes back looking exactly like a successful one.

/// Switch long range mode (0x16). Reports what the device now holds (0x17).
#[tauri::command]
fn set_long_range(on: bool) -> Result<bool, String> {
    with_link(|link| {
        link.set_long_range(on)?;
        link.long_range().ok_or_else(|| {
            "写入已收到，但之后没有读到状态。请稍等一下再刷新。".to_string()
        })
    })
}

/// Switch onboard profile (0x0f). Reports what the device now holds (0x0e).
#[tauri::command]
fn set_current_config(index: u8) -> Result<u8, String> {
    with_link(|link| {
        link.set_current_config(index)?;
        link.current_config().ok_or_else(|| {
            "切换已收到，但之后没有读到配置档。请稍等一下再刷新。".to_string()
        })
    })
}

/// Wipe every user setting in the device's flash (0x09).
///
/// The confirmation is the caller's job and the UI's, not this handler's: by
/// the time this runs the user has already been asked. Nothing else in the app
/// reaches 0x09, and no poll or diagnostic sends it either — an accidental
/// factory reset is unrecoverable, so the whole path is kept to one call site.
#[tauri::command]
fn factory_reset() -> Result<(), String> {
    with_link(|link| link.factory_reset())
}

#[tauri::command]
fn forget() {
    if let Ok(mut guard) = LINK.lock() {
        *guard = None;
    }
}

// ── app settings ─────────────────────────────────────────────────────────
//
// Small user preferences (theme mode) that must survive a cleared WebView
// profile, so they live in the app config dir rather than localStorage.

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct Settings {
    theme: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self { theme: "system".into() }
    }
}

/// `<config>/cn.metaphyuni.hub/settings.json`, created on demand.
fn settings_path(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    use tauri::Manager;
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("无法定位配置目录：{e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建配置目录：{e}"))?;
    Ok(dir.join("settings.json"))
}

/// Missing or unreadable file means defaults — never an error the UI must
/// handle on first launch.
#[tauri::command]
fn load_settings(app: tauri::AppHandle) -> Settings {
    settings_path(&app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

#[tauri::command]
fn save_settings(app: tauri::AppHandle, theme: String) -> Result<(), String> {
    if !["system", "light", "dark"].contains(&theme.as_str()) {
        return Err(format!("未知主题模式：{theme}"));
    }
    let path = settings_path(&app)?;
    let json = serde_json::to_string_pretty(&Settings { theme })
        .map_err(|e| format!("{e}"))?;
    std::fs::write(&path, json).map_err(|e| format!("无法写入设置：{e}"))
}

/// The one list the UI genuinely cannot write itself: the sleep-timeout menu.
///
/// The rest of what this used to return (DPI limits, debounce range, report
/// rates, profile count) is hard-coded in the markup and in `device_info`
/// respectively, so shipping it twice only gave the two copies room to drift.
/// The stored value is in units of 10 seconds — the vendor app's own list.
#[tauri::command]
fn capabilities() -> serde_json::Value {
    serde_json::json!({
        // 10-second units, so 3 is half a minute and 180 is half an hour. The
        // list stops where the vendor app's does; nothing here is a guess about
        // what the firmware would accept beyond 0xff units.
        "sleepTimeout": [3, 6, 12, 18, 30, 60, 90, 120, 180],
        "sleepUnitSeconds": 10,
        "sensorModes": [
            { "value": 0, "label": "基本" },
            { "value": 1, "label": "竞技固件" },
            { "value": 2, "label": "其他（2）" },
        ],
        // One authoritative list of what a button may be bound to, so the UI
        // never offers a binding the firmware lacks.
        "macro": {
            "slots": protocol::MACRO_SLOTS,
            "maxEvents": protocol::MACRO_MAX_EVENTS,
            "minDelay": protocol::MACRO_MIN_DELAY_MS,
            "maxDelay": 5000,
            "nameChars": (protocol::MACRO_COUNT_OFFSET - 1) / 2,
        },
        "buttons": {
            "names": protocol::BUTTON_NAMES,
            "actions": protocol::BUTTON_ACTIONS.iter().map(|a| a.label).collect::<Vec<_>>(),
            // Present in the shared class table, never seen this mouse perform
            // one. Offered in their own group so the list does not imply a
            // confidence it does not have.
            "unverified": protocol::BUTTON_ACTIONS_UNVERIFIED
                .iter()
                .map(|a| a.label)
                .collect::<Vec<_>>(),
            "modifiers": [
                { "bit": protocol::MOD_CTRL, "label": "Ctrl" },
                { "bit": protocol::MOD_SHIFT, "label": "Shift" },
                { "bit": protocol::MOD_ALT, "label": "Alt" },
                { "bit": protocol::MOD_GUI, "label": "Win" },
            ],
            "keys": protocol::OFFERED_KEYS
                .iter()
                .map(|(usage, label)| serde_json::json!({ "usage": usage, "label": label }))
                .collect::<Vec<_>>(),
            "consumer": protocol::OFFERED_CONSUMER
                .iter()
                .map(|(usage, label)| serde_json::json!({ "usage": usage, "label": label }))
                .collect::<Vec<_>>(),
        },
        // Stored lift-off bytes, low -> high. The order looks scrambled
        // because it is: 1 is the middle setting on this layout.
        "lod": [
            { "value": protocol::LOD_LOW, "label": "低" },
            { "value": protocol::LOD_MEDIUM, "label": "中" },
            { "value": protocol::LOD_HIGH, "label": "高" },
        ],
        "dpiLightModes": [
            { "value": 0, "label": "关闭" },
            { "value": 1, "label": "常亮" },
            { "value": 2, "label": "呼吸" },
        ],
        "dpiLightBrightness": [1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
        "dpiLightSpeed": [1, 2, 3, 4, 5],
    })
}

/// 建窗口之前先看一眼 WebView2 在不在。
///
/// 缺了它窗口根本建不起来，而用户看到的现象是"双击了没反应" —— 没有窗口、
/// 没有报错、日志里也没有。这是把程序拷到另一台机器上时最可能撞上的事，
/// 而且是唯一一个"界面本身无法解释"的失败，所以只能用系统消息框来说。
/// 返回 false 表示调用方应当直接退出。
fn ensure_webview2() -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_OKCANCEL, MB_TOPMOST, IDOK, SW_SHOW,
    };

    if env::webview2_version().is_some() {
        return true;
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    let text = wide(
        "界面需要微软的 WebView2 运行时，这台电脑上没有找到。\n\n\
         程序本体是完整的，缺的只是系统组件 —— Windows 10/11 一般随 Edge 一起\n\
         装，这台机器可能是精简版系统或长期未更新。\n\n\
         点「确定」打开官方下载页，装完再启动本程序即可（无需重装本程序）。",
    );
    let caption = wide("Metaphyuni玄熊猫P1管理器 —— 缺少 WebView2 运行时");
    let answer = unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_ICONERROR | MB_OKCANCEL | MB_TOPMOST,
        )
    };
    if answer == IDOK {
        let verb = wide("open");
        let url = wide("https://go.microsoft.com/fwlink/p/?LinkId=2124703");
        unsafe {
            ShellExecuteW(
                None,
                PCWSTR(verb.as_ptr()),
                PCWSTR(url.as_ptr()),
                None,
                None,
                SW_SHOW,
            )
        };
    }
    false
}

pub fn run() {
    if !ensure_webview2() {
        return;
    }
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            device_info,
            status,
            connection,
            bluetooth,
            pair_begin,
            pair_poll,
            read_all,
            apply,
            export_block,
            import_block,
            wake,
            forget,
            open_bluetooth_settings,
            capabilities,
            load_settings,
            save_settings,
            set_report_rate,
            parse_firmware,
            firmware_probe,
            firmware_flash,
            set_dongle_rgb,
            set_long_range,
            set_current_config,
            set_button,
            set_button_combo,
            set_button_macro,
            read_macro,
            factory_reset
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

// ── command line diagnostics ─────────────────────────────────────────────
//
// The GUI is not a good place to debug a protocol, so the same binary answers a
// few flags and writes a log. `--probe` is the quick health check.

/// Keep knocking until the receiver answers, up to `secs`. Zero means "one try".
///
/// A sleeping mouse only comes back on physical motion, so a diagnostic run
/// started against an idle desk must be able to wait - and must say how long it
/// waited, otherwise a silent device looks like a broken command.
fn wake_patiently(link: &Link, secs: u64, log: &mut String) {
    if link.ensure_reads(2) {
        return;
    }
    if secs == 0 {
        return;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    let mut attempt = 0;
    loop {
        attempt += 1;
        if link.ensure_reads(3) {
            log.push_str(&format!("woke on attempt {attempt}\n"));
            return;
        }
        if std::time::Instant::now() >= deadline {
            log.push_str(&format!("gave up after {attempt} attempts\n"));
            return;
        }
        sleep(Duration::from_millis(1200));
    }
}

/// Read `--wait <seconds>` from the command line.
fn wait_secs(args: &[String]) -> u64 {
    args.iter()
        .position(|a| a == "--wait")
        .and_then(|j| args.get(j + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Read a `--payload <hex>` argument, tolerating spaces and commas.
fn parse_hex_arg(args: &[String], flag: &str) -> Vec<u8> {
    let raw = match args.iter().position(|a| a == flag).and_then(|j| args.get(j + 1)) {
        Some(v) => v,
        None => return Vec::new(),
    };
    let clean: String = raw.chars().filter(|c| !c.is_whitespace() && *c != ',').collect();
    (0..clean.len() / 2)
        .filter_map(|k| u8::from_str_radix(&clean[k * 2..k * 2 + 2], 16).ok())
        .collect()
}

pub fn diagnostic(args: &[String]) {
    let mut log = String::new();
    // Written out on every line, not once at the end: when a probe hangs the
    // whole point of the log is to show how far it got before hanging. A single
    // write at the end told us nothing the last time this happened.
    macro_rules! line {
        ($($arg:tt)*) => {{
            log.push_str(&format!($($arg)*));
            log.push('\n');
            let _ = std::fs::write(std::env::temp_dir().join("p1studio-probe.log"), &log);
        }};
    }
    let started = std::time::Instant::now();

    if args.iter().any(|a| a == "--check") {
        // 环境自检，一个字节都不发给设备：它回答的是"这台电脑缺什么"，
        // 而不是"鼠标怎么样"。拿到一台陌生机器时先跑这个。
        let report = env::report();
        line!("{}", report);
        // 在别人的机器上常常没有终端可用，报告又只躺在 %TEMP% 里。exe 旁边
        // 留一份并用记事本打开，双击一个 .bat 就能看到结论。
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let path = dir.join("环境自检.txt");
                match std::fs::write(&path, &report) {
                    Ok(()) => {
                        line!("报告已写入 {}", path.display());
                        let _ = std::process::Command::new("notepad").arg(&path).spawn();
                    }
                    Err(e) => line!("无法在程序旁写入报告：{e}"),
                }
            }
        }
    } else if let Some(i) = args.iter().position(|a| a == "--bench") {
        // Times the two things the 600 ms poll used to pay for every tick, so a
        // regression here is visible instead of only felt.
        let rounds: usize = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(5);
        match Link::connect() {
            Ok(link) => {
                for n in 0..rounds {
                    let t = std::time::Instant::now();
                    let answered = link.read_block(0x0000, 10, 1).is_some();
                    let read_ms = t.elapsed().as_millis();
                    let t = std::time::Instant::now();
                    let batt = link.battery().is_some();
                    let batt_ms = t.elapsed().as_millis();
                    // 0xfff0 is outside the config block, so nothing answers it:
                    // this is what one poll costs while the mouse is silent, and
                    // it is the number that decides how the UI may poll.
                    let t = std::time::Instant::now();
                    let silent = link.read_block(0xfff0, 10, 1).is_some();
                    line!(
                        "poll {n}: read {read_ms} ms (answers {answered}), battery {batt_ms} ms ({batt}), silent-read {} ms ({silent})",
                        t.elapsed().as_millis()
                    );
                }
            }
            Err(e) => line!("connect failed: {e}"),
        }
    } else     if args.iter().any(|a| a == "--conn") {
        // Reports the link, the receiver ids and the pairing state — read-only,
        // so it is safe to run while the mouse is in use.
        match Link::connect() {
            Ok(link) => {
                wake_patiently(&link, wait_secs(args), &mut log);
                let (cid, mid, dtype) = match link.identity() {
                    Some(i) => (
                        format!("0x{:02x}", i.cid),
                        format!("0x{:02x}", i.mid),
                        format!("0x{:02x}", i.device_type),
                    ),
                    None => ("none".to_string(), "none".to_string(), "none".to_string()),
                };
                let online = link.read_block(0x0000, 2, 1).is_some();
                let fw_mouse = read_version(&link, protocol::cmd::VERSION_MOUSE)
                    .unwrap_or_else(|| "—".to_string());
                let fw_dongle = format_release(link.release).unwrap_or_else(|| "—".to_string());
                // 0xb3 is the far end of the link, not the receiver. Printed
                // here only to keep the two readings distinguishable.
                let fw_slave = read_version(&link, protocol::cmd::VERSION_DONGLE)
                    .unwrap_or_else(|| "—".to_string());
                line!("pid: 0x{:04x}", link.product_id);
                line!("cid: {cid}  mid: {mid}  device type: {dtype}");
                line!("firmware: mouse {fw_mouse}  receiver {fw_dongle}  (slave 0xb3: {fw_slave})");
                line!("bcdDevice: 0x{:04x}", link.release);
                line!("mouse online: {online}");
                line!("pair state: {:?}", link.pair_state());
                match webview2_com_sys::loader::location() {
                    Some(p) => line!(
                        "webview2 loader: embedded ({}) -> {p}",
                        webview2_com_sys::loader::EMBEDDED_SIZE
                    ),
                    None => line!("webview2 loader: embedded, cache dir unavailable"),
                }
                let kind = match (link.product_id, online) {
                    (0xf5ab, _) => "wired (USB cable)",
                    (_, true) => "wireless (2.4 GHz receiver)",
                    _ => "unknown (mouse asleep, or on another mode)",
                };
                line!("link: {kind}");
            }
            Err(e) => line!("connect failed: {e}"),
        }
    } else if args.iter().any(|a| a == "--hids") {
        // Every HID interface with our vendor id, including the Bluetooth one:
        // in Bluetooth mode the mouse appears as its own HID-over-GATT device
        // (VID 0x3554, PID 0xf533 on the bench) and the question is whether the
        // vendor command channel also exists there.
        match HidApi::new() {
            Ok(api) => {
                let mut any = false;
                for info in api.device_list() {
                    if info.vendor_id() != protocol::VENDOR_ID {
                        continue;
                    }
                    any = true;
                    line!(
                        "pid 0x{:04x}  usage_page 0x{:04x} usage 0x{:04x}  iface {}  {}",
                        info.product_id(),
                        info.usage_page(),
                        info.usage(),
                        info.interface_number(),
                        info.path().to_string_lossy()
                    );
                    line!(
                        "   product: {}  |  mfr: {}",
                        info.product_string().unwrap_or("-"),
                        info.manufacturer_string().unwrap_or("-")
                    );
                    match api.open_path(info.path()) {
                        Ok(dev) => {
                            let mut feat = [0u8; 9];
                            feat[0] = 0x06;
                            match dev.get_feature_report(&mut feat) {
                                Ok(n) => line!("   feature 0x06: {n} byte(s) {}", hex(&feat[..n.min(9)])),
                                Err(e) => line!("   feature 0x06: error {e}"),
                            }
                        }
                        Err(e) => line!("   open failed: {e}"),
                    }
                }
                if !any {
                    line!("no HID device with VID 0x{:04x} present", protocol::VENDOR_ID);
                }
            }
            Err(e) => line!("hidapi init failed: {e}"),
        }
    } else if args.iter().any(|a| a == "--ble") {
        // Bluetooth mode: what Windows sees, and what is readable over GATT.
        match ble::scan() {
            Ok(devices) => {
                if devices.is_empty() {
                    line!("no Metaphyuni BLE device found (mouse off, or on 2.4G/USB mode)");
                }
                for d in devices {
                    line!("device: {} ({})", d.name, d.address);
                    line!("  connected: {}", d.connected);
                    line!(
                        "  battery: {}",
                        d.battery.map(|b| format!("{b}%")).unwrap_or_else(|| "—".into())
                    );
                    line!("  services: {}", d.services.join(", "));
                    if !d.vendor_services.is_empty() {
                        line!("  vendor services: {}", d.vendor_services.join(", "));
                    }
                    if let Some(e) = d.error {
                        line!("  error: {e}");
                    }
                }
            }
            Err(e) => line!("ble scan failed: {e}"),
        }
    } else if let Some(i) = args.iter().position(|a| a == "--fwbin") {
        // Identify the device first, if it is there. The image's IC is checked
        // against *this* product's two chips, and they differ by product — a 4K
        // receiver (CH32V305) must not be judged against the 1K receiver's
        // chip. Reading the ids costs one connection and no flashing.
        match Link::connect() {
            Ok(link) => {
                LAST_PID.store(link.product_id, Ordering::Relaxed);
                if let Some(i) = link.identity() {
                    LAST_CID.store(i.cid, Ordering::Relaxed);
                    LAST_MID.store(i.mid, Ordering::Relaxed);
                }
            }
            Err(e) => line!("device: not connected ({e}) — IC check uses the fallback product"),
        }
        // Decode a vendor firmware image. Useful because the flashing commands
        // and the bootloader's HID path live inside the file, so the file
        // itself is the contract.
        let path = match args.get(i + 1) {
            Some(p) => p.clone(),
            None => {
                line!("usage: --fwbin <image.bin>");
                return;
            }
        };
        let read = std::fs::read(&path).map_err(|e| e.to_string());
        let result = read.and_then(|bytes| {
            let name = std::path::Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.clone());
            let size = bytes.len() as u64;
            // The UI only ever sends the header; the diagnostic has the whole
            // file but the parser needs no more than that.
            let head = bytes.len().min(MIN_HEADER + 512);
            parse_image(name, size, &bytes[..head]).map_err(|e| e.to_string())
        });
        match result {
            Ok(img) => {
                line!("file:      {}", img.file_name);
                line!("size:      {} bytes (fw body {})", img.file_size, img.firmware_length);
                line!("version:   {}", img.version);
                line!("target:    {} (type {})", img.device_type_label, img.device_type);
                line!("ic name:   {}", img.ic_name);
                line!("cid/mid:   {} / {}", img.cid, img.mid);
                line!("product:   {}", img.product_name);
                line!("sensor:    {}", img.sensor_name);
                line!("head crc:  0x{:x}  head len: {}", img.head_crc, img.header_length);
                line!("boot in:   {}", img.boot_input_endpoint);
                line!("boot out:  {}", img.boot_output_endpoint);
                line!("cmd reset:    {}", img.reset_to_update_mode_cmd);
                line!("cmd prepare:  {}", img.prepare_download_cmd);
                line!("cmd download: {}", img.data_download_cmd);
                line!("verdict:   {}", img.compatibility);
            }
            Err(e) => line!("{e}"),
        }
    } else if let Some(i) = args.iter().position(|a| a == "--rate") {
        // Write one report rate and read it back. The only way to know the
        // codes are right: the device either stores the value or it does not.
        // `--rate 0` lists what the endpoint offers without writing.
        let hz: u32 = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(0);
        match Link::connect() {
            Ok(link) => {
                let allowed = protocol::rates_for(link.product_id);
                line!(
                    "pid 0x{:04x} offers: {}",
                    link.product_id,
                    allowed
                        .iter()
                        .map(|r| r.to_string())
                        .collect::<Vec<_>>()
                        .join(" / ")
                );
                let before = link.read_block(0x0000, 2, 3);
                line!(
                    "stored now: 0x{}",
                    before
                        .as_ref()
                        .and_then(|v| v.first())
                        .map(|b| format!("{b:02x}"))
                        .unwrap_or_else(|| "??".into())
                );
                if hz == 0 {
                    return;
                }
                if !allowed.contains(&hz) {
                    line!("{hz} Hz is not offered on this endpoint — not written");
                    return;
                }
                let t = std::time::Instant::now();
                match link.set_report_rate(hz) {
                    Ok(()) => {
                        let after = link.read_block(0x0000, 2, 3);
                        line!(
                            "wrote {hz} Hz in {} ms -> stored 0x{}",
                            t.elapsed().as_millis(),
                            after
                                .as_ref()
                                .and_then(|v| v.first())
                                .map(|b| format!("{b:02x}"))
                                .unwrap_or_else(|| "??".into())
                        );
                    }
                    Err(e) => line!("write failed: {e}"),
                }
            }
            Err(e) => line!("connect failed: {e}"),
        }
    } else if args.iter().any(|a| a == "--rgb") {
        // The receiver light: `p1studio --rgb` reads it, and
        // `--rgb <mode> <c1> <c2> <c3>` writes it and reads back.
        match Link::connect() {
            Ok(link) => {
                wake_patiently(&link, wait_secs(args), &mut log);
                let mode = args
                    .iter()
                    .position(|a| a == "--rgb")
                    .and_then(|i| args.get(i + 1))
                    .and_then(|v| v.parse::<u8>().ok());
                if let Some(m) = mode {
                    let cols: Vec<String> = args
                        .iter()
                        .skip_while(|a| *a != "--rgb")
                        .skip(2)
                        .take(3)
                        .map(|s| s.trim_start_matches('#').to_string())
                        .collect();
                    if cols.len() == 3 {
                        let to_rgb = |h: &String| -> [u8; 3] {
                            let mut o = [0u8; 3];
                            for i in 0..3 {
                                o[i] = u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap_or(0);
                            }
                            o
                        };
                        let rgb = protocol::DongleRgb {
                            mode: m,
                            color1: to_rgb(&cols[0]),
                            color2: to_rgb(&cols[1]),
                            color3: to_rgb(&cols[2]),
                        };
                        line!(
                            "write: mode {} c1 #{} c2 #{} c3 #{}",
                            m, protocol::DongleRgb::hex(rgb.color1),
                            protocol::DongleRgb::hex(rgb.color2),
                            protocol::DongleRgb::hex(rgb.color3)
                        );
                        match link.set_dongle_rgb(rgb) {
                            Ok(()) => line!("send: ok"),
                            Err(e) => line!("send failed: {e}"),
                        }
                    } else {
                        line!("usage: --rgb <mode> <c1rrggbb> <c2rrggbb> <c3rrggbb>");
                    }
                }
                match read_rgb_patiently(&link) {
                    Some(r) => line!(
                        "light: mode {} ({}) c1 #{} c2 #{} c3 #{}",
                        r.mode,
                        protocol::DongleRgb::mode_label(r.mode),
                        protocol::DongleRgb::hex(r.color1),
                        protocol::DongleRgb::hex(r.color2),
                        protocol::DongleRgb::hex(r.color3)
                    ),
                    None => line!("light: no answer (not a 4K receiver, or it is asleep)"),
                }
            }
            Err(e) => line!("connect failed: {e}"),
        }
    } else if args.iter().any(|a| a == "--dump") {
        // Full configuration as the app decodes it — the fastest way to see
        // which settings the device actually reports and which stay empty.
        match Link::connect() {
            Ok(link) => {
                wake_patiently(&link, wait_secs(args), &mut log);
                match read_block_retrying(&link) {
                    Ok(block) => {
                        let snap = protocol::decode(&block);
                        line!(
                            "{}",
                            serde_json::to_string_pretty(&snap)
                                .unwrap_or_else(|e| format!("serialize failed: {e}"))
                        );
                    }
                    Err(e) => line!("read failed: {e}"),
                }
            }
            Err(e) => line!("connect failed: {e}"),
        }
    } else if let Some(i) = args.iter().position(|a| a == "--pairloop") {
        // Enter pairing and then watch 0x06 for a while, so the question
        // "does the receiver ever report 1/2?" is answered by the hardware
        // instead of by guessing. `--pairloop 30` = 30 s of polling.
        let secs: u64 = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(20);
        match Link::connect() {
            Ok(link) => {
                line!("pid 0x{:04x}", link.product_id);
                let identity = link.identity();
                let (cid, mid) = identity.map(|i| (i.cid, i.mid)).unwrap_or((0, 0));
                line!("identity: cid 0x{cid:02x} mid 0x{mid:02x}");
                line!("---- before ----");
                line!("0x06 {:?}", link.pair_state_raw());
                match link.enter_pair(cid, mid) {
                    Ok(()) => line!("enter_pair(0x{cid:02x}, 0x{mid:02x}) sent"),
                    Err(e) => line!("enter_pair failed: {e}"),
                }
                let deadline = std::time::Instant::now() + Duration::from_secs(secs);
                let mut tick = 0;
                loop {
                    sleep(Duration::from_millis(600));
                    tick += 1;
                    let raw = link.pair_state_raw().unwrap_or_default();
                    line!(
                        "t+{:>4} ms 0x06 {:?} | online {}",
                        (secs * 1000).saturating_sub(deadline.saturating_duration_since(std::time::Instant::now()).as_millis() as u64),
                        &raw[..raw.len().min(2)],
                        link.read_block(0x0000, 2, 1).is_some()
                    );
                    if raw.first().copied() == Some(2) {
                        line!("BONDED");
                        break;
                    }
                    if std::time::Instant::now() >= deadline {
                        line!("window closed after {tick} polls without a bond");
                        break;
                    }
                }
            }
            Err(e) => line!("connect failed: {e}"),
        }
    } else if let Some(i) = args.iter().position(|a| a == "--rawcmd") {
        let code = args
            .get(i + 1)
            .and_then(|v| u8::from_str_radix(v.trim_start_matches("0x"), 16).ok())
            .unwrap_or(0);
        let payload = parse_hex_arg(args, "--payload");
        let listen: i32 = args
            .iter()
            .position(|a| a == "--listen")
            .and_then(|j| args.get(j + 1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(1500);

        match Link::connect() {
            Ok(link) => {
                wake_patiently(&link, wait_secs(args), &mut log);
                line!("cmd 0x{code:02x} payload [{}] listen {listen} ms", hex(&payload));
                line!("---- raw exchange ----");
                for l in link.raw_exchange(code, 0x0000, &payload, listen) {
                    line!("{}", l);
                }
            }
            Err(e) => line!("connect failed: {e}"),
        }
    } else if args.iter().any(|a| a == "--advanced") {
        // Long range and the onboard profile, read and then written and read
        // back. Kept separate from `--rate`/`--rgb` because both change how the
        // mouse behaves, and the point of the round trip is to prove the write
        // landed rather than assume it. Restores whatever it found.
        match Link::connect() {
            Ok(link) => {
                wake_patiently(&link, wait_secs(args), &mut log);
                line!("pid 0x{:04x}", link.product_id);
                if let Some(i) = link.identity() {
                    line!("identity: cid 0x{:02x} mid 0x{:02x} type 0x{:02x}",
                          i.cid, i.mid, i.device_type);
                }
                // ── long range ──
                let before = link.long_range();
                line!("long range: {:?}", before);
                if before.is_none() {
                    line!("  device does not answer 0x17 — control stays hidden in the UI");
                } else {
                    match link.set_long_range(true) {
                        Ok(()) => line!("  set on -> ok, reads back {:?}", link.long_range()),
                        Err(e) => line!("  set on FAILED: {e}"),
                    }
                    match link.set_long_range(false) {
                        Ok(()) => line!("  set off -> ok, reads back {:?}", link.long_range()),
                        Err(e) => line!("  set off FAILED: {e}"),
                    }
                }
                // ── onboard profile ──
                let cfg = link.current_config();
                line!("current config: {:?}", cfg);
                if cfg.is_none() {
                    line!("  device does not answer 0x0e — one profile, no picker");
                } else {
                    // Visit every profile the UI offers, then come home, so the
                    // whole combo is exercised rather than one step of it.
                    let home = cfg.unwrap_or(0);
                    for index in 0..protocol::ONBOARD_PROFILES as u8 {
                        match link.set_current_config(index) {
                            Ok(()) => line!("  -> {index} ok, reads back {:?}", link.current_config()),
                            Err(e) => line!("  -> {index} FAILED: {e}"),
                        }
                    }
                    match link.set_current_config(home) {
                        Ok(()) => line!("  restored -> {home}, reads back {:?}", link.current_config()),
                        Err(e) => line!("  restore FAILED: {e}"),
                    }
                }
                line!("---- note ----");
                line!("0x09 ClearSetting (factory reset) is NOT exercised here: it would");
                line!("erase every setting on the only available mouse.");
            }
            Err(e) => line!("connect failed: {e}"),
        }
    } else if args.iter().any(|a| a == "--newfeatures") {
        // The button table, angle snapping, LOD and the DPI indicator light,
        // each written and read back and then restored to whatever was found.
        // All four go through the exact functions the UI calls, so a pass here
        // says the buttons on the page will do what they claim.
        //
        // The initial read uses one link and the writes each open their own —
        // the same pattern `--sleep-auto` uses — so the handles never overlap.
        let initial = Link::connect().and_then(|link| {
            wake_patiently(&link, wait_secs(args), &mut log);
            read_block_retrying(&link)
        });
        let initial = match initial {
            Ok(block) => Some(block),
            Err(e) => {
                line!("read failed: {e}");
                None
            }
        };
        let start = initial.as_deref().map(decode);

        // ── angle snapping ──
        if let Some(start) = &start {
            line!("angle snapping: {}", start.angle_snapping);
            for want in [!start.angle_snapping, start.angle_snapping] {
                match apply(StatePatch { angle_snapping: Some(want), ..Default::default() }) {
                    Ok(a) => line!("  set {want} -> {}", a.snapshot.angle_snapping),
                    Err(e) => line!("  set {want} FAILED: {e}"),
                }
            }

            // ── LOD ──
            // Cycle every setting rather than one, because the values are not
            // ordered (1 is the middle) and a code that only round trips one
            // of them has not been round tripped.
            line!("lod: {} ({})", start.lod, start.lod_name.unwrap_or("?"));
            for probe in [protocol::LOD_LOW, protocol::LOD_HIGH, protocol::LOD_MEDIUM] {
                if probe == start.lod {
                    continue;
                }
                match apply(StatePatch { lod: Some(probe), ..Default::default() }) {
                    Ok(a) => line!(
                        "  -> {probe} reads {} ({})",
                        a.snapshot.lod,
                        a.snapshot.lod_name.unwrap_or("?")
                    ),
                    Err(e) => line!("  -> {probe} FAILED: {e}"),
                }
            }
            match apply(StatePatch { lod: Some(start.lod), ..Default::default() }) {
                Ok(a) => line!("  restored {}", a.snapshot.lod),
                Err(e) => line!("  restore FAILED: {e}"),
            }

            // ── DPI indicator light ──
            line!("dpi light: {:?}", start.dpi_light);
            for mode in [1u8, 2, 0] {
                match apply(StatePatch {
                    dpi_light_mode: Some(mode),
                    dpi_light_brightness: Some(9),
                    dpi_light_speed: Some(2),
                    ..Default::default()
                }) {
                    Ok(a) => line!(
                        "  mode {mode} -> {:?} (bright {}, speed {})",
                        a.snapshot.dpi_light.mode, a.snapshot.dpi_light.brightness, a.snapshot.dpi_light.speed
                    ),
                    Err(e) => line!("  mode {mode} FAILED: {e}"),
                }
            }
            match apply(StatePatch {
                dpi_light_mode: Some(start.dpi_light.mode),
                dpi_light_brightness: Some(start.dpi_light.brightness),
                dpi_light_speed: Some(start.dpi_light.speed),
                ..Default::default()
            }) {
                Ok(a) => line!("  restored {:?}", a.snapshot.dpi_light),
                Err(e) => line!("  restore FAILED: {e}"),
            }

            // ── buttons ──
            // Index 3 (Back) is borrowed because it is the least destructive
            // thing to lose for a second: rebound, read back, restored. The
            // left-click guard is then asked to do the one thing it exists to
            // refuse.
            for b in &start.buttons {
                line!("  btn {} {:<6} -> {}", b.index, b.name, b.action);
            }
        }
        match set_button(3, "前进".into()) {
            Ok(a) => line!("  btn 3 -> 前进 ok, reads back {}", a.snapshot.buttons[3].action),
            Err(e) => line!("  btn 3 -> 前进 FAILED: {e}"),
        }
        match set_button(3, "后退".into()) {
            Ok(a) => line!("  btn 3 -> 后退 restored, reads back {}", a.snapshot.buttons[3].action),
            Err(e) => line!("  btn 3 restore FAILED: {e}"),
        }
        match set_button(0, "禁用".into()) {
            Ok(_) => line!("  GUARD FAILED: disabling the last left click was accepted"),
            Err(e) => line!("  guard refused: {e}"),
        }

        // ── combination slot ──
        // Borrows the DPI button: rebound to Ctrl+C, read back, then put back
        // with the slot erased. Everything here is the same path the UI uses,
        // including the read-back comparison the UI turns into an error.
        line!("combo slot @0x{:04x}: before {:?}", protocol::SHORTCUT_BASE + 5 * protocol::SHORTCUT_STRIDE, combo_probe_read(5));
        match set_button_combo(5, protocol::MOD_CTRL, 0x06, false) {
            Ok(a) => line!("  btn 5 -> Ctrl+C ok, reads back {}", a.snapshot.buttons[5].action),
            Err(e) => line!("  btn 5 -> Ctrl+C FAILED: {e}"),
        }
        line!("  slot now: {:?}", combo_probe_read(5));
        match set_button(5, "DPI 循环".into()) {
            Ok(a) => line!("  btn 5 restored to {}", a.snapshot.buttons[5].action),
            Err(e) => line!("  btn 5 restore FAILED: {e}"),
        }
        match combo_probe_erase(5) {
            Ok(()) => line!("  slot erased back to {:?}", combo_probe_read(5)),
            Err(e) => line!("  slot erase FAILED: {e}"),
        }
    } else if args.iter().any(|a| a == "--verify") {
        // The import check run against the device's own block. If a real
        // configuration cannot pass it, then no backup can ever be restored,
        // so this is the one way to find that out without writing anything.
        match Link::connect() {
            Ok(link) => {
                wake_patiently(&link, wait_secs(args), &mut log);
                match read_block_retrying(&link) {
                    Ok(block) => match protocol::block_is_sane(&block) {
                        Ok(()) => line!("设备当前配置块通过导入校验（可以备份并还原）"),
                        Err(e) => line!("设备当前配置块没能通过自己的导入校验：{e}"),
                    },
                    Err(e) => line!("read failed: {e}"),
                }
            }
            Err(e) => line!("connect failed: {e}"),
        }
    } else if args.iter().any(|a| a == "--stage") {
        // Does the active DPI stage stay where it is put? compxctl reports the
        // mouse overwriting it from RAM within 5-10 s, which would make the
        // write pointless and the UI a lie. Poll the block and watch.
        match Link::connect() {
            Ok(link) => {
                wake_patiently(&link, wait_secs(args), &mut log);
                let before = read_block_retrying(&link).ok().map(|b| decode(&b).active_stage + 1);
                line!("active stage before: {before:?}");
                let target = if before == Some(1) { 2 } else { 1 };
                match apply(StatePatch { active_stage: Some(target - 1), ..StatePatch::default() }) {
                    Ok(a) => line!("wrote stage {target}, reported back as {}", a.snapshot.active_stage + 1),
                    Err(e) => line!("apply failed: {e}"),
                }
                let t0 = std::time::Instant::now();
                let mut last = None;
                for _ in 0..14 {
                    sleep(Duration::from_secs(1));
                    let now = read_block_retrying(&link).ok().map(|b| decode(&b).active_stage + 1);
                    if now != last {
                        line!("  +{} s: stage {now:?}", t0.elapsed().as_secs());
                        last = now;
                    }
                }
                if let Some(original) = before {
                    match apply(StatePatch { active_stage: Some(original - 1), ..StatePatch::default() }) {
                        Ok(_) => line!("restored stage {original}"),
                        Err(e) => line!("restore failed: {e}"),
                    }
                }
            }
            Err(e) => line!("connect failed: {e}"),
        }
    } else if let Some(i) = args.iter().position(|a| a == "--sleep") {
        // `--sleep <units>` writes the timeout through the same path the Apply
        // button uses, `--watch` then leaves the mouse alone and reports how
        // long the link stays up. That last part is the whole point: the byte
        // at 0xAD read back as 6 whether or not anything was listening to it,
        // so the only honest check is the mouse going quiet.
        let units = args
            .get(i + 1)
            .and_then(|v| v.parse::<u8>().ok())
            .unwrap_or(6);
        let watch = args.iter().any(|a| a == "--watch");
        match Link::connect() {
            Ok(link) => {
                wake_patiently(&link, wait_secs(args), &mut log);
                let before = read_block_retrying(&link)
                    .map(|b| b[protocol::SLEEP_TIME_ADDR])
                    .unwrap_or(0);
                line!("sleep timeout was {} units ({} s)", before, before as u32 * 10);
                match apply(StatePatch { sleep_timeout: Some(units), ..StatePatch::default() }) {
                    Ok(a) => {
                        line!("wrote {} frames, timeout now {} units ({} s)",
                              a.frames, a.snapshot.sleep_timeout, a.snapshot.sleep_timeout as u32 * 10);
                    }
                    Err(e) => line!("apply failed: {e}"),
                }
                if watch {
                    let t0 = std::time::Instant::now();
                    let mut slept = None;
                    while t0.elapsed() < Duration::from_secs(120) {
                        // Command 0x03 is answered by the receiver itself, so
                        // asking does not keep the mouse awake.
                        match link.mouse_online() {
                            false => {
                                slept = Some(t0.elapsed().as_secs());
                                break;
                            }
                            _ => sleep(Duration::from_secs(1)),
                        }
                    }
                    match slept {
                        Some(s) => line!("link dropped after {s} s"),
                        None => line!("still online after 120 s"),
                    }
                }
                match apply(StatePatch { sleep_timeout: Some(before), ..StatePatch::default() }) {
                    Ok(_) => line!("restored {before} units"),
                    Err(e) => line!("restore failed: {e}"),
                }
            }
            Err(e) => line!("connect failed: {e}"),
        }
    } else if let Some(i) = args.iter().position(|a| a == "--macro") {
        // Write one macro, then either undo it or leave it in place.
        //
        // `--scroll` swaps in two ScrollLock taps, which is the shape to use
        // when a human is going to press the button while a keyboard hook is
        // listening: ScrollLock shows up in the hook and does nothing else,
        // whereas a Ctrl+C macro would copy whatever happens to be focused.
        let index: usize = args
            .get(i + 1)
            .and_then(|v| v.parse().ok())
            .unwrap_or(5);
        let keep = args.iter().any(|a| a == "--keep");
        let scroll = args.iter().any(|a| a == "--scroll");
        // `--key 0x73` picks any keyboard usage: F24 is the useful one for a
        // replay test, because no keyboard has the key and so nothing a person
        // types can be mistaken for it.
        let key_usage = args
            .iter()
            .position(|a| a == "--key")
            .and_then(|j| args.get(j + 1))
            .and_then(|v| u16::from_str_radix(v.trim_start_matches("0x"), 16).ok());
        let steps = if let Some(usage) = key_usage {
            vec![
                protocol::MacroStep { modifiers: 0, usage, delay_ms: 250, keyboard: true },
                protocol::MacroStep { modifiers: 0, usage, delay_ms: 250, keyboard: true },
            ]
        } else if scroll {
            vec![
                protocol::MacroStep { modifiers: 0, usage: 0x47, delay_ms: 120, keyboard: true },
                protocol::MacroStep { modifiers: 0, usage: 0x47, delay_ms: 120, keyboard: true },
            ]
        } else {
            vec![
                protocol::MacroStep { modifiers: protocol::MOD_CTRL, usage: 0x06, delay_ms: 120, keyboard: true },
                protocol::MacroStep { modifiers: 0, usage: 0x04, delay_ms: 40, keyboard: true },
            ]
        };
        let name = if key_usage.is_some() { "F24 测试" } else if scroll { "回放测试" } else { "测试宏" };
        let old_record = with_link(|link| {
            require_awake(link, 6)?;
            let block = read_block_retrying(link)?;
            let addr = protocol::key_record_addr(index);
            Ok(block[addr..addr + 4].to_vec())
        });
        match &old_record {
            Ok(r) => line!("button {index} was {}", hex(r)),
            Err(e) => line!("button {index} read failed: {e}"),
        }
        match read_macro(index as u8) {
            Ok(before) => line!("macro slot {index} before: {before:?}"),
            Err(e) => line!("macro read failed: {e}"),
        }
        match set_button_macro(index as u8, name.into(), steps, 1) {
            Ok(a) => line!("  btn {index} -> macro ok, reads back {}", a.snapshot.buttons[index].action),
            Err(e) => line!("  btn {index} -> macro FAILED: {e}"),
        }
        match read_macro(index as u8) {
            Ok(Some(v)) => {
                line!("  slot {index}: name={:?} events={} checksum_ok={}", v.name, v.events, v.checksum_ok);
                for step in &v.steps {
                    line!("    {step}");
                }
            }
            Ok(None) => line!("  slot {index}: <no macro>"),
            Err(e) => line!("  slot {index} read failed: {e}"),
        }
        if keep {
            line!("  left in place (--keep)");
        } else if let Ok(old) = old_record {
            // Clearing the region to zeros leaves no macro behind: count 0
            // decodes as "nothing here", and zeros can always be rewritten.
            let restored = with_link(|link| {
                let base = protocol::MACRO_BASE + index * protocol::MACRO_STRIDE;
                let before = link
                    .read_range(base as u16, (base + protocol::MACRO_STRIDE) as u16)
                    .ok();
                link.write_chunks(base as u16, &[0u8; protocol::MACRO_STRIDE], before.as_deref())?;
                let block = read_block_retrying(link)?;
                let mut next = block.clone();
                next[protocol::key_record_addr(index)..protocol::key_record_addr(index) + 4]
                    .copy_from_slice(&old);
                commit(link, &next, &block).map(|_| ())
            });
            match restored {
                Ok(()) => line!("  restored button {index} and cleared the slot"),
                Err(e) => line!("  restore FAILED: {e}"),
            }
        }
    } else if args.iter().any(|a| a == "--commit") {
        // Proof for the commit-frame fix. A zero-length write is refused by this
        // receiver at every address, so the commit used to be dropped while the
        // app reported success: the staged records never reached flash and only
        // looked right because reads answer from RAM. This prints the accept
        // flag for both shapes so the difference can be seen without a GUI.
        match Link::connect() {
            Ok(link) => {
                let _ = link.ensure_reads(6);
                let addr = protocol::WRITE_COMMIT_ADDR;
                for (label, payload) in [
                    ("empty payload (the old, refused one)", Vec::new()),
                    ("one 0x00 byte (what the app sends now)", vec![0u8]),
                    ("one 0x01 byte", vec![1u8]),
                ] {
                    match link.exchange(protocol::cmd::WRITE, addr, &payload, 600) {
                        None => line!("commit {label}: no reply"),
                        Some(r) if r.rejected => line!("commit {label}: REJECTED"),
                        Some(_) => line!("commit {label}: accepted"),
                    }
                    sleep(Duration::from_millis(400));
                }
            }
            Err(e) => line!("connect failed: {e}"),
        }
    } else if let Some(i) = args.iter().position(|a| a == "--cmd") {
        let code = args
            .get(i + 1)
            .and_then(|v| u8::from_str_radix(v.trim_start_matches("0x"), 16).ok())
            .unwrap_or(0);
        match Link::connect() {
            Ok(link) => {
                line!("cmd 0x{code:02x} on pid=0x{:04x}", link.product_id);
                let _ = link.ensure_reads(4);
                match link.command(code, 0x0000, &[], 600) {
                    Some(payload) => line!("reply payload: {}", hex(&payload)),
                    None => line!("no reply"),
                }
            }
            Err(e) => line!("connect failed: {e}"),
        }
    } else {
        match Link::connect() {
            Ok(link) => {
                line!("receiver pid=0x{:04x}", link.product_id);
                let woke = link.ensure_reads(8);
                line!("awake: {woke}");
                if woke {
                    let t = std::time::Instant::now();
                    match link.read_range(0, CONFIG_END as u16) {
                        Ok(block) => {
                            line!("read 192 bytes in {} ms", t.elapsed().as_millis());
                            line!("head: {}", hex(&block[..12]));
                            line!("battery: {:?}", link.battery());
                            line!("config: {:?}", decode(&block));
                        }
                        Err(e) => line!("read failed: {e}"),
                    }
                }
            }
            Err(e) => line!("connect failed: {e}"),
        }
    }

    line!("total {} ms", started.elapsed().as_millis());
    let path = std::env::temp_dir().join("p1studio-probe.log");
    let _ = std::fs::write(&path, &log);
}

/// Space-separated hex, for traces and dumps. Shared with `hid.rs`.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}
