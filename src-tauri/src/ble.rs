//! Bluetooth LE side of the mouse.
//!
//! In Bluetooth mode the mouse is no longer behind the 2.4 GHz receiver, so
//! none of the vendor HID commands reach it: it shows up on the PC as a
//! standard HID-over-GATT device ("MpandaMouse PRO", VID 0x3554 / PID 0xf533
//! on the bench) with the usual GATT services — notably the standard Battery
//! Service (0x180F), which is the one thing that is readable over BLE.
//!
//! Everything below is read-only and best effort. If Windows will not give us
//! the services (device busy, radio off, not bonded) the fields stay empty
//! rather than being invented.

#![cfg(target_os = "windows")]

use futures::executor::block_on;
use std::future::IntoFuture;
use crate::protocol;
use hidapi::HidApi;
use windows::core::HSTRING;
use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCharacteristicUuids, GattDeviceService,
};
use windows::Devices::Bluetooth::{BluetoothCacheMode, BluetoothLEDevice};
use windows::Devices::Enumeration::DeviceInformation;
use windows::Storage::Streams::DataReader;

/// Every BLE device Windows knows about (GATT protocol id), narrowed to the
/// ones that look like ours.
const GATT_AQS: &str =
    r#"System.Devices.Aep.ProtocolId:="{bb7bb05e-5972-42b5-94fc-76eaa7084d49}""#;
/// VID 0x3554 as Windows spells it in the device id (decimal, zero padded).
const VID_TOKEN: &str = "VID&023554";

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BleDevice {
    pub name: String,
    pub id: String,
    pub address: String,
    pub battery: Option<u8>,
    pub connected: bool,
    /// Standard services we recognised, e.g. "0x180f (battery)".
    pub services: Vec<String>,
    /// Non-standard (vendor) services — a configuration channel would live here.
    pub vendor_services: Vec<String>,
    pub error: Option<String>,
}

/// WinRT async operations are `IntoFuture`, not `Future` — unwrap one into a
/// plain blocking call so the scan stays a synchronous command.
fn wait<T>(op: windows_future::IAsyncOperation<T>) -> windows::core::Result<T>
where
    T: windows::core::RuntimeType,
    windows_future::IAsyncOperation<T>: IntoFuture<Output = windows::core::Result<T>>,
{
    block_on(op.into_future())
}

/// Bluetooth mice show up in the HID layer first (HID-over-GATT), and their
/// device path carries both the VID/PID and the MAC — e.g.
/// `..._Dev_VID&023554_PID&f533_REV&0001_d664ebb05200&Col01#...`.
/// That is a far more reliable handle than guessing at BLE advertisement names,
/// which the OS may not even expose once the device is bonded.
fn bluetooth_macs_from_hid() -> Vec<(String, u64, String)> {
    let mut out = Vec::new();
    let Ok(api) = HidApi::new() else { return out };
    for info in api.device_list() {
        if info.vendor_id() != protocol::VENDOR_ID {
            continue;
        }
        let path = info.path().to_string_lossy().to_lowercase();
        // Only the HID-over-GATT device carries the 0x1812 service in its path.
        if !path.contains("00001812-") {
            continue;
        }
        if let Some((mac, pid)) = parse_hogp_path(&path) {
            if let Some(addr) = mac_to_bluetooth_address(&mac) {
                if !out.iter().any(|(_, a, _)| *a == addr) {
                    out.push((format!("{pid}"), addr, mac));
                }
            }
        }
    }
    out
}

/// Pull the MAC and PID out of a HID-over-GATT device path. The path is a pile
/// of `_`/`&`/`#` separated chunks, so scan for the shapes instead of counting
/// separators: the MAC is the only 12-digit hex run, the PID follows `pid&`.
fn parse_hogp_path(path: &str) -> Option<(String, String)> {
    let mac = path
        .split(|c| c == '_' || c == '&' || c == '#')
        .find(|seg| seg.len() == 12 && seg.chars().all(|c| c.is_ascii_hexdigit()))?
        .to_string();
    let pid = path
        .split("pid&")
        .nth(1)?
        .split(|c| c == '_' || c == '&' || c == '#')
        .next()?
        .to_string();
    Some((mac, pid))
}

/// Windows spells a BLE address as the six MAC bytes in reverse order.
fn mac_to_bluetooth_address(mac: &str) -> Option<u64> {
    let mut bytes = [0u8; 6];
    for i in 0..6 {
        bytes[i] = u8::from_str_radix(&mac[i * 2..i * 2 + 2], 16).ok()?;
    }
    let mut addr = 0u64;
    for (i, byte) in bytes.iter().enumerate() {
        addr |= (*byte as u64) << (8 * i);
    }
    Some(addr)
}

/// Battery level via the GATT service device interface.
///
/// `BluetoothLEDevice::FromBluetoothAddressAsync` returns null for a desktop
/// app when the peripheral is not currently associated, but Windows still
/// creates a device interface per GATT service — and opening *that* needs no
/// pairing dance. So enumerate the battery service (0x180F) interfaces, keep the
/// one whose device id carries our MAC, and read the standard level (0x2A19).
fn battery_via_service(mac: &str) -> Option<(u8, Vec<String>)> {
    let battery_service = windows::Devices::Bluetooth::GenericAttributeProfile::GattServiceUuids::Battery().ok()?;
    let battery_char = GattCharacteristicUuids::BatteryLevel().ok()?;
    let selector = GattDeviceService::GetDeviceSelectorFromUuid(battery_service).ok()?;
    let collection = wait(DeviceInformation::FindAllAsyncAqsFilter(&selector).ok()?).ok()?;
    let count = collection.Size().ok()?;
    let mut services = Vec::new();
    let mut level = None;
    for i in 0..count {
        let Ok(info) = collection.GetAt(i) else { continue };
        let id = info.Id().map(|v| text(&v)).unwrap_or_default();
        if !mac.is_empty() && !id.to_lowercase().contains(mac) {
            continue;
        }
        services.push("0x180f (battery)".to_string());
        let service = match wait(GattDeviceService::FromIdAsync(&HSTRING::from(&id)).ok()?) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let chars = match wait(service.GetCharacteristicsAsync().ok()?) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let Ok(chars) = chars.Characteristics() else { continue };
        let Ok(ccount) = chars.Size() else { continue };
        for j in 0..ccount {
            let Ok(ch) = chars.GetAt(j) else { continue };
            if ch.Uuid().ok()? != battery_char {
                continue;
            }
            if let Ok(res) = wait(ch.ReadValueWithCacheModeAsync(BluetoothCacheMode::Uncached).ok()?) {
                if let Ok(buffer) = res.Value() {
                    if let Ok(reader) = DataReader::FromBuffer(&buffer) {
                        if let Ok(v) = reader.ReadByte() {
                            level = Some(v);
                        }
                    }
                }
            }
        }
    }
    level.map(|v| (v, services))
}

fn text(v: &windows::core::HSTRING) -> String {
    v.to_string_lossy()
}

fn is_ours(name: &str, id: &str) -> bool {
    let n = name.to_lowercase();
    // The mouse advertises itself as "MpandaMouse PRO" (玄派 OEM name); the
    // vendor id in the device instance path is the other reliable signal.
    id.to_uppercase().contains(VID_TOKEN)
        || n.contains("mpandamouse")
        || n.contains("metaphyuni")
        || n.contains("玄派")
        || n.contains("p1")
        || n.contains("m1")
        || n.contains("nrf")
}

/// One BLE device: which services it exposes and, if the standard battery
/// service is there, the battery level.
fn probe(device: &BluetoothLEDevice) -> BleDevice {
    let mut out = BleDevice {
        name: device.Name().map(|v| text(&v)).unwrap_or_default(),
        id: device.DeviceId().map(|v| text(&v)).unwrap_or_default(),
        address: format!("{:012X}", device.BluetoothAddress().unwrap_or(0)),
        battery: None,
        connected: device.ConnectionStatus().map(|s| s.0 == 1).unwrap_or(false),
        services: Vec::new(),
        vendor_services: Vec::new(),
        error: None,
    };

    let battery_service = match windows::Devices::Bluetooth::GenericAttributeProfile::GattServiceUuids::Battery() {
        Ok(g) => g,
        Err(e) => {
            out.error = Some(format!("battery uuid: {e}"));
            return out;
        }
    };
    let battery_char = match GattCharacteristicUuids::BatteryLevel() {
        Ok(g) => g,
        Err(e) => {
            out.error = Some(format!("battery level uuid: {e}"));
            return out;
        }
    };

    let services = match device.GetGattServicesWithCacheModeAsync(BluetoothCacheMode::Uncached) {
        Ok(op) => match wait(op) {
            Ok(r) => match r.Services() {
                Ok(v) => Some(v),
                Err(e) => {
                    out.error = Some(format!("services: {e}"));
                    None
                }
            },
            Err(e) => {
                out.error = Some(format!("services: {e}"));
                None
            }
        },
        Err(e) => {
            out.error = Some(format!("services: {e}"));
            None
        }
    };

    if let Some(services) = services {
        if let Ok(count) = services.Size() {
            for i in 0..count {
                let Ok(service) = services.GetAt(i) else { continue };
                let Ok(uuid) = service.Uuid() else { continue };
                // Bluetooth SIG UUIDs are 0000xxxx-0000-1000-8000-00805f9b34fb,
                // so the short form lives in the low half of data1.
                let is_sig = uuid.data2 == 0 && uuid.data3 == 0;
                let short = (uuid.data1 & 0xffff) as u16;
                let label = if is_sig {
                    format!("0x{short:04x}")
                } else {
                    format!("{:08x}-{:04x}", uuid.data1, uuid.data2)
                };
                if uuid == battery_service {
                    out.services.push(format!("{label} (battery)"));
                } else if is_sig {
                    out.services.push(label);
                } else {
                    out.vendor_services.push(label);
                }

                // Read the battery level from whichever service carries it.
                let Ok(chars) = service
                    .GetCharacteristicsAsync()
                    .map(wait)
                    .and_then(|r| r)
                else {
                    continue;
                };
                let Ok(chars) = chars.Characteristics() else { continue };
                let Ok(ccount) = chars.Size() else { continue };
                for j in 0..ccount {
                    let Ok(ch) = chars.GetAt(j) else { continue };
                    let Ok(cuuid) = ch.Uuid() else { continue };
                    if cuuid != battery_char {
                        continue;
                    }
                    if let Ok(op) = ch.ReadValueWithCacheModeAsync(BluetoothCacheMode::Uncached) {
                        if let Ok(res) = wait(op) {
                            if let Ok(buffer) = res.Value() {
                                if let Ok(reader) = DataReader::FromBuffer(&buffer) {
                                    if let Ok(level) = reader.ReadByte() {
                                        out.battery = Some(level);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

/// Enumerate BLE devices and probe the ones that look like this mouse.
pub fn scan() -> Result<Vec<BleDevice>, String> {
    // Preferred route: the HID layer already knows the exact radio address.
    let known = bluetooth_macs_from_hid();
    if let Some(v) = std::env::var_os("P1STUDIO_BLE_TRACE") {
        if !v.is_empty() {
            eprintln!("[ble] hid discovered: {known:?}");
        }
    }
    let mut found = Vec::new();
    let mut failures = Vec::new();
    for (pid, address, _mac) in &known {
        let address = *address;
        match BluetoothLEDevice::FromBluetoothAddressAsync(address) {
            Ok(op) => match wait(op) {
                Ok(device) => {
                    let mut d = probe(&device);
                    if d.id.is_empty() {
                        d.id = format!("BLE {pid}");
                    }
                    if d.error.is_none() {
                        found.push(d);
                    }
                }
                Err(e) => failures.push(BleDevice {
                    name: format!("蓝牙设备 {pid}"),
                    id: format!("address 0x{address:012X}"),
                    address: format!("{address:012X}"),
                    battery: None,
                    connected: false,
                    services: Vec::new(),
                    vendor_services: Vec::new(),
                    error: Some(format!("BluetoothLEDevice 打开失败：{e}")),
                }),
            },
            Err(e) => failures.push(BleDevice {
                name: format!("蓝牙设备 {pid}"),
                id: format!("address 0x{address:012X}"),
                address: format!("{address:012X}"),
                battery: None,
                connected: false,
                services: Vec::new(),
                vendor_services: Vec::new(),
                error: Some(format!("BluetoothLEDevice 打开失败：{e}")),
            }),
        }
    }
    if !found.is_empty() {
        return Ok(found);
    }

    // Fallback: walk every BLE device the OS knows. The HID layer told us the
    // MAC, so match on that; the friendly name is not always available.
    let op = DeviceInformation::FindAllAsyncAqsFilter(&HSTRING::from(GATT_AQS))
        .map_err(|e| format!("枚举 BLE 设备失败：{e}"))?;
    let collection = wait(op).map_err(|e| format!("枚举 BLE 设备失败：{e}"))?;
    let count = collection.Size().map_err(|e| format!("{e}"))?;
    let macs: Vec<String> = known.iter().map(|(_, _, m)| m.to_lowercase()).collect();

    let mut found = Vec::new();
    for i in 0..count {
        let Ok(info) = collection.GetAt(i) else { continue };
        let name = info.Name().map(|v| text(&v)).unwrap_or_default();
        let id = info.Id().map(|v| text(&v)).unwrap_or_default();
        if std::env::var_os("P1STUDIO_BLE_TRACE").is_some() && i < 25 {
            eprintln!("[ble] cand #{i}: name={name:?} id={id:?}");
        }
        let id_lc = id.to_lowercase();
        let hit_mac = macs.iter().any(|m| id_lc.contains(m));
        if !hit_mac && !is_ours(&name, &id) {
            continue;
        }
        let mac = macs.iter().find(|m| id.to_lowercase().contains(*m)).cloned().unwrap_or_default();
        match BluetoothLEDevice::FromIdAsync(&HSTRING::from(&id)) {
            Ok(op) => match wait(op) {
                Ok(device) => {
                    let mut d = probe(&device);
                    // Opening the device object is not always allowed; the
                    // per-service interface still is, so fall back to it.
                    if d.battery.is_none() {
                        if let Some((level, svcs)) = battery_via_service(&mac) {
                            d.battery = Some(level);
                            for s in svcs {
                                if !d.services.contains(&s) {
                                    d.services.push(s);
                                }
                            }
                            d.error = None;
                        }
                    }
                    found.push(d);
                }
                Err(e) => found.push(BleDevice {
                    name,
                    id,
                    address: String::new(),
                    battery: None,
                    connected: false,
                    services: Vec::new(),
                    vendor_services: Vec::new(),
                    error: Some(format!("打开设备失败：{e}")),
                }),
            },
            Err(e) => found.push(BleDevice {
                name,
                id,
                address: String::new(),
                battery: None,
                connected: false,
                services: Vec::new(),
                vendor_services: Vec::new(),
                error: Some(format!("打开设备失败：{e}")),
            }),
        }
    }
    if found.is_empty() {
        // Nothing through the device object, but the HID layer proves the mouse
        // is there — report it with whatever the service interface can read.
        for (pid, _addr, mac) in &known {
            let mut d = BleDevice {
                name: match pid.as_str() {
                    // The vendor advertisement name, as shown in Windows' device list.
                    "f533" => "MpandaMouse PRO".to_string(),
                    _ => format!("玄派鼠标（蓝牙 PID {pid}）"),
                },
                id: format!("BLE {mac}"),
                address: mac.to_uppercase(),
                battery: None,
                connected: true,
                services: Vec::new(),
                vendor_services: Vec::new(),
                error: None,
            };
            if let Some((level, svcs)) = battery_via_service(mac) {
                d.battery = Some(level);
                d.services = svcs;
            } else {
                d.error = Some("电池服务不可用（鼠标可能未在蓝牙档或已断开）".to_string());
            }
            found.push(d);
        }
    }
    if found.is_empty() {
        return Ok(failures);
    }
    Ok(found)
}
