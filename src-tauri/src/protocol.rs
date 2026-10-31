//! Metaphyuni / 玄派 vendor protocol — ported from the TypeScript driver.
//!
//! Everything here was verified byte-for-byte against a live P1 Ultra 4K
//! receiver (VID 0x3554, PID 0xF5AC) and cross-checked against the vendor's own
//! `HIDUsb.dll`, two other CompX-based vendor apps, and a USBPcap capture.
//!
//! ── Frame (17 bytes, HID report id 0x08, usage page 0xFF02) ──────────────
//!
//!   [0]  0x08      report id
//!   [1]  cmd       command code
//!   [2]  flag      0x00 normal, 0x01 the device rejected the request
//!   [3]  addr hi   address, big endian
//!   [4]  addr lo
//!   [5]  len       payload length (0..10)
//!   [6..15]        payload
//!   [16] chk       (0x55 - sum(bytes[0..15])) & 0xff
//!
//! ── Command codes ──────────────────────────────────────────────────────
//!
//! The official driver names these in a plain `UsbCommandID` enum, so every
//! code below is taken from that list rather than guessed. Two of them were
//! labelled wrongly here before it was recovered: 0x09 was recorded as a
//! pairing-status read when it is in fact `ClearSetting` (factory reset), and
//! 0x0e/0x17 were recorded as write-commit codes when they are configuration
//! and long-range reads. Nothing ever sent 0x09, and that is a small mercy.
//!
//!   EncryptionData = 1, PCDriverStatus = 2, DeviceOnLine = 3,
//!   BatteryLevel = 4, DongleEnterPair = 5, GetPairState = 6,
//!   WriteFlashData = 7, ReadFlashData = 8, ClearSetting = 9,
//!   StatusChanged = 10, SetDeviceVidPid = 11, SetDeviceDescriptorString = 12,
//!   EnterUsbUpdateMode = 13, GetCurrentConfig = 14, SetCurrentConfig = 15,
//!   ReadCIDMID = 16, EnterMTKMode = 17, ReadVersionID = 18,
//!   Set4KDongleRGB = 20, Get4KDongleRGBValue = 21, SetLongRangeMode = 22,
//!   GetLongRangeMode = 23, WriteKBCIdMID = 240, ReadKBCIdMID = 241
//!
//! Reads and writes use DIFFERENT command codes. The earlier TypeScript driver
//! sent both as 0x08, which is why the device acknowledged writes but never
//! stored them: on the wire a "write" was indistinguishable from a read.
//!
//! ── The accept/reject flag ─────────────────────────────────────────────
//!
//! Byte 2 of a reply is 0x00 when the device took the frame and 0x01 when it
//! refused it. That is not inference: writing the ten-byte RGB payload answers
//! flag 0x00 and writing an empty one to the same command answers 0x01, and
//! `SetLongRangeMode` behaves exactly the same way — ten bytes accepted, one
//! byte refused. Anything that writes a vendor command should check this byte
//! instead of assuming the write landed.
//!
//! ── Reading ─────────────────────────────────────────────────────────────
//!
//! A `len = 10` request answers with ten real bytes, so the 192-byte
//! configuration block costs 20 round trips instead of 96. A `len = 2` request
//! answers with two. Verified on hardware:
//!
//!   request   08 08 00 00 00 0a | 02 02 00 00 00 00 00 00 00 00 | chk
//!   reply     08 08 00 00 00 0a | 20 35 07 4e 03 52 00 55 00 55 | 92
//!
//! ── Writing ─────────────────────────────────────────────────────────────
//!
//! Four bytes per frame at the record address, then ONE commit frame at
//! [`WRITE_COMMIT_ADDR`]. Recovered from the vendor DLL's own writer, and
//! verified by writing through it on this device: setting the sleep fields moved
//! exactly four bytes (0x0B5..0x0B8) and the vendor parser read them back.
//!
//! ── Configuration records ───────────────────────────────────────────────
//!
//! Every 4-byte record is `[v0][v1][v2][chk]` with `chk = (0x55 - v0 - v1 - v2)`.
//! The MouseConfig tail (0x00A9..0x00BD) uses `[value][0x55 - value]` pairs.

use serde::{Deserialize, Serialize};

/// Vendor id shared by every Metaphyuni product.
pub const VENDOR_ID: u16 = 0x3554;
// ── Product catalogue ────────────────────────────────────────────────────
//
// One table, keyed by product id, holding everything the UI needs to say which
// device it is talking to. It follows the shape FerrisPulse uses in
// `UniversalGamingDeviceHelper.AddProduct(vid, pid, brand, model, category,
// maxPoll)`: the brand and the model are separate fields, and the catalogue
// records what the hardware can actually do rather than what its name suggests.
//
// Before this existed, the name came from a `match pid` whose result was
// hard-coded in the corner of the HTML as well — so plugging in a different
// receiver left the app still calling itself a P1 Ultra 4K. Everything visible
// now reads from this table.

/// A row of the catalogue: what this product id is, and what it can do.
pub struct Product {
    pub pid: u16,
    /// Report rates this endpoint has actually been measured with, in Hz.
    ///
    /// This is separate from [`Product::max_rate_hz`]: the catalogue answers
    /// "how fast can this product go", this answers "which of those settings
    /// has someone actually written to this endpoint and read back". Values
    /// came from the FerrisPulse Metapanda adapter, which captured six rates
    /// from the official driver and confirmed each one on a real F5AC
    /// receiver; every other id keeps the two rates that were verified there.
    ///
    /// The UI offers only what is listed. An unverified 8000 Hz button is how
    /// a mouse ends up at a rate nobody has ever seen it report.
    pub rates: &'static [u32],
    /// The maker, e.g. "玄熊猫". FerrisPulse keeps this separate from the model.
    pub brand: &'static str,
    /// The model, e.g. "P1 Pro Max".
    pub model: &'static str,
    /// How this id is attached: wired, or 2.4G through a receiver.
    pub mode: &'static str,
    /// True when this id is the receiver end rather than the mouse itself.
    pub receiver: bool,
    /// Fastest report rate this id carries, in Hz.
    ///
    /// The 4K receiver really is faster; the plain P1 ids are not. Listing the
    /// rate per product instead of assuming the best one keeps the app from
    /// advertising 4000 Hz to a mouse that tops out at 1000.
    pub max_rate_hz: u32,
    /// Mouse MCU, from the vendor's own `Config.ini` (base64-obfuscated `MM`).
    /// A firmware image declares which IC it is for, and this is what it is
    /// checked against — flashing a CX52650N image to a CX52850P is how a
    /// mouse stops being a mouse.
    pub mcu_mouse: &'static str,
    /// Receiver MCU (`DM` / `D2M` / `D4M`, depending on the receiver generation).
    pub mcu_dongle: &'static str,
}

/// The MCU every P1 variant is built on. Same for all of them: Config.ini
/// names `MM=CX52850P` (mouse) and `DM=CX52650N` (receiver) for both device
/// entries, and `D4M=CH32V305` for the 4K receiver.
const MCU_MOUSE: &str = "CX52850P";
const MCU_DONGLE_1K: &str = "CX52650N";
const MCU_DONGLE_4K: &str = "CH32V305";

/// Report rates verified per endpoint generation.
///
/// The 4K receiver carries all six. Everything else keeps the two that were
/// confirmed on the 1K air interface and the wired endpoint — the adapter
/// notes the wired one was never exercised at 2000/4000 Hz, so those are not
/// offered here rather than assumed.
const RATES_1K: &[u32] = &[125, 1000];
const RATES_4K: &[u32] = &[125, 250, 500, 1000, 2000, 4000];

pub const PRODUCTS: &[Product] = &[
    Product {
        pid: 0xf5ac,
        rates: RATES_4K,
        brand: "玄熊猫",
        model: "P1 Ultra 4K",
        mode: "2.4G · 4K 接收器",
        receiver: true,
        max_rate_hz: 4000,
        mcu_mouse: MCU_MOUSE,
        mcu_dongle: MCU_DONGLE_4K,
    },
    Product {
        pid: 0xf5ab,
        rates: RATES_1K,
        brand: "玄熊猫",
        model: "P1 Ultra 4K",
        mode: "USB 有线",
        receiver: false,
        max_rate_hz: 4000,
        mcu_mouse: MCU_MOUSE,
        mcu_dongle: MCU_DONGLE_4K,
    },
    Product {
        // Seen in the vendor config; the same 4K product on the 1K air interface.
        pid: 0x0021,
        rates: RATES_1K,
        brand: "玄熊猫",
        model: "P1 Ultra 4K",
        mode: "2.4G",
        receiver: true,
        max_rate_hz: 4000,
        mcu_mouse: MCU_MOUSE,
        mcu_dongle: MCU_DONGLE_4K,
    },
    Product {
        pid: 0xf533,
        rates: RATES_1K,
        brand: "玄熊猫",
        model: "P1 Ultra 4K",
        mode: "2.4G",
        receiver: true,
        max_rate_hz: 4000,
        mcu_mouse: MCU_MOUSE,
        mcu_dongle: MCU_DONGLE_4K,
    },
    Product {
        // From the official driver's Config.ini, which lists the receiver ids
        // as F5A2, F5A4, F5A9, F5AC — F5A4 was missing here.
        pid: 0xf5a4,
        rates: RATES_1K,
        brand: "玄熊猫",
        model: "P1",
        mode: "2.4G",
        receiver: true,
        max_rate_hz: 1000,
        mcu_mouse: MCU_MOUSE,
        mcu_dongle: MCU_DONGLE_1K,
    },
    Product {
        pid: 0xf5a9,
        rates: RATES_1K,
        brand: "玄熊猫",
        model: "P1 Pro Max",
        mode: "2.4G",
        receiver: true,
        max_rate_hz: 1000,
        mcu_mouse: MCU_MOUSE,
        mcu_dongle: MCU_DONGLE_1K,
    },
    Product {
        pid: 0xf521,
        rates: RATES_1K,
        brand: "玄熊猫",
        model: "P1 Pro Max",
        mode: "USB 有线",
        receiver: false,
        max_rate_hz: 1000,
        mcu_mouse: MCU_MOUSE,
        mcu_dongle: MCU_DONGLE_1K,
    },
    Product {
        pid: 0xf5a2,
        rates: RATES_1K,
        brand: "玄熊猫",
        model: "P1",
        mode: "2.4G",
        receiver: true,
        max_rate_hz: 1000,
        mcu_mouse: MCU_MOUSE,
        mcu_dongle: MCU_DONGLE_1K,
    },
    Product {
        pid: 0xf5a3,
        rates: RATES_1K,
        brand: "玄熊猫",
        model: "P1",
        mode: "USB 有线",
        receiver: false,
        max_rate_hz: 1000,
        mcu_mouse: MCU_MOUSE,
        mcu_dongle: MCU_DONGLE_1K,
    },
];

/// Shown when a CompX id we do not know is plugged in. Keeping it in the same
/// shape as the catalogue means the UI never has to special-case it.
pub const UNKNOWN_PRODUCT: Product = Product {
    pid: 0,
    rates: RATES_1K,
    brand: "Metaphyuni",
    model: "未知设备",
    mode: "未知模式",
    receiver: false,
    max_rate_hz: 1000,
    mcu_mouse: MCU_MOUSE,
    mcu_dongle: MCU_DONGLE_1K,
};

/// Look a product id up in the catalogue.
pub fn product_for(pid: u16) -> &'static Product {
    PRODUCTS.iter().find(|p| p.pid == pid).unwrap_or(&UNKNOWN_PRODUCT)
}

/// Does this product id speak the protocol? `Link::connect` filters on it.
///
/// This used to hold only the three P1 Ultra 4K ids, which is why a P1 Pro Max
/// (0xf5a9) was reported as "no receiver found" even with the mouse working:
/// the device was there, the catalogue was not.
pub fn is_supported_pid(pid: u16) -> bool {
    PRODUCTS.iter().any(|p| p.pid == pid)
}
/// Usage page of the 17-byte command channel (col05).
pub const CMD_USAGE_PAGE: u16 = 0xff02;
/// Usage within [`CMD_USAGE_PAGE`] that is the real command channel.
///
/// The reference implementation accepts an endpoint only when the page, this
/// usage and a 17-byte report length match. Several devices share the vendor
/// id and expose other collections on the same page, and a command sent to
/// one of those is simply dropped.
pub const CMD_USAGE: u16 = 2;
/// Usage page of the read-only status snapshot (col06), FEATURE report 0x06.
pub const STATUS_USAGE_PAGE: u16 = 0xff04;
/// Usage page of the 49-byte report channel (col07).
pub const AUX_USAGE_PAGE: u16 = 0xff06;
/// HID report id carried by every command frame.
pub const REPORT_ID: u8 = 0x08;
/// Report id of the constant battery snapshot on col06 / usage page 0xFF04.
pub const STATUS_REPORT_ID: u8 = 0x06;
/// Full command frame size.
pub const FRAME_SIZE: usize = 17;
/// Max bytes one 0x08 frame can carry.
pub const MAX_PAYLOAD: usize = 10;
/// Checksum base the device uses.
pub const CHECKSUM_BASE: u8 = 0x55;

pub mod cmd {
    /// SetEEPROM — the only command that actually writes a configuration record.
    pub const WRITE: u8 = 0x07;
    /// GetEEPROM — the read command (also the reply code for reads).
    pub const READ: u8 = 0x08;
    /// GetBatteryLevel (0x04). Armed with the payload `5a 00` the vendor DLL
    /// uses this same code as its "prepare to write flash" step, which is why
    /// two names existed for one byte; this app only ever reads the battery, so
    /// only one name survives here.
    pub const BATTERY: u8 = 0x04;
    /// DeviceOnLine — is a mouse on the far end of the link.
    pub const POLL: u8 = 0x03;
    /// GetMouseVersion — answers with the mouse firmware version.
    pub const VERSION_MOUSE: u8 = 0x12;
    /// GetSlaveVersion — a second opinion on the receiver's firmware, and an
    /// unreliable one.
    ///
    /// Measured on two receivers:
    ///   * P1 Ultra 4K receiver (0xf5ac): answers v1.26, matching its bcdDevice
    ///     exactly.
    ///   * P1 Pro Max receiver (0xf5a9): answers nothing at all.
    /// So it agrees with the descriptor where it works and is silent where it
    /// does not, which is why the app shows the bcdDevice value and only prints
    /// this one alongside it in the diagnostic.
    pub const VERSION_DONGLE: u8 = 0xb3;
    /// A handshake, not really a command: wakes a receiver whose mouse has gone
    /// to standby. Eight bytes, four of them random. It shares its code with
    /// [`DEVICE_INFO`] because it *is* the same exchange — the difference is
    /// only whether the caller cares about the answer.
    pub const HANDSHAKE: u8 = 0x01;

    /// ReadCidMid — the receiver's own ids and type. Same code as
    /// [`HANDSHAKE`], see the note there.
    ///
    /// This is `EncryptionData` in the vendor enum, and it is the command the
    /// driver feeds straight to its `CS_GetCidMid` parser, which fills
    /// `DeviceInfo { CID, MID, DeviceType }`.
    ///
    /// It wants an 8-byte payload slot (`08 01 00 00 00 08 …`) even though the
    /// bytes are zero: with a zero length the receiver answers a one-byte stub
    /// instead of the id block. Measured on a P1 Ultra 4K receiver:
    ///
    ///   request   08 01 00 00 00 08 00 00 00 00 00 00 00 00 00 00 44
    ///   reply     08 01 00 00 00 08 1d 02 00 00 1d 02 01 00 00 00 05
    ///                               │           │        └─ device type 1
    ///                               │           └─ MID 29
    ///                               └─ CID 29
    ///
    /// Both ids sit on a four-byte stride, which is how the driver's struct
    /// lays out; 29 is exactly the CID the vendor's own Config.ini carries.
    ///
    /// Note that the vendor enum also lists a `ReadCIDMID = 16`, and 0x10
    /// answers nothing at all on this hardware — so the *name* in the enum and
    /// the code the firmware acts on disagree here. The reading is what the
    /// device does, not what the enum says.
    pub const DEVICE_INFO: u8 = 0x01;
    /// EnterDonglePair — open the pairing window.
    ///
    /// The payload is **two** bytes, `[cid][mid]`, not one: that is what the
    /// vendor's `CS_UsbServer_EnterDonglePairWithCidMid` builds
    /// (`08 05 00 00 00 02 cid mid …`). Sending a single byte is accepted but
    /// does not open the window — which is why pairing "failed" for so long.
    pub const ENTER_PAIR: u8 = 0x05;
    /// The receiver's pairing window report: `[state][seconds_left]`.
    ///
    /// 0x06 is not the vendor's status command, but it is the one this
    /// receiver answers with a live countdown in it, so it is the one worth
    /// polling. Idle answers `00 00`; inside the window it answers something
    /// like `03 16` (state 3, 22 s left).
    pub const PAIR_STATE: u8 = 0x06;

    /// ClearSetting — **factory reset** (0x09).
    ///
    /// This code was recorded here before as a pairing-status read, borrowed
    /// from an unofficial note. The vendor enum is unambiguous and the driver's
    /// own button handler settles it: `customButton_Reset_Click` pops a confirm
    /// dialog and then calls `UsbServer.SetClearSetting()`, which sends this
    /// command with no payload. Nothing in the driver reads it, and nothing in
    /// this app called it, which is why the mistake never wiped anybody's
    /// settings.
    ///
    /// Everything user-set lives in flash — DPI stages, colours, debounce,
    /// key mapping — and this zeroes it. It is therefore only ever sent from an
    /// explicit, confirmed user action, and never from a poll or a probe.
    pub const CLEAR_SETTING: u8 = 0x09;

    /// GetCurrentConfig — which onboard profile is live (0x0e).
    ///
    /// Answers one byte, the profile index. A device with only one profile
    /// answers nothing at all, which the driver uses to grey its profile picker
    /// out. Verified on a P1 Ultra 4K receiver: it answers `01 <index>`, and the
    /// index it reports is the one a [`CONFIG_SET`] write puts there.
    pub const CONFIG_GET: u8 = 0x0e;
    /// SetCurrentConfig — switch onboard profile (0x0f).
    ///
    /// The payload is **one** byte, the profile index. Ten bytes — the shape
    /// `Set4KDongleRGB` and `SetLongRangeMode` both want — are answered with the
    /// reject flag on this receiver, so those shapes are not interchangeable
    /// and guessing costs a refused write. Verified end to end: writing `01`
    /// moves [`CONFIG_GET`] to 1, and writing `00` moves it back.
    pub const CONFIG_SET: u8 = 0x0f;

    /// GetLongRangeMode (0x17).
    ///
    /// Answers ten bytes; `[0] > 0` means long range is on. This code was
    /// recorded here before as a write-commit code; it is nothing of the kind.
    pub const LONG_RANGE_GET: u8 = 0x17;
    /// SetLongRangeMode — trade latency for range (0x16).
    ///
    /// Needs **ten** payload bytes, `[enable] 0 0 0 0 0 0 0 0 0`. One or two
    /// bytes answer with the reject flag; ten answer `0x00`, and a following
    /// [`LONG_RANGE_GET`] reports exactly the value written. Verified both
    /// directions on a P1 Ultra 4K receiver.
    ///
    /// Only ever offered on the wireless link: the driver guards it with
    /// `if (!ConnectedDeviceInfo.isUSB)`.
    pub const LONG_RANGE_SET: u8 = 0x16;

    /// Get4KDongleRGBValue — read the 4K receiver's light (0x15).
    ///
    /// Answers with ten payload bytes,
    /// `[mode][c1 R G B][c2 R G B][c3 R G B]`, which is byte for byte the
    /// vendor's `DongleRGB` struct. It takes the same 17-byte frame as every
    /// other command here, so no new transport is needed.
    pub const DONGLE_RGB_GET: u8 = 0x15;
    /// Set4KDongleRGB — write it (0x14), the same ten bytes, no commit frame.
    ///
    /// Reverse engineered from `CS_UsbServer_Set4KDongleRGB` and confirmed on a
    /// P1 Ultra 4K receiver: writing `02 ff 00 00 00 ff 00 00 00 ff` and
    /// reading back returns exactly those bytes.
    pub const DONGLE_RGB_SET: u8 = 0x14;

    // The two update-mode codes (`EnterUsbUpdateMode` 0x0d, `EnterMTKMode`
    // 0x11) are named in the enum at the top of this file but deliberately have
    // no constant here: nothing should be able to acquire a caller by accident.
    // Both reboot the device out of its normal mode and leave it unable to act
    // as a mouse until an image is written back, so even an unused typed name is
    // one autocomplete away from a bricked-looking mouse.
    //
    // 0x02 (`PCDriverStatus`) answered with the reject flag every time it was
    // probed, in every address and length tried. It has no constant either.
}

// ── receiver LED ─────────────────────────────────────────────────────────

/// The 4K receiver's light: one mode plus three colours.
///
/// This mirrors the vendor's `DongleRGB` struct. The three colours are not
/// three lamps — they are the three levels the chosen mode switches between,
/// so in battery mode they read as "full / half / low".
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DongleRgb {
    pub mode: u8,
    /// `[r, g, b]`.
    pub color1: [u8; 3],
    pub color2: [u8; 3],
    pub color3: [u8; 3],
}

/// The three modes the official UI offers, as their `Tag` values.
pub const DONGLE_RGB_MODES: [(u8, &str); 3] = [
    (1, "连接状态"),
    (2, "电量状态"),
    (3, "仅电量"),
];

impl DongleRgb {
    /// Unpack the ten payload bytes the receiver answers with.
    pub fn from_payload(p: &[u8]) -> Option<Self> {
        if p.len() < 10 {
            return None;
        }
        Some(Self {
            mode: p[0],
            color1: [p[1], p[2], p[3]],
            color2: [p[4], p[5], p[6]],
            color3: [p[7], p[8], p[9]],
        })
    }

    /// The ten bytes that go into the write frame.
    pub fn to_payload(self) -> [u8; 10] {
        let mut out = [0u8; 10];
        out[0] = self.mode;
        out[1..4].copy_from_slice(&self.color1);
        out[4..7].copy_from_slice(&self.color2);
        out[7..10].copy_from_slice(&self.color3);
        out
    }

    /// What the mode number means, or "未知" for one we have not seen.
    pub fn mode_label(mode: u8) -> &'static str {
        DONGLE_RGB_MODES
            .iter()
            .find(|(v, _)| *v == mode)
            .map(|(_, n)| *n)
            .unwrap_or("未知模式")
    }

    /// `#rrggbb`, for feeding straight to a colour input.
    pub fn hex(color: [u8; 3]) -> String {
        format!("{:02x}{:02x}{:02x}", color[0], color[1], color[2])
    }
}

/// How many bytes a ReadCidMid frame declares, and how many it answers with.
pub const CID_MID_LEN: usize = 8;
/// Where the device type sits inside a ReadCidMid reply.
///
/// The vendor's `DeviceInfo { CID, MID, DeviceType }` is filled from the eight
/// answer bytes `[cid] 02 00 00 [mid] 02 ?? ??`, and on the one receiver we can
/// measure this is the byte that carries the type: it reads 1 on the 4K
/// receiver, which is exactly the value the driver tests for before it offers
/// the receiver-light controls. Read from one device only, so the UI shows it
/// as information and nothing branches on it.
pub const DEVICE_TYPE_INDEX: usize = 6;

/// How many onboard profiles the driver's picker offers once a device answers
/// [`cmd::CONFIG_GET`] at all.
///
/// The driver itself keeps two answers: no reply means one profile and the
/// picker is greyed out, a reply means this many.
pub const ONBOARD_PROFILES: usize = 4;

/// Payload length of the ten-byte vendor writes (receiver light, long range).
pub const TEN_BYTE_PAYLOAD: usize = 10;

/// True when this reply carries the reject flag.
///
/// Byte 2 is the device saying no. Checked rather than assumed because a
/// refused write looks exactly like a successful one otherwise — the frame is
/// echoed back either way, with one bit changed.
pub fn rejected(reply: &[u8]) -> bool {
    reply.get(2).copied().unwrap_or(0) != 0
}

/// End of the configuration block we care about (192 bytes).
pub const CONFIG_END: usize = 0x00c0;
/// Active DPI stage, 1-based, stored as a `[value][0x55-value]` pair.
pub const ACTIVE_STAGE_ADDR: u16 = 0x0004;
/// First DPI stage record; 8 slots, 4 bytes apart.
pub const DPI_STAGE_BASE: usize = 0x000c;
pub const DPI_STAGE_SLOTS: usize = 8;
/// RGB colours, 4-byte records.
pub const COLOR_BASE: usize = 0x002c;
pub const COLOR_COUNT: usize = 8;
/// `DPIMaxGrade`: how many of the 8 stage slots are live.
pub const STAGE_COUNT_ADDR: usize = 0x0080;
/// Key debounce, in milliseconds, as a four-byte record `[ms][04][00][chk]`.
///
/// There are **two** debounce bytes in this block, and they are independent:
/// writing one and reading the other back, in both orders, moves nothing
/// (measured on this unit — both sat at 8 before and after each write). 0x0084
/// is the record the vendor driver edits; 0x00A9 is byte 0 of the ten-byte
/// "advanced" row that OpenMouse's ATK driver edits, and that project's
/// firmware family is close enough to this one that its layout otherwise
/// matches field for field.
///
/// Since the block ships with the same value in both, the only reading that
/// cannot be wrong is that the vendor keeps the copies in step. So both are
/// written here: an extra byte in the same range, in exchange for not leaving
/// the setting dead on whichever copy this firmware actually reads.
pub const DEBOUNCE_ADDR: usize = 0x0084;
/// The second copy, inside the advanced row (see [`DEBOUNCE_ADDR`]).
pub const ADVANCED_DEBOUNCE_ADDR: usize = 0x00a9;
/// LOD / silence height, stored as a `[value][0x55-value]` pair.
pub const LOD_ADDR: usize = 0x000a;
// 0x0060 was left unrecorded here once — it was recognisable as a key table
// but nobody had the binding codes. The CompX family this receiver belongs to
// shares one flash layout, and another driver for the same layout publishes
// that table, so all six records below now decode. Every one of them checked
// out on this unit: Left/Right/Middle/Back/Forward/DPI-loop, each with a
// matching checksum.
//
// Three more settings arrived the same way and were verified the same way
// (pair checksums are what make the offsets believable — see `read_pair`).

/// First key-assignment record. Six physical buttons, four bytes each.
pub const KEY_FUNCTION_BASE: usize = 0x0060;
pub const KEY_RECORD_LEN: usize = 4;

/// Angle correction / straight-line correction. `0` off, `1` on.
pub const ANGLE_SNAPPING_ADDR: usize = 0x00af;

/// The LED that lights the DPI stage button.
pub const DPI_LIGHT_MODE_ADDR: usize = 0x004c;
pub const DPI_LIGHT_BRIGHTNESS_ADDR: usize = 0x004e;
pub const DPI_LIGHT_SPEED_ADDR: usize = 0x0050;
pub const DPI_LIGHT_STATE_ADDR: usize = 0x0052;

// Deliberately not decoded: 0x0054–0x005F (ambient colour, effect mode,
// brightness, speed, energy saving), 0x00AD (LED-off delay) and 0x00B3 ("lights
// out while moving") all hold self-consistent, writable bytes and all appear in
// the shared family layout — but this shell has no ambient LED. The only light
// it has is the wheel, which is the DPI indicator at 0x004C. These were
// implemented once and taken back out; a byte the firmware stores is not proof
// that anything is wired to it.

/// MouseConfig tail pairs (`[value][0x55-value]`).
///
/// The sleep timer sits at **0x00ad**, byte 4 of a ten-byte row at 0x00a9
/// that OpenMouse's ATK driver calls "advanced": debounce at 0xa9, motion sync
/// at 0xab, sleep at 0xad, angle snap at 0xaf, ripple at 0xb1. Measured on
/// this mouse rather than read off a table — see the note on
/// [`SLEEP_TIME_ADDR`].
pub const SLEEP_TIME_ADDR: usize = 0x00ad;
/// Not a sleep setting: 0x00B5 starts a six-byte sensor-performance row
/// `[0x00][0x55][marker lo][marker hi][mode][0x55-mode]`. Bytes 0xB7/0xB8 are
/// that row's marker (a sensor id, `06 4f` on this unit), which happens to sum
/// to 0x55 and so looks exactly like a value/checksum pair.
pub const SENSOR_MODE_ADDR: usize = 0x00b9;
pub const MOTION_SYNC_ADDR: usize = 0x00ab;
pub const RIPPLE_ADDR: usize = 0x00b1;

/// Frame that makes the device flush the staged records to flash.
///
/// A third-party tool for this family commits with a frame it labels "apply /
/// save (opcode 0x04)", which reads like a contradiction: the vendor enum calls
/// 0x04 `BatteryLevel`, and that tool's own byte sequence for it
/// (`08 04 00 00 00 00 …`) is character for character the battery request this
/// app already sends. So the "conflict" is a naming one — either 0x04 answers a
/// battery read *and* flushes, or the label is wrong. `--persist battery`
/// settles which, by staging a byte and asking whether it survives a sleep
/// cycle after each kind of flush.
pub const WRITE_COMMIT_ADDR: u16 = 0x061e;

// ── checksums ────────────────────────────────────────────────────────────

pub fn checksum(frame: &[u8]) -> u8 {
    let mut sum: u32 = 0;
    for i in 0..16 {
        sum += *frame.get(i).unwrap_or(&0) as u32;
    }
    (CHECKSUM_BASE as u32).wrapping_sub(sum & 0xff) as u8
}

/// Build one 17-byte command frame with a valid checksum.
pub fn build_frame(cmd: u8, addr: u16, payload: &[u8]) -> [u8; FRAME_SIZE] {
    let mut out = [0u8; FRAME_SIZE];
    let len = payload.len().min(MAX_PAYLOAD);
    out[0] = REPORT_ID;
    out[1] = cmd;
    out[3] = (addr >> 8) as u8;
    out[4] = addr as u8;
    out[5] = len as u8;
    out[6..6 + len].copy_from_slice(&payload[..len]);
    out[16] = checksum(&out);
    out
}

pub fn record_checksum(v0: u8, v1: u8, v2: u8) -> u8 {
    (CHECKSUM_BASE as u32).wrapping_sub((v0 as u32 + v1 as u32 + v2 as u32) & 0xff) as u8
}

pub fn pair_checksum(value: u8) -> u8 {
    (CHECKSUM_BASE as u32).wrapping_sub(value as u32 & 0xff) as u8
}

/// `(v0, v1, v2, checksum_valid)` for the record at `addr`.
pub fn read_record(blob: &[u8], addr: usize) -> (u8, u8, u8, bool) {
    let g = |i: usize| blob.get(i).copied().unwrap_or(0);
    let (v0, v1, v2, chk) = (g(addr), g(addr + 1), g(addr + 2), g(addr + 3));
    (v0, v1, v2, record_checksum(v0, v1, v2) == chk)
}

pub fn write_record(blob: &mut [u8], addr: usize, v0: u8, v1: u8, v2: u8) {
    if addr + 3 >= blob.len() {
        return;
    }
    blob[addr] = v0;
    blob[addr + 1] = v1;
    blob[addr + 2] = v2;
    blob[addr + 3] = record_checksum(v0, v1, v2);
}

/// `(value, checksum_valid)` for the `[value][0x55-value]` pair at `addr`.
pub fn read_pair(blob: &[u8], addr: usize) -> (u8, bool) {
    let value = blob.get(addr).copied().unwrap_or(0);
    (value, blob.get(addr + 1).copied() == Some(pair_checksum(value)))
}

pub fn write_pair(blob: &mut [u8], addr: usize, value: u8) {
    if addr + 1 >= blob.len() {
        return;
    }
    blob[addr] = value;
    blob[addr + 1] = pair_checksum(value);
}

// ── button assignment ────────────────────────────────────────────────────
//
// A button record is `[class][param_hi][param_lo][checksum]` — the same
// three-bytes-plus-checksum shape every other record in this block uses. What
// the class means comes from the shared CompX map; what each button can be
// bound to is the subset that is safe to offer, because binding something
// wrong here costs you a working left-click.

pub const KEY_CLASS_DISABLE: u8 = 0;
pub const KEY_CLASS_MOUSE: u8 = 1;
pub const KEY_CLASS_DPI: u8 = 2;
pub const KEY_CLASS_TILT: u8 = 3;
/// 连发键（XClick）：param_hi 是每次点击之间的毫秒数，param_lo 是次数。
pub const KEY_CLASS_FIRE: u8 = 4;
/// 组合键 / 多媒体键：真正的键序在 [`SHORTCUT_BASE`] 的 32 字节槽里。
pub const KEY_CLASS_COMBO: u8 = 5;
/// 宏：param_hi 是宏槽，param_lo 是循环次数。
pub const KEY_CLASS_MACRO: u8 = 6;
pub const KEY_CLASS_RATE: u8 = 7;
pub const KEY_CLASS_LIGHT: u8 = 8;
pub const KEY_CLASS_PROFILE: u8 = 9;
pub const KEY_CLASS_DPI_LOCK: u8 = 0x0a;
pub const KEY_CLASS_WHEEL: u8 = 0x0b;

/// What one button can be bound to.
pub struct ButtonAction {
    pub label: &'static str,
    pub cls: u8,
    pub param: u16,
}

pub const BUTTON_ACTIONS: &[ButtonAction] = &[
    ButtonAction { label: "左键", cls: KEY_CLASS_MOUSE, param: 0x0100 },
    ButtonAction { label: "右键", cls: KEY_CLASS_MOUSE, param: 0x0200 },
    ButtonAction { label: "中键", cls: KEY_CLASS_MOUSE, param: 0x0400 },
    ButtonAction { label: "后退", cls: KEY_CLASS_MOUSE, param: 0x0800 },
    ButtonAction { label: "前进", cls: KEY_CLASS_MOUSE, param: 0x1000 },
    ButtonAction { label: "DPI 循环", cls: KEY_CLASS_DPI, param: 0x0100 },
    ButtonAction { label: "DPI +", cls: KEY_CLASS_DPI, param: 0x0200 },
    ButtonAction { label: "DPI -", cls: KEY_CLASS_DPI, param: 0x0300 },
    ButtonAction { label: "横向滚轮左", cls: KEY_CLASS_TILT, param: 0x0100 },
    ButtonAction { label: "横向滚轮右", cls: KEY_CLASS_TILT, param: 0x0200 },
    ButtonAction { label: "纵向滚轮上", cls: KEY_CLASS_WHEEL, param: 0x0100 },
    ButtonAction { label: "纵向滚轮下", cls: KEY_CLASS_WHEEL, param: 0x0200 },
    ButtonAction { label: "禁用", cls: KEY_CLASS_DISABLE, param: 0x0000 },
];

/// Bindings the firmware almost certainly understands but this mouse has never
/// been seen performing.
///
/// They come from the same shared class table as everything above, yet nobody
/// has watched a P1 act on them, and for three of them even the meaning of the
/// second byte is a guess. Offered behind a clearly-marked group in the UI
/// rather than mixed into the plain list: worst case the button does nothing
/// and the user sets it back, but the list should not imply certainty it does
/// not have.
pub const BUTTON_ACTIONS_UNVERIFIED: &[ButtonAction] = &[
    ButtonAction { label: "回报率切换", cls: KEY_CLASS_RATE, param: 0x0000 },
    ButtonAction { label: "灯光开关", cls: KEY_CLASS_LIGHT, param: 0x0000 },
    ButtonAction { label: "DPI 锁定", cls: KEY_CLASS_DPI_LOCK, param: 0x0000 },
    ButtonAction { label: "连发 20 ms × 3", cls: KEY_CLASS_FIRE, param: 0x1403 },
    ButtonAction { label: "连发 50 ms × 3", cls: KEY_CLASS_FIRE, param: 0x3203 },
];

/// Physical buttons, in the order their records appear at 0x0060.
///
/// Six. Records 6 and 7 at 0x0078 and 0x007C also hold `DPI +` and `DPI -`
/// with valid checksums, and offering them looked reasonable — but nothing on
/// this mouse is wired to those slots, so they were removed again. The DPI
/// actions stay in `BUTTON_ACTIONS` because the wheel button can still take
/// them.
pub const BUTTON_NAMES: &[&str] = &["左键", "右键", "中键", "后退", "前进", "DPI"];
pub const BUTTON_COUNT: usize = BUTTON_NAMES.len();

/// One button as the UI sees it.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ButtonMapping {
    pub index: usize,
    pub name: &'static str,
    /// Resolved label, or `"自定义"` for a class/param pair this build cannot
    /// name — which is the honest answer for a vendor macro or a key combo.
    pub action: String,
    pub cls: u8,
    pub param: u16,
    /// False when the stored checksum does not match; shown greyed out.
    pub valid: bool,
}

pub fn key_record_addr(index: usize) -> usize {
    KEY_FUNCTION_BASE + index * KEY_RECORD_LEN
}

/// What `param` means for each class, in one table.
///
/// Class 0..3 were already known; 4..0x0B come from the shared CompX map that
/// `x33-control` recovered from the vendor's own web driver, and they are what
/// used to be shown as 「自定义」. Two of them point somewhere else in flash:
///
/// * class 5 (组合键/多媒体) stores the actual keys in a 32-byte slot at
///   [`SHORTCUT_BASE`] + 0x20 × `param_hi`; `param_lo` is the slot index.
/// * class 6 (宏) stores the macro in a 384-byte region at 0x0300; `param_hi`
///   is the slot, `param_lo` is how often it repeats.
///
/// Verified end to end on this mouse: the slots answer reads (all `0xff`,
/// erased), a 32-byte slot round-trips a write exactly, and binding the DPI
/// button to Ctrl+C then pressing it produced a real Ctrl+C in Windows.
pub fn class_param_label(cls: u8, param: u16) -> Option<String> {
    let hi = (param >> 8) as u8;
    let lo = (param & 0xff) as u8;
    let text = match cls {
        // param_hi is a bitmask: 01 左 02 右 04 中 08 后退 10 前进.
        KEY_CLASS_MOUSE => match hi {
            0x01 => "左键",
            0x02 => "右键",
            0x04 => "中键",
            0x08 => "后退",
            0x10 => "前进",
            _ => return None,
        },
        KEY_CLASS_DPI => match hi {
            1 => "DPI 循环",
            2 => "DPI +",
            3 => "DPI -",
            _ => return None,
        },
        KEY_CLASS_TILT => match hi {
            1 => "横向滚轮左",
            2 => "横向滚轮右",
            _ => return None,
        },
        // 连发键：间隔毫秒 + 次数。
        KEY_CLASS_FIRE => {
            return Some(format!("连发 {} ms × {}", hi, match lo {
                0 => "按住期间".to_string(),
                n => n.to_string(),
            }))
        }
        // 组合键/多媒体：真正的内容在 0x0100 槽里，由调用方补齐。
        KEY_CLASS_COMBO => "组合键/多媒体",
        // 宏：槽号 + 循环次数（0xFE 按住循环，0xFF 按任意键停止）。
        KEY_CLASS_MACRO => {
            return Some(format!("宏 {}{}", hi, match lo {
                0xfe => "（按住期间循环）".to_string(),
                0xff => "（循环到再按一次）".to_string(),
                0 => String::new(),
                n => format!("× {n}"),
            }))
        }
        KEY_CLASS_RATE => return Some("回报率切换".to_string()),
        KEY_CLASS_LIGHT => return Some("灯光开关".to_string()),
        KEY_CLASS_PROFILE => return Some(format!("配置切换 {}", hi)),
        KEY_CLASS_DPI_LOCK => return Some("DPI 锁定".to_string()),
        KEY_CLASS_WHEEL => match hi {
            1 => return Some("纵向滚轮上".to_string()),
            2 => return Some("纵向滚轮下".to_string()),
            _ => return None,
        },
        _ => return None,
    };
    Some(text.to_string())
}

/// First 32-byte shortcut slot, holding the keys behind a class-5 binding.
///
/// Verified readable and writable on this receiver; every slot ships erased
/// (`0xff`), which is why no button on a factory mouse decodes to a combo.
/// Shortcut slots are per button: button `i` reads its keys from slot `i`.
///
/// There is no slot number anywhere in the button record — OpenMouse leaves
/// both parameter bytes at zero for class 5 — which is exactly what the
/// second independent source found on its own hardware ("the hardware has one
/// region per button and nothing numbers them"). Six buttons, sixteen slots,
/// so there is room to spare and no allocation to get wrong. Confirmed the
/// same way: the binding written to the DPI button works when pressed.
pub const SHORTCUT_BASE: usize = 0x0100;
pub const SHORTCUT_STRIDE: usize = 0x20;
pub const SHORTCUT_SLOTS: usize = 16;

/// Modifier bit inside a shortcut event's `kind` byte (kind 0).
pub const MOD_CTRL: u8 = 0x01;
pub const MOD_SHIFT: u8 = 0x02;
pub const MOD_ALT: u8 = 0x04;
pub const MOD_GUI: u8 = 0x08;
/// Everything a modifier bitmask is allowed to hold.
pub const MODIFIER_MASK: u8 = MOD_CTRL | MOD_SHIFT | MOD_ALT | MOD_GUI;

/// An event's first byte is `kind | flag`: press or release on top of what
/// kind of code follows.
pub const EV_PRESS: u8 = 0x80;
pub const EV_RELEASE: u8 = 0x40;
pub const EV_KIND_MOD: u8 = 0x00;
pub const EV_KIND_KEY: u8 = 0x01;
pub const EV_KIND_CONSUMER: u8 = 0x02;

/// Keys the UI offers for a combination, as HID keyboard usages.
///
/// Deliberately short: a mouse shortcut wants letters, digits, arrows and a
/// handful of editing keys, not the full 104-key table.
pub const OFFERED_KEYS: &[(u16, &str)] = &[
    (0x04, "A"), (0x05, "B"), (0x06, "C"), (0x07, "D"), (0x08, "E"), (0x09, "F"),
    (0x0a, "G"), (0x0b, "H"), (0x0c, "I"), (0x0d, "J"), (0x0e, "K"), (0x0f, "L"),
    (0x10, "M"), (0x11, "N"), (0x12, "O"), (0x13, "P"), (0x14, "Q"), (0x15, "R"),
    (0x16, "S"), (0x17, "T"), (0x18, "U"), (0x19, "V"), (0x1a, "W"), (0x1b, "X"),
    (0x1c, "Y"), (0x1d, "Z"),
    (0x1e, "1"), (0x1f, "2"), (0x20, "3"), (0x21, "4"), (0x22, "5"), (0x23, "6"),
    (0x24, "7"), (0x25, "8"), (0x26, "9"), (0x27, "0"),
    (0x28, "回车"), (0x29, "Esc"), (0x2a, "退格"), (0x2b, "Tab"), (0x2c, "空格"),
    (0x2d, "-"), (0x2e, "="), (0x2f, "["), (0x30, "]"), (0x31, "\\"), (0x33, ";"),
    (0x34, "'"), (0x36, ","), (0x37, "."), (0x38, "/"),
    (0x3a, "F1"), (0x3b, "F2"), (0x3c, "F3"), (0x3d, "F4"), (0x3e, "F5"), (0x3f, "F6"),
    (0x40, "F7"), (0x41, "F8"), (0x42, "F9"), (0x43, "F10"), (0x44, "F11"), (0x45, "F12"),
    (0x4f, "→"), (0x50, "←"), (0x51, "↓"), (0x52, "↑"),
    (0x4b, "PageUp"), (0x4e, "PageDown"), (0x4a, "Home"), (0x4d, "End"),
];

/// Multimedia and launcher actions the UI offers, as HID Consumer usages.
pub const OFFERED_CONSUMER: &[(u16, &str)] = &[
    (0x00cd, "播放/暂停"),
    (0x00b5, "下一曲"),
    (0x00b6, "上一曲"),
    (0x00b7, "停止播放"),
    (0x00e2, "静音"),
    (0x00e9, "音量+"),
    (0x00ea, "音量-"),
    (0x0183, "媒体播放器"),
    (0x018a, "邮件"),
    (0x0192, "计算器"),
    (0x0221, "搜索"),
    (0x0223, "浏览器主页"),
    (0x0225, "前进"),
    (0x0224, "后退"),
    (0x0227, "刷新"),
];

/// Build the event list behind one combination, from modifier bits and either
/// a keyboard or a consumer usage.
///
/// Modifiers travel as events of their own, which is why a combination is four
/// events and not two. Both reference sources independently captured the
/// vendor's own output for Ctrl+C and got the same eight bytes.
pub fn shortcut_events(modifiers: u8, usage: u16, consumer: bool) -> Vec<(u8, u16)> {
    let kind = if consumer { EV_KIND_CONSUMER } else { EV_KIND_KEY };
    if modifiers == 0 {
        return vec![(kind | EV_PRESS, usage), (kind | EV_RELEASE, usage)];
    }
    vec![
        (EV_KIND_MOD | EV_PRESS, modifiers as u16),
        (kind | EV_PRESS, usage),
        (EV_KIND_MOD | EV_RELEASE, modifiers as u16),
        (kind | EV_RELEASE, usage),
    ]
}

/// Pack an event list into a 32-byte slot.
///
/// `[count][count × 3-byte event][checksum]`, everything after the checksum
/// left at the erased `0xff`. The checksum covers the count byte and the
/// events but nothing after them, which is what makes the tail length
/// irrelevant — for `Ctrl+C` it lands on the same `0xc1` the vendor writes.
/// Returns `None` if the events do not fit.
pub fn encode_shortcut_slot(events: &[(u8, u16)]) -> Option<[u8; SHORTCUT_STRIDE]> {
    if events.is_empty() || 1 + events.len() * 3 + 1 > SHORTCUT_STRIDE {
        return None;
    }
    let mut slot = [0xffu8; SHORTCUT_STRIDE];
    slot[0] = events.len() as u8;
    for (i, (kind, code)) in events.iter().enumerate() {
        let at = 1 + i * 3;
        slot[at] = *kind;
        slot[at + 1] = (code & 0xff) as u8;
        slot[at + 2] = (code >> 8) as u8;
    }
    let end = 1 + events.len() * 3;
    let sum: u32 = slot[..end].iter().map(|b| *b as u32).sum();
    slot[end] = (CHECKSUM_BASE as u32).wrapping_sub(sum & 0xff) as u8;
    Some(slot)
}

/// How a combination should read back, for verifying a write.
pub fn shortcut_target_label(modifiers: u8, usage: u16, consumer: bool) -> Option<String> {
    decode_shortcut_slot(&encode_shortcut_slot(&shortcut_events(modifiers, usage, consumer))?)
}

/// HID keyboard usage -> what the UI calls it. Only the keys a mouse user is
/// likely to reach for; anything else decodes to `usage 0xNN`.
pub fn key_usage_name(usage: u16) -> String {
    let name = match usage {
        0x04..=0x1d => {
            return char::from_u32(b'a' as u32 + (usage - 0x04) as u32)
                .unwrap_or('?')
                .to_ascii_uppercase()
                .to_string()
        }
        0x1e..=0x26 => return format!("{}", usage - 0x1e + 1),
        0x27 => "0".to_string(),
        0x28 => "回车".to_string(),
        0x29 => "Esc".to_string(),
        0x2a => "退格".to_string(),
        0x2b => "Tab".to_string(),
        0x2c => "空格".to_string(),
        0x2d..=0x38 => ["-", "=", "[", "]", "\\", "#", ";", "'", "`", ",", ".", "/"]
            [(usage - 0x2d) as usize].to_string(),
        0x39 => "Caps".to_string(),
        0x46 => "PrintScreen".to_string(),
        0x47 => "ScrollLock".to_string(),
        0x48 => "Pause".to_string(),
        0x49 => "Insert".to_string(),
        0x4a => "Home".to_string(),
        0x4b => "PageUp".to_string(),
        0x4c => "Delete".to_string(),
        0x4d => "End".to_string(),
        0x4e => "PageDown".to_string(),
        0x3a..=0x45 => format!("F{}", usage - 0x3a + 1),
        0x4f => "→".to_string(),
        0x50 => "←".to_string(),
        0x51 => "↓".to_string(),
        0x52 => "↑".to_string(),
        _ => format!("usage 0x{usage:02x}"),
    };
    name
}

/// HID Consumer (multimedia) usage -> what the UI calls it.
pub fn consumer_usage_name(usage: u16) -> Option<&'static str> {
    Some(match usage {
        0x006f => "亮度+",
        0x0070 => "亮度-",
        0x00b5 => "下一曲",
        0x00b6 => "上一曲",
        0x00b7 => "停止播放",
        0x00cd => "播放/暂停",
        0x00e2 => "静音",
        0x00e9 => "音量+",
        0x00ea => "音量-",
        0x0183 => "媒体播放器",
        0x018a => "邮件",
        0x0192 => "计算器",
        0x0194 => "文件浏览器",
        0x0221 => "搜索",
        0x0223 => "浏览器主页",
        0x0224 => "后退",
        0x0225 => "前进",
        0x0226 => "停止页面",
        0x0227 => "刷新",
        0x022a => "书签",
        _ => return None,
    })
}

/// Decode one 32-byte shortcut slot into something a human can read.
///
/// `[count][count × 3-byte event][checksum]`. Events are chronological, not
/// grouped press-then-release: a combination reads `修饰键按下, 按键按下,
/// 修饰键松开, 按键松开`. Each event is `[kind, code_lo, code_hi]` where kind
/// carries both the kind and a press/release flag — `0x80|k` press, `0x40|k`
/// release — and `k` selects what the code means: 0 = modifier bitmask,
/// 1 = HID keyboard usage, 2 = HID Consumer usage.
///
/// Only press events contribute to the label; releases repeat a key already
/// named. The checksum is not verified — we know the rule the vendor uses but
/// not whether the firmware enforces it, and refusing to display a binding
/// the mouse is happy with would be the worse failure.
pub fn decode_shortcut_slot(slot: &[u8]) -> Option<String> {
    let count = *slot.first()? as usize;
    if count == 0 || count == 0xff {
        return None;
    }
    // Room for the events and for the byte that settles their sum.
    if 1 + count * 3 + 1 > slot.len() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    for i in 0..count {
        let at = 1 + i * 3;
        let kind = slot[at];
        let code = ((slot[at + 2] as u16) << 8) | slot[at + 1] as u16;
        if kind & EV_PRESS == 0 {
            continue;
        }
        parts.push(match kind & 0x3f {
            EV_KIND_MOD => {
                let mods_on = (code & 0xff) as u8;
                let mut mods: Vec<&str> = Vec::new();
                if mods_on & MOD_CTRL != 0 { mods.push("Ctrl"); }
                if mods_on & MOD_SHIFT != 0 { mods.push("Shift"); }
                if mods_on & MOD_ALT != 0 { mods.push("Alt"); }
                if mods_on & MOD_GUI != 0 { mods.push("Win"); }
                if mods.is_empty() {
                    format!("修饰键 0x{code:02x}")
                } else {
                    mods.join("+")
                }
            }
            EV_KIND_KEY => key_usage_name(code),
            EV_KIND_CONSUMER => {
                let text = consumer_usage_name(code).unwrap_or("多媒体").to_string();
                if text == "多媒体" {
                    format!("多媒体 0x{code:04x}")
                } else {
                    text
                }
            }
            other => format!("kind {other}"),
        });
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("+"))
}

pub fn button_action_label(cls: u8, param: u16) -> Option<&'static str> {
    BUTTON_ACTIONS
        .iter()
        .find(|a| a.cls == cls && a.param == param)
        .map(|a| a.label)
}

// ── macros ──────────────────────────────────────────────────────────────
//
// A macro lives in its own 384-byte region at 0x0300 + 0x180 × slot, and the
// button record points at it with class 6: `[06][slot][repeat][chk]`. The
// layout inside the region:
//
//   0x00        name length in BYTES (UTF-16LE, so 2 per character)
//   0x01..0x1e  the name, zero padded — cosmetic, and outside the checksum
//   0x1f        event count
//   0x20 + 5i   one event: [0x80 press | 0x40 release | kind][code][0][delay hi][delay lo]
//   …           checksum = 0x55 - (count + every event byte)
//
// Two things about this are worth stating plainly, because the honest version
// is less flattering than "reversed from the vendor software":
//
// * The layout comes from a tool written for the Areson revision of this
//   family, whose author says outright that CompX "uses different addressing
//   throughout, and none of it has been captured". It matches, field for field,
//   the constants a CompX-side project publishes for ATK hardware (base 0x0300,
//   slot 0x180, twelve slots), and this receiver answers reads across that whole
//   range with the erased 0xff, so the region exists here.
// * It was an experiment until it ran: a two-step F24 macro bound to the
//   forward button, then pressed — the PC received F24 twice, 250 ms apart,
//   with a real scan code and no injected flag. So this firmware does execute
//   the region, at this base, in this layout, and it honours the delay byte.

pub const MACRO_BASE: usize = 0x0300;
pub const MACRO_STRIDE: usize = 0x180;
pub const MACRO_SLOTS: usize = 12;
pub const MACRO_COUNT_OFFSET: usize = 0x1f;
pub const MACRO_EVENTS_OFFSET: usize = 0x20;
pub const MACRO_EVENT_SIZE: usize = 5;
/// 3 ms is the floor the firmware honours; a stored 3 reads back as "no delay".
pub const MACRO_MIN_DELAY_MS: u16 = 3;
/// How many five-byte events fit between the count byte and the checksum.
pub const MACRO_MAX_EVENTS: usize =
    (MACRO_STRIDE - MACRO_EVENTS_OFFSET - 1) / MACRO_EVENT_SIZE;

/// What one event of a macro acts on. The numbers are the same ones the button
/// table uses for that kind of thing, so there is nothing to translate.
pub const MACRO_KIND_MOD: u8 = 0;
pub const MACRO_KIND_KEY: u8 = 1;
pub const MACRO_KIND_MOUSE: u8 = 4;

/// One step of a macro, as the UI thinks of it: press these modifiers and this
/// key, hold them for `delay_ms`, release.
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MacroStep {
    #[serde(default)]
    pub modifiers: u8,
    pub usage: u16,
    #[serde(default)]
    pub delay_ms: u16,
    /// True for a keyboard key, false for a mouse button (`usage` is then the
    /// button bitmask at 0x01 left … 0x10 forward).
    #[serde(default = "default_true")]
    pub keyboard: bool,
}

fn default_true() -> bool {
    true
}

/// The events one step expands to.
///
/// Modifiers are events of their own, exactly as in a shortcut: press them,
/// press the key, release the key, release them. The step's delay rides on the
/// key release — the firmware treats the delay on an event as "wait this long
/// after it", so putting it there makes it the pause between steps rather than
/// a pause in the middle of one.
fn macro_step_events(step: &MacroStep) -> Vec<(u8, u8, u16)> {
    let kind = if step.keyboard { MACRO_KIND_KEY } else { MACRO_KIND_MOUSE };
    let code = (step.usage & 0xff) as u8;
    let delay = step.delay_ms.max(MACRO_MIN_DELAY_MS);
    let mut out: Vec<(u8, u8, u16)> = Vec::new();
    if step.modifiers != 0 {
        out.push((EV_KIND_MOD | EV_PRESS, step.modifiers, MACRO_MIN_DELAY_MS));
    }
    out.push((kind | EV_PRESS, code, MACRO_MIN_DELAY_MS));
    out.push((kind | EV_RELEASE, code, delay));
    if step.modifiers != 0 {
        out.push((EV_KIND_MOD | EV_RELEASE, step.modifiers, MACRO_MIN_DELAY_MS));
    }
    out
}

/// How many events a set of steps turns into, without building them.
pub fn macro_event_count(steps: &[MacroStep]) -> usize {
    steps.iter().map(|s| macro_step_events(s).len()).sum()
}

/// Lay one macro region out byte for byte.
///
/// Padding is zero, not erased `0xff`: that is what the vendor encoder leaves,
/// and a decode that treats 0xff as "past the end" would be relying on the
/// padding value rather than on the count.
pub fn encode_macro(name: &str, steps: &[MacroStep]) -> Result<[u8; MACRO_STRIDE], String> {
    if steps.is_empty() {
        return Err("宏里至少要有一个步骤".to_string());
    }
    let events: Vec<(u8, u8, u16)> = steps.iter().flat_map(macro_step_events).collect();
    if events.len() > MACRO_MAX_EVENTS {
        return Err(format!(
            "宏太长：{} 个事件，固件最多存 {MACRO_MAX_EVENTS} 个（约 {} 步）",
            events.len(),
            MACRO_MAX_EVENTS / 4
        ));
    }

    let mut region = [0u8; MACRO_STRIDE];
    // Name: length first, in bytes, then UTF-16LE. Truncated to whole
    // characters so a cut cannot leave half a code unit behind.
    let limit = (MACRO_COUNT_OFFSET - 1) / 2;
    // UTF-16 units, not characters: a name outside the BMP needs two, and the
    // length byte counts bytes either way.
    let mut chars: Vec<u16> = Vec::new();
    for c in name.chars() {
        let mut buf = [0u16; 2];
        let units = c.encode_utf16(&mut buf);
        if chars.len() + units.len() > limit.min(15) {
            break;
        }
        chars.extend_from_slice(units);
    }
    region[0] = (chars.len() * 2) as u8;
    for (i, unit) in chars.iter().enumerate() {
        let bytes = unit.to_le_bytes();
        region[1 + i * 2] = bytes[0];
        region[2 + i * 2] = bytes[1];
    }

    region[MACRO_COUNT_OFFSET] = events.len() as u8;
    let mut off = MACRO_EVENTS_OFFSET;
    for (kind, code, delay) in &events {
        region[off] = *kind;
        region[off + 1] = *code;
        region[off + 2] = 0x00;
        region[off + 3] = (delay >> 8) as u8;
        region[off + 4] = (*delay & 0xff) as u8;
        off += MACRO_EVENT_SIZE;
    }
    let sum: u32 = region[MACRO_COUNT_OFFSET..off]
        .iter()
        .map(|b| *b as u32)
        .sum();
    region[off] = (CHECKSUM_BASE as u32).wrapping_sub(sum & 0xff) as u8;
    Ok(region)
}

/// A macro region as the UI sees it.
#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct MacroView {
    pub slot: u8,
    pub name: String,
    /// One line per step, e.g. `Ctrl+C 120 ms`.
    pub steps: Vec<String>,
    pub events: usize,
    pub checksum_ok: bool,
}

/// Read back one region: name, steps, and whether the checksum still adds up.
///
/// `None` when the region holds no macro — erased flash or a zero count.
pub fn decode_macro(slot: u8, region: &[u8]) -> Option<MacroView> {
    if region.len() < MACRO_EVENTS_OFFSET {
        return None;
    }
    let count = region[MACRO_COUNT_OFFSET] as usize;
    if count == 0 || count == 0xff || count > MACRO_MAX_EVENTS {
        return None;
    }
    let end = MACRO_EVENTS_OFFSET + count * MACRO_EVENT_SIZE;
    if end + 1 > region.len() {
        return None;
    }
    let sum: u32 = region[MACRO_COUNT_OFFSET..end].iter().map(|b| *b as u32).sum();
    let stored = region[end];
    let checksum_ok = (CHECKSUM_BASE as u32).wrapping_sub(sum & 0xff) as u8 == stored;

    // Name: length in bytes, then UTF-16LE, kept only if it looks like text.
    let len = (region[0] as usize).min(MACRO_COUNT_OFFSET - 1) & !1;
    let mut units: Vec<u16> = Vec::new();
    for i in 0..len / 2 {
        units.push(u16::from_le_bytes([region[1 + i * 2], region[2 + i * 2]]));
    }
    let name = String::from_utf16_lossy(&units);

    // Group events into steps: a press opens one, the matching release closes
    // it. Anything left unpaired is shown on its own rather than dropped.
    let mut steps: Vec<String> = Vec::new();
    let mut pending_mods: Vec<&'static str> = Vec::new();
    let mut open_key: Option<(String, u16)> = None;
    for i in 0..count {
        let at = MACRO_EVENTS_OFFSET + i * MACRO_EVENT_SIZE;
        let kind = region[at];
        let code = region[at + 1] as u16;
        let delay = u16::from_be_bytes([region[at + 3], region[at + 4]]);
        let press = kind & EV_PRESS != 0;
        match kind & 0x3f {
            MACRO_KIND_MOD => {
                if press {
                    pending_mods = modifier_names((code & 0xff) as u8);
                } else {
                    pending_mods.clear();
                }
            }
            MACRO_KIND_KEY | MACRO_KIND_MOUSE => {
                let label = if kind & 0x3f == MACRO_KIND_KEY {
                    key_usage_name(code)
                } else {
                    mouse_button_name((code & 0xff) as u8)
                };
                if press {
                    open_key = Some((label, delay));
                } else if let Some((name, _)) = open_key.take() {
                    let mut parts: Vec<String> = pending_mods.iter().map(|s| s.to_string()).collect();
                    parts.push(name);
                    steps.push(format!("{}  {} ms", parts.join("+"), delay.max(MACRO_MIN_DELAY_MS)));
                }
            }
            _ => {}
        }
    }
    if let Some((label, delay)) = open_key {
        steps.push(format!("{label}  {delay} ms（没有松开事件）"));
    }

    Some(MacroView { slot, name, steps, events: count, checksum_ok })
}

fn modifier_names(bits: u8) -> Vec<&'static str> {
    let mut out = Vec::new();
    if bits & MOD_CTRL != 0 { out.push("Ctrl"); }
    if bits & MOD_SHIFT != 0 { out.push("Shift"); }
    if bits & MOD_ALT != 0 { out.push("Alt"); }
    if bits & MOD_GUI != 0 { out.push("Win"); }
    out
}

/// Mouse buttons as a macro records them: a bitmask, same as the button table.
fn mouse_button_name(bits: u8) -> String {
    let mut out = Vec::new();
    if bits & 0x01 != 0 { out.push("左键"); }
    if bits & 0x02 != 0 { out.push("右键"); }
    if bits & 0x04 != 0 { out.push("中键"); }
    if bits & 0x08 != 0 { out.push("后退"); }
    if bits & 0x10 != 0 { out.push("前进"); }
    if out.is_empty() {
        return format!("鼠标键 0x{bits:02x}");
    }
    out.join("+")
}

pub fn find_button_action(label: &str) -> Option<&'static ButtonAction> {
    BUTTON_ACTIONS
        .iter()
        .chain(BUTTON_ACTIONS_UNVERIFIED.iter())
        .find(|a| a.label == label)
}

/// Decode all six button records.
pub fn decode_buttons(blob: &[u8]) -> Vec<ButtonMapping> {
    BUTTON_NAMES
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let (v0, v1, v2, valid) = read_record(blob, key_record_addr(index));
            let cls = v0;
            let param = ((v1 as u16) << 8) | v2 as u16;
            ButtonMapping {
                index,
                name,
                action: button_action_label(cls, param)
                    .map(|s| s.to_string())
                    .or_else(|| class_param_label(cls, param))
                    .unwrap_or_else(|| format!("自定义 (class 0x{cls:02x} param 0x{param:04x})")),
                cls,
                param,
                valid,
            }
        })
        .collect()
}

pub fn encode_button(blob: &mut [u8], index: usize, cls: u8, param: u16) {
    write_record(
        blob,
        key_record_addr(index),
        cls,
        (param >> 8) as u8,
        param as u8,
    );
}

/// True when at least one button is still bound to left click.
///
/// Unbinding every left-click makes the mouse unusable without another mouse,
/// so every write is checked against this before it goes out.
pub fn has_left_click(blob: &[u8]) -> bool {
    (0..BUTTON_COUNT).any(|i| {
        let (v0, v1, v2, _) = read_record(blob, key_record_addr(i));
        v0 == KEY_CLASS_MOUSE && ((v1 as u16) << 8 | v2 as u16) == 0x0100
    })
}

/// Refuse a block that could not have come from this device.
///
/// `import_block` is the one path that writes bytes this program did not
/// compose itself, and a wrong guess there is expensive in a way nothing else
/// is: a block whose buttons hold no left click leaves a mouse that still
/// moves the cursor but cannot press anything, with no way back without a
/// second mouse. The import therefore checks the parts of the layout that are
/// understood before a single frame is staged, and it applies the same
/// left-click rule as `set_button` — importing reaches the same flash by
/// another door, so a guard on one door alone guards nothing.
///
/// Deliberately narrow: only the records this build decodes are checked, so a
/// block carrying settings from a newer driver still passes.
pub fn block_is_sane(blob: &[u8]) -> Result<(), String> {
    if blob.len() < CONFIG_END {
        return Err(format!(
            "这块只有 {} 字节，本设备的配置块是 {} 字节",
            blob.len(),
            CONFIG_END
        ));
    }
    if blob.iter().all(|&b| b == 0) {
        return Err("这块配置全是 0 —— 不是有效配置，多半是在设备待机时读出来的空块。".to_string());
    }

    for index in 0..BUTTON_COUNT {
        let addr = key_record_addr(index);
        let (cls, _, _, valid) = read_record(blob, addr);
        if !valid {
            return Err(format!(
                "按键 {}（0x{addr:04x}）的记录校验不符，不像是本设备的配置块",
                BUTTON_NAMES[index]
            ));
        }
        if cls > KEY_CLASS_TILT {
            return Err(format!(
                "按键 {}（0x{addr:04x}）的类型是 0x{cls:02x}，超出已知范围 0–3",
                BUTTON_NAMES[index]
            ));
        }
    }
    if !has_left_click(blob) {
        return Err(
            "这块配置里没有任何按键是左键，导入之后鼠标将无法点击。\
             请先让至少一个按键绑定为左键再导入。"
                .to_string(),
        );
    }

    let (stage, valid) = read_pair(blob, ACTIVE_STAGE_ADDR as usize);
    if !valid || stage == 0 || stage as usize > DPI_STAGE_SLOTS {
        return Err(format!(
            "当前 DPI 档位（0x{:04x}）是 {stage}，不在 1–{DPI_STAGE_SLOTS} 内：不像是本设备的配置块",
            ACTIVE_STAGE_ADDR
        ));
    }

    let (v0, _, v2, valid) = read_record(blob, DPI_STAGE_BASE);
    if !valid {
        return Err("第一档 DPI 的记录校验不符：不像是本设备的配置块".to_string());
    }
    let cpi = record_bytes_to_dpi(v0, v2);
    if !(DPI_MIN..=DPI_MAX).contains(&cpi) {
        return Err(format!(
            "第一档 DPI 是 {cpi} CPI，超出 {DPI_MIN}–{DPI_MAX}：不像是本设备的配置块"
        ));
    }
    Ok(())
}

// ── lift-off height ──────────────────────────────────────────────────────
//
// The stored byte is not a distance and not ordered: 1 is the middle setting.
// Read on this unit as 1, and every possible value below has a valid pair
// checksum on real hardware.

pub const LOD_LOW: u8 = 3;
pub const LOD_MEDIUM: u8 = 1;
pub const LOD_HIGH: u8 = 2;

pub fn lod_label(value: u8) -> Option<&'static str> {
    match value {
        LOD_LOW => Some("低"),
        LOD_MEDIUM => Some("中"),
        LOD_HIGH => Some("高"),
        _ => None,
    }
}

// ── DPI indicator light ──────────────────────────────────────────────────
//
// Mode and state are separate controls on this layout: `state` gates the LED
// entirely, `mode` picks solid or breathing within it. Reading them apart and
// combining them only for display keeps "mode says breathe but the LED is
// off" from being reported as "breathing".

/// Mode as shown to the user: 0 off, 1 solid, 2 breathing.
pub fn dpi_light_mode(mode_byte: u8, state_byte: u8) -> u8 {
    if state_byte != 1 {
        0
    } else if mode_byte == 2 {
        2
    } else {
        1
    }
}

/// The two flash bytes for a display mode, written together.
pub fn dpi_light_mode_bytes(display_mode: u8) -> (u8, u8) {
    if display_mode == 0 {
        (0, 0)
    } else {
        (if display_mode == 2 { 2 } else { 1 }, 1)
    }
}

/// Brightness levels are 1..10, but the stored byte is not linear — the vendor
/// slider maps through a short table. Unknown bytes fall back to 5, which is
/// what the reference driver does with them.
pub fn decode_light_brightness(stored: u8) -> u8 {
    if stored != 0 && stored % 0x1e == 0 {
        let level = stored / 0x1e + 1;
        if (2..=8).contains(&level) {
            return level;
        }
    }
    match stored {
        0x10 => 1,
        0x80 => 5,
        0xe6 => 9,
        0xff => 10,
        _ => 5,
    }
}

pub fn encode_light_brightness(level: u8) -> u8 {
    match level.clamp(1, 10) {
        1 => 0x10,
        5 => 0x80,
        9 => 0xe6,
        10 => 0xff,
        n => 0x1e * (n - 1),
    }
}

// ── DPI encoding ─────────────────────────────────────────────────────────

pub const DPI_STEP: u32 = 50;
pub const DPI_MIN: u32 = 50;
pub const DPI_MAX: u32 = 26000;
/// Scale factor the device applies to the high byte of the sensor code.
pub const DPI_HIGH_SCALE: u32 = 0x44;

pub fn snap_dpi(dpi: u32) -> u32 {
    let snapped = ((dpi + DPI_STEP / 2) / DPI_STEP) * DPI_STEP;
    snapped.clamp(DPI_MIN, DPI_MAX)
}

pub fn dpi_to_code(dpi: u32) -> u32 {
    (snap_dpi(dpi) - DPI_MIN) / DPI_STEP
}

pub fn code_to_dpi(code: u32) -> u32 {
    code * DPI_STEP + DPI_MIN
}

/// `(v0, v1, v2)` for a stage record holding `dpi` (the checksum is separate).
pub fn dpi_to_record_bytes(dpi: u32) -> (u8, u8, u8) {
    let code = dpi_to_code(dpi);
    let low = (code & 0xff) as u8;
    (low, low, ((code >> 8) * DPI_HIGH_SCALE & 0xff) as u8)
}

pub fn record_bytes_to_dpi(v0: u8, v2: u8) -> u32 {
    let high = ((v2 as u32 + DPI_HIGH_SCALE / 2) / DPI_HIGH_SCALE) & 0xff;
    code_to_dpi((high << 8) | v0 as u32)
}

// ── report rate ─────────────────────────────────────────────────────────
//
// Ported from the FerrisPulse Metapanda adapter (`MetapandaProtocol.cs`),
// which captured all six from the official driver and then confirmed each one
// by writing it to a real F5AC receiver and reading it back. The codes are
// NOT an encoding of the Hz number: 125..1000 and 2000/4000 use different
// arrangements, so 8000 Hz cannot be extrapolated from them and is not
// offered.

/// `(Hz, code)` pairs, verified on hardware. Sorted fastest code first is not
/// possible: the table is stored the way it was measured.
const RATE_TABLE: [(u32, u8); 6] = [
    (125, 0x08),
    (250, 0x04),
    (500, 0x02),
    (1000, 0x01),
    (2000, 0x10),
    (4000, 0x20),
];

/// The code for a report rate. `None` when the rate was never verified.
pub fn rate_to_code(hz: u32) -> Option<u8> {
    RATE_TABLE.iter().find(|(h, _)| *h == hz).map(|(_, c)| *c)
}

/// The rate a stored code means. `None` for a code nobody has measured.
pub fn code_to_rate(code: u8) -> Option<u32> {
    RATE_TABLE.iter().find(|(_, c)| *c == code).map(|(h, _)| *h)
}

/// Rates this endpoint may be set to, in Hz. From the product catalogue.
pub fn rates_for(pid: u16) -> &'static [u32] {
    product_for(pid).rates
}

// ── battery voltage curve ────────────────────────────────────────────────
//
// The P1 reports its charge as a coarse 5 %-step level, and separately the
// cell voltage. Level 85 can sit there while the voltage walks from 4055 mV
// down to 4030 mV, so refreshing looked broken when only the level was used.
//
// The 21-point table below is the calibration the official 1.0.0.4 driver
// uses; the FerrisPulse adapter cross-checked it by reading the same device
// (4038 mV -> 84 %, 4055 mV -> 88 %) and the official UI agreed. It is a
// discharge curve for this one cell, not a universal Li-ion formula, and it
// says nothing while charging — charge voltage includes the charging effect.

/// Millivolts at 0, 5, 10 … 100 percent. One entry per five percent.
const BATTERY_CURVE: [u16; 21] = [
    3050, 3420, 3480, 3540, 3600, 3660, 3720, 3760, 3800, 3840, 3880, 3920, 3940, 3960, 3980,
    4000, 4020, 4040, 4060, 4080, 4110,
];

/// Percent from a discharge voltage, linearly interpolated between points.
///
/// `None` when the reading cannot be trusted: no voltage at all, or one
/// outside 2500..4500 mV, which no single-cell Li-ion can be. The caller then
/// keeps the device's own level instead of inventing a number.
pub fn discharge_estimate(millivolts: u16) -> Option<f32> {
    if millivolts == 0 || millivolts < 2500 || millivolts > 4500 {
        return None;
    }
    if millivolts <= BATTERY_CURVE[0] {
        return Some(0.0);
    }
    let last = BATTERY_CURVE[BATTERY_CURVE.len() - 1];
    if millivolts >= last {
        return Some(100.0);
    }
    for upper in 1..BATTERY_CURVE.len() {
        if millivolts > BATTERY_CURVE[upper] {
            continue;
        }
        let low = BATTERY_CURVE[upper - 1];
        let span = (BATTERY_CURVE[upper] - low) as f32;
        let base = ((upper - 1) * 5) as f32;
        return Some(base + (millivolts - low) as f32 * 5.0 / span);
    }
    Some(100.0)
}

// ── decoded snapshot ─────────────────────────────────────────────────────

#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub dpi_stages: Vec<u32>,
    /// Y-axis CPI of each stage, parallel to `dpi_stages`.
    ///
    /// A stage record is `[x][y][high][checksum]`: the two axes are stored
    /// separately, and the official driver lets them differ. Read but not
    /// written — a mouse that cannot hold independent axes would only be
    /// confused by a panel that offers to set them.
    pub dpi_stages_y: Vec<u32>,
    /// True when at least one stage holds a different Y than X.
    pub xy_independent: bool,
    /// 0-based index into `dpi_stages`.
    pub active_stage: usize,
    pub stage_count: usize,
    pub debounce: Option<u8>,
    /// 0x00AD timeout in 10-second units; 6 is one minute.
    ///
    /// Never measured on the hardware until it mattered: the app used to write
    /// 0x00B7 here, which is the low half of the sensor row's marker, so the
    /// number in the UI changed while the mouse kept its factory timeout. Then
    /// it was measured — set to 1, the mouse drops off the link in about
    /// twenty seconds; left at 6 it stays up for minutes.
    pub sleep_timeout: u8,
    /// 0x00B9 performance mode of the sensor row: 0 basic, 1 competitive
    /// firmware. Written only when the user asks for a mode change.
    pub sensor_mode: u8,
    pub motion_sync: bool,
    pub ripple_control: bool,
    /// 0x00AF: angle / straight-line correction.
    pub angle_snapping: bool,
    /// The six physical buttons and what each is bound to.
    pub buttons: Vec<ButtonMapping>,
    /// The DPI-stage indicator LED: display mode, brightness level, speed.
    pub dpi_light: DpiLight,
    /// `#rrggbb` per usable colour record.
    pub colors: Vec<String>,
    /// 0x0000 report-rate byte: 0x01 = 1K, 0x20 = 4K, 0x40 = 8K.
    pub report_rate: u8,
    /// What that byte means in Hz. `None` for a code nobody has measured,
    /// which is the honest answer for 0x40 and anything else unrecorded.
    pub rate_hz: Option<u32>,
    /// 0x000A: lift-off / silence height as the device stores it.
    pub lod: u8,
    /// What that byte means, from the shared map. `None` for an unrecorded
    /// value, so the UI can fall back to showing the raw number.
    pub lod_name: Option<&'static str>,
    /// Shortcut slots a class-5 button points at, decoded for display.
    ///
    /// Filled in by the caller, not by [`decode`]: the slots live outside the
    /// 192-byte block, and reading all sixteen costs 52 frames for nothing on a
    /// mouse where no button uses one. `slot` is the 0-based index, `label` is
    /// `None` when the slot is blank or cannot be parsed.
    #[serde(default)]
    pub shortcuts: Vec<ShortcutRef>,
    /// Macros bound to buttons, read out of their own regions. `None` for a
    /// button bound to a macro whose region could not be read.
    #[serde(default)]
    pub macros: Vec<Option<MacroView>>,
}

/// One class-5 binding's underlying slot, as the UI sees it.
#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutRef {
    pub slot: u8,
    pub label: Option<String>,
}

/// The LED that shows which DPI stage is live.
#[derive(Serialize, Clone, Copy, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct DpiLight {
    /// Display mode: 0 off, 1 solid, 2 breathing.
    pub mode: u8,
    /// Brightness, 1..10.
    pub brightness: u8,
    /// Effect speed.
    pub speed: u8,
}


/// Decode a raw configuration block. Addresses past the end read as 0, so a
/// short buffer degrades instead of panicking.
pub fn decode(blob: &[u8]) -> Snapshot {
    let declared = read_record(blob, STAGE_COUNT_ADDR).0 as usize;
    let stage_count = declared.clamp(1, DPI_STAGE_SLOTS);

    let mut dpi_stages = Vec::with_capacity(stage_count);
    let mut dpi_stages_y = Vec::with_capacity(stage_count);
    for i in 0..stage_count {
        let (v0, v1, v2, _) = read_record(blob, DPI_STAGE_BASE + i * 4);
        dpi_stages.push(record_bytes_to_dpi(v0, v2));
        // The single high byte is shared by both axes, so it is applied to Y
        // as well: identical axes — the usual case — then read correctly, and
        // a genuine X/Y split below the high range reads correctly too. The
        // one case it cannot express (X high and Y low) is left unguessed.
        dpi_stages_y.push(record_bytes_to_dpi(v1, v2));
    }
    let xy_independent = dpi_stages
        .iter()
        .zip(dpi_stages_y.iter())
        .any(|(x, y)| x != y);

    let (dv0, _, _, dvalid) = read_record(blob, DEBOUNCE_ADDR);

    let mut colors = Vec::new();
    for i in 0..COLOR_COUNT {
        let (v0, v1, v2, valid) = read_record(blob, COLOR_BASE + i * 4);
        if valid {
            colors.push(format!("#{v0:02x}{v1:02x}{v2:02x}"));
        }
    }


    let raw_stage = blob.get(ACTIVE_STAGE_ADDR as usize).copied().unwrap_or(0);
    let active_stage = decode_active_stage(raw_stage, stage_count);

    Snapshot {
        dpi_stages,
        dpi_stages_y,
        xy_independent,
        active_stage,
        stage_count,
        debounce: if dvalid { Some(dv0) } else { None },
        sleep_timeout: read_pair(blob, SLEEP_TIME_ADDR).0,
        sensor_mode: read_pair(blob, SENSOR_MODE_ADDR).0,
        motion_sync: read_pair(blob, MOTION_SYNC_ADDR).0 > 0,
        ripple_control: read_pair(blob, RIPPLE_ADDR).0 > 0,
        angle_snapping: read_pair(blob, ANGLE_SNAPPING_ADDR).0 > 0,
        buttons: decode_buttons(blob),
        dpi_light: DpiLight {
            mode: dpi_light_mode(
                read_pair(blob, DPI_LIGHT_MODE_ADDR).0,
                read_pair(blob, DPI_LIGHT_STATE_ADDR).0,
            ),
            brightness: decode_light_brightness(read_pair(blob, DPI_LIGHT_BRIGHTNESS_ADDR).0),
            speed: read_pair(blob, DPI_LIGHT_SPEED_ADDR).0.clamp(1, 5),
        },
        colors,
        report_rate: blob.first().copied().unwrap_or(0),
        rate_hz: code_to_rate(blob.first().copied().unwrap_or(0)),
        lod: read_pair(blob, LOD_ADDR).0,
        lod_name: lod_label(read_pair(blob, LOD_ADDR).0),
        shortcuts: Vec::new(),
        macros: Vec::new(),
    }
}

/// 1-based grade at 0x0004 -> 0-based, clamped index.
pub fn decode_active_stage(raw: u8, stage_count: usize) -> usize {
    let idx = raw as isize - 1;
    if idx < 0 || idx as usize >= stage_count {
        // Tolerate a 0-based firmware or a stray byte without hiding the setting.
        (raw as usize).min(stage_count.saturating_sub(1))
    } else {
        idx as usize
    }
}

// ── patch / encode ───────────────────────────────────────────────────────

/// The subset of `MouseState` this driver can write. Unknown fields in the
/// incoming JSON are ignored on purpose, so the UI may send the whole state.
#[derive(Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct StatePatch {
    pub dpi_stages: Option<Vec<u32>>,
    /// How many of the eight stage slots the mouse cycles through when you
    /// press the DPI button (`DPIMaxGrade` at [`STAGE_COUNT_ADDR`]).
    ///
    /// The vendor drivers expose it as a setting — Darmoshark calls it
    /// `DPI Count`, Pulsar's `Config.ini` carries `DPIMaxGrade = 6` — and the
    /// record on this unit reads 7, so it is writable here rather than fixed.
    pub stage_count: Option<u8>,
    pub active_stage: Option<usize>,
    pub debounce: Option<u8>,
    /// 0x00AD, in 10-second units.
    pub sleep_timeout: Option<u8>,
    /// 0x00B9, 0..2.
    pub sensor_mode: Option<u8>,
    pub motion_sync: Option<bool>,
    pub ripple_control: Option<bool>,
    pub angle_snapping: Option<bool>,
    /// A [`LOD_LOW`] / [`LOD_MEDIUM`] / [`LOD_HIGH`] value.
    pub lod: Option<u8>,
    /// Display mode for the DPI indicator: 0 off, 1 solid, 2 breathing.
    pub dpi_light_mode: Option<u8>,
    /// Brightness level, 1..10.
    pub dpi_light_brightness: Option<u8>,
    /// Effect speed, 1..5.
    pub dpi_light_speed: Option<u8>,
}

/// Patch a configuration block in place. Records keep valid checksums.
/// Apply a patch to a block. Everything is clamped rather than rejected —
/// refusing a whole batch of settings over one out-of-range slider would be
/// worse than nudging that slider.
pub fn encode(blob: &mut [u8], patch: &StatePatch) {
    if let Some(stages) = patch.dpi_stages.as_ref().filter(|s| !s.is_empty()) {
        let stages: Vec<u32> = stages.iter().copied().take(DPI_STAGE_SLOTS).collect();
        // Keep the declared live count in step with what we are about to store.
        let (_, v1, v2, _) = read_record(blob, STAGE_COUNT_ADDR);
        write_record(blob, STAGE_COUNT_ADDR, stages.len() as u8, v1, v2);
        for (i, dpi) in stages.iter().enumerate() {
            let (v0, v1, v2) = dpi_to_record_bytes(*dpi);
            write_record(blob, DPI_STAGE_BASE + i * 4, v0, v1, v2);
        }
    }

    // `DPIMaxGrade` on its own: change how many of the stored stages the DPI
    // button cycles through without touching their values. Only the first byte
    // of the record is the count; the rest belongs to something else, so it is
    // carried over rather than zeroed.
    if let Some(count) = patch.stage_count {
        let count = count.clamp(1, DPI_STAGE_SLOTS as u8);
        let (_, v1, v2, _) = read_record(blob, STAGE_COUNT_ADDR);
        write_record(blob, STAGE_COUNT_ADDR, count, v1, v2);
        // A stage that is no longer live cannot stay selected.
        let (raw, _) = read_pair(blob, ACTIVE_STAGE_ADDR as usize);
        let clamped = decode_active_stage(raw, count as usize);
        write_pair(blob, ACTIVE_STAGE_ADDR as usize, (clamped + 1) as u8);
    }

    if let Some(stage) = patch.active_stage {
        // Stored 1-based; the pair partner holds 0x55 - grade.
        let grade = (stage + 1).min(DPI_STAGE_SLOTS) as u8;
        write_pair(blob, ACTIVE_STAGE_ADDR as usize, grade);
    }

    if let Some(debounce) = patch.debounce {
        // A 4-byte record, not a pair: only the first byte is the value, and
        // the two bytes after it belong to something else.
        let (_, v1, v2, _) = read_record(blob, DEBOUNCE_ADDR);
        write_record(blob, DEBOUNCE_ADDR, debounce, v1, v2);
        // And the copy in the advanced row, which this firmware may read
        // instead — the two do not mirror each other.
        write_pair(blob, ADVANCED_DEBOUNCE_ADDR, debounce);
    }

    if let Some(timeout) = patch.sleep_timeout {
        write_pair(blob, SLEEP_TIME_ADDR, timeout);
    }
    if let Some(mode) = patch.sensor_mode {
        write_pair(blob, SENSOR_MODE_ADDR, mode);
    }
    if let Some(on) = patch.motion_sync {
        write_pair(blob, MOTION_SYNC_ADDR, on as u8);
    }
    if let Some(on) = patch.ripple_control {
        write_pair(blob, RIPPLE_ADDR, on as u8);
    }
    if let Some(on) = patch.angle_snapping {
        write_pair(blob, ANGLE_SNAPPING_ADDR, on as u8);
    }
    if let Some(lod) = patch.lod {
        write_pair(blob, LOD_ADDR, lod);
    }
    // Mode is two bytes that have to agree, so they always move together —
    // writing the mode alone would leave the LED gated off by a stale state.
    if let Some(mode) = patch.dpi_light_mode {
        let (mode_byte, state_byte) = dpi_light_mode_bytes(mode.min(2));
        write_pair(blob, DPI_LIGHT_MODE_ADDR, mode_byte);
        write_pair(blob, DPI_LIGHT_STATE_ADDR, state_byte);
    }
    if let Some(level) = patch.dpi_light_brightness {
        write_pair(
            blob,
            DPI_LIGHT_BRIGHTNESS_ADDR,
            encode_light_brightness(level),
        );
    }
    if let Some(speed) = patch.dpi_light_speed {
        write_pair(blob, DPI_LIGHT_SPEED_ADDR, speed.clamp(1, 5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real 192-byte factory block, read from the device.
    const FACTORY: [u8; 192] = [
        0x01, 0x54, 0x07, 0x4e, 0x02, 0x53, 0x00, 0x55, 0x00, 0x55, 0x01, 0x54, 0x0f, 0x0f, 0x00,
        0x37, 0x1f, 0x1f, 0x00, 0x17, 0x2f, 0x2f, 0x00, 0xf7, 0x3f, 0x3f, 0x00, 0xd7, 0x7f, 0x7f,
        0x00, 0x57, 0x03, 0x03, 0x44, 0x0b, 0x07, 0x07, 0x88, 0xbf, 0x07, 0x07, 0x88, 0xbf, 0x00,
        0x00, 0xff, 0x56, 0x00, 0xff, 0x00, 0x56, 0xff, 0xff, 0x00, 0x57, 0x00, 0xff, 0xff, 0x57,
        0xff, 0x80, 0x00, 0xd6, 0xff, 0x00, 0xff, 0x57, 0xff, 0xff, 0xff, 0x58, 0xff, 0xff, 0xff,
        0x58, 0x02, 0x53, 0x80, 0xd5, 0x03, 0x52, 0x00, 0x55, 0xff, 0x00, 0xff, 0x57, 0x00, 0x55,
        0x80, 0xd5, 0x03, 0x52, 0x00, 0x55, 0x01, 0x01, 0x00, 0x53, 0x01, 0x02, 0x00, 0x52, 0x01,
        0x04, 0x00, 0x50, 0x01, 0x08, 0x00, 0x4c, 0x01, 0x10, 0x00, 0x44, 0x02, 0x01, 0x00, 0x52,
        0x02, 0x02, 0x00, 0x51, 0x02, 0x03, 0x00, 0x50, 0x07, 0x00, 0x00, 0x4e, 0x08, 0x04, 0x00,
        0x49, 0x04, 0x0a, 0x03, 0x44, 0x00, 0x00, 0x00, 0x55, 0x00, 0x00, 0x00, 0x55, 0x00, 0x00,
        0x00, 0x55, 0x00, 0x00, 0x00, 0x55, 0x00, 0x00, 0x00, 0x55, 0x01, 0xff, 0x00, 0xff, 0x07,
        0x09, 0x46, 0x00, 0x55, 0x08, 0x4d, 0x01, 0x54, 0x06, 0x4f, 0x00, 0x55, 0x00, 0x55, 0x00,
        0x55, 0x00, 0x55, 0x06, 0x4f, 0x00, 0x55, 0x0a, 0x4b, 0xff, 0xff, 0xff,
    ];

    #[test]
    fn frame_checksum_matches_the_device() {
        // Real read request the driver sends for 0x0000, len 10.
        let f = build_frame(cmd::READ, 0x0000, &[0x02, 0x02, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(f[0], 0x08);
        assert_eq!(f[5], 0x0a);
        assert_eq!(checksum(&f[..16]), f[16]);
    }

    #[test]
    fn decodes_the_real_block() {
        let s = decode(&FACTORY);
        assert_eq!(s.dpi_stages, vec![800, 1600, 2400, 3200, 6400, 13000, 26000]);
        assert_eq!(s.stage_count, 7);
        // 0x0004 = 0x02 -> grade 2 -> 0-based index 1 (1600 CPI).
        assert_eq!(s.active_stage, 1);
        assert_eq!(s.debounce, Some(8));
        assert_eq!(s.sleep_timeout, 6);
        assert_eq!(s.sensor_mode, 0);
        assert_eq!(s.report_rate, 0x01);
    }

    /// The six records at 0x0060 decode to exactly what a mouse ships with.
    /// Checksums all agree, which is what makes the offset believable.
    #[test]
    fn decodes_the_key_table() {
        let s = decode(&FACTORY);
        let labels: Vec<&str> = s.buttons.iter().map(|b| b.action.as_str()).collect();
        assert_eq!(labels, vec!["左键", "右键", "中键", "后退", "前进", "DPI 循环"]);
        assert!(s.buttons.iter().all(|b| b.valid));
        // Losing this one would leave a mouse nobody can click. See the guard
        // on writes in `lib.rs`.
        assert!(has_left_click(&FACTORY));
    }

    /// Debounce is the first byte of the record at 0x0084.
    ///
    /// Both copies are written and neither disturbs its neighbours.
    #[test]
    fn debounce_is_written_to_both_of_its_copies() {
        assert_eq!(DEBOUNCE_ADDR, 0x0084);
        assert_eq!(decode(&FACTORY).debounce, Some(8));

        let mut block = FACTORY.to_vec();
        encode(&mut block, &StatePatch { debounce: Some(20), ..Default::default() });
        assert_eq!(decode(&block).debounce, Some(20));
        // Only the value byte moves; its two neighbours are not ours to touch.
        assert_eq!(block[0x0085], FACTORY[0x0085]);
        assert_eq!(block[0x0086], FACTORY[0x0086]);
        // The advanced row's copy follows, because the two bytes are
        // independent and only one of them is the one the firmware reads.
        assert_eq!(block[ADVANCED_DEBOUNCE_ADDR], 20);
        assert_eq!(block[ADVANCED_DEBOUNCE_ADDR + 1], 0x55 - 20);
        // Its neighbours in that row (motion sync at 0xab/0xac) stay put.
        assert_eq!(block[MOTION_SYNC_ADDR], FACTORY[MOTION_SYNC_ADDR]);
        assert_eq!(block[MOTION_SYNC_ADDR + 1], FACTORY[MOTION_SYNC_ADDR + 1]);
    }

    #[test]
    fn rebinding_a_button_keeps_the_record_valid() {
        let mut block = FACTORY.to_vec();
        encode_button(&mut block, 3, KEY_CLASS_DPI, 0x0300); // Back -> DPI-
        let s = decode(&block);
        assert_eq!(s.buttons[3].action, "DPI -");
        assert!(s.buttons[3].valid);
        // Untouched neighbours stay as they were.
        assert_eq!(s.buttons[2].action, "中键");
        assert!(has_left_click(&block));
    }

    #[test]
    fn unbinding_every_left_click_is_detected() {
        let mut block = FACTORY.to_vec();
        encode_button(&mut block, 0, KEY_CLASS_DISABLE, 0x0000);
        assert!(!has_left_click(&block));
    }

    #[test]
    fn lod_and_indicator_light_come_from_verified_offsets() {
        let s = decode(&FACTORY);
        // Stored 1 is the middle setting on this layout, not the lowest.
        assert_eq!(s.lod, LOD_MEDIUM);
        assert_eq!(s.lod_name, Some("中"));
        // Mode says "breathing" but state gates the LED off, so it reads off.
        assert_eq!(s.dpi_light.mode, 0);
        assert_eq!(s.dpi_light.brightness, 5);
        assert_eq!(s.dpi_light.speed, 3);
        assert!(!s.angle_snapping);
    }

    /// `DPIMaxGrade` decides how many of the eight stored stages the DPI button
    /// walks through. Two things are easy to get wrong and both are checked
    /// here: the two bytes after the count are not ours to zero, and a stage
    /// that stops being live cannot stay selected.
    #[test]
    fn encoding_a_shortcut_slot_reproduces_the_vendor_bytes_byte_for_byte() {
        // The Ctrl+C record both reference sources captured out of their
        // vendor software: 04 | 80 01 00 | 81 06 00 | 40 01 00 | 41 06 00 | c1
        let slot = encode_shortcut_slot(&shortcut_events(MOD_CTRL, 0x06, false)).unwrap();
        assert_eq!(
            &slot[..14],
            &[0x04, 0x80, 0x01, 0x00, 0x81, 0x06, 0x00, 0x40, 0x01, 0x00, 0x41, 0x06, 0x00, 0xc1]
        );
        // Everything past the checksum stays erased, so the tail length cannot
        // change what the firmware reads.
        assert!(slot[14..].iter().all(|&b| b == 0xff));
        assert_eq!(decode_shortcut_slot(&slot).as_deref(), Some("Ctrl+C"));

        // A multimedia key is two events: press then release of the same usage.
        let media = encode_shortcut_slot(&shortcut_events(0, 0x00cd, true)).unwrap();
        assert_eq!(&media[..8], &[0x02, 0x82, 0xcd, 0x00, 0x42, 0xcd, 0x00, 0xf5]);
        assert_eq!(decode_shortcut_slot(&media).as_deref(), Some("播放/暂停"));
    }

    #[test]
    fn a_shortcut_slot_round_trips_and_refuses_what_does_not_fit() {
        for (mods, usage, consumer) in [
            (0, 0x04, false),
            (MOD_CTRL | MOD_SHIFT, 0x3e, false),
            (MOD_GUI, 0x00ea, true),
            (MOD_CTRL | MOD_ALT | MOD_SHIFT | MOD_GUI, 0x1e, false),
        ] {
            let want = shortcut_target_label(mods, usage, consumer).unwrap();
            let slot = encode_shortcut_slot(&shortcut_events(mods, usage, consumer)).unwrap();
            assert_eq!(decode_shortcut_slot(&slot).as_deref(), Some(want.as_str()));
        }
        // Ten events is the most the checksum leaves room for; eleven is not a
        // slot any more.
        let too_many: Vec<(u8, u16)> = (0..11).map(|_| (EV_KIND_KEY | EV_PRESS, 0x04)).collect();
        assert!(encode_shortcut_slot(&too_many).is_none());
        assert!(encode_shortcut_slot(&[]).is_none());
    }

    #[test]
    fn decodes_a_shortcut_slot_like_the_vendor_web_driver_writes_it() {
        // An event list, not a press/release pair: the checksum is left out of
        // the fixtures on purpose. The padding behind the events was measured
        // as both 0x00 and 0xff round-tripping on this receiver, so a hand-made
        // checksum here would only prove the writer agrees with itself — the
        // encode test above checks it against the vendor's own bytes.
        let mut slot = [0xffu8; 32];
        let events = [0x02u8, 0x81, 0x06, 0x00, 0x41, 0x06, 0x00];
        slot[..events.len()].copy_from_slice(&events);
        assert_eq!(decode_shortcut_slot(&slot).as_deref(), Some("C"));

        // 修饰键事件：kind 0，code 是位掩码。Ctrl+Shift+F5（usage 0x3e）。
        let mut combo = [0xffu8; 32];
        let events = [0x04u8, 0x80, 0x03, 0x00, 0x81, 0x3e, 0x00, 0x40, 0x03, 0x00, 0x41, 0x3e, 0x00];
        combo[..events.len()].copy_from_slice(&events);
        assert_eq!(decode_shortcut_slot(&combo).as_deref(), Some("Ctrl+Shift+F5"));

        // 多媒体 usage 0x00CD（播放/暂停）。
        let mut media = [0xffu8; 32];
        let events = [0x02u8, 0x82, 0xcd, 0x00, 0x42, 0xcd, 0x00];
        media[..events.len()].copy_from_slice(&events);
        assert_eq!(decode_shortcut_slot(&media).as_deref(), Some("播放/暂停"));

        // 擦除态（全 0xff）算空；事件数超出槽长也算空。
        assert_eq!(decode_shortcut_slot(&[0xffu8; 32]), None);
        assert_eq!(decode_shortcut_slot(&[0x02, 0x81, 0x06]), None);
        // 只有松开事件、没有按下事件的槽，读出来是没有名字的。
        let release_only = [0x01u8, 0x41, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(decode_shortcut_slot(&release_only), None);
    }

    #[test]
    fn a_macro_region_carries_its_name_count_events_and_a_checksum_over_the_events() {
        let steps = vec![
            MacroStep { modifiers: MOD_CTRL, usage: 0x06, delay_ms: 120, keyboard: true },
            MacroStep { modifiers: 0, usage: 0x04, delay_ms: 0, keyboard: true },
        ];
        let region = encode_macro("测试", &steps).unwrap();

        // 0x00 is the name length in BYTES, not characters: two characters are
        // four bytes of UTF-16LE.
        assert_eq!(region[0], 4);
        assert_eq!(&region[1..5], &[0x4b, 0x6d, 0xd5, 0x8b]);
        // The count sits at 0x1f, below the events, not above them.
        // Ctrl+C is four events, a bare key is two.
        assert_eq!(region[MACRO_COUNT_OFFSET], 6);

        // First event: modifier press, minimum delay.
        assert_eq!(&region[MACRO_EVENTS_OFFSET..MACRO_EVENTS_OFFSET + 5], &[0x80, 0x01, 0x00, 0x00, 0x03]);
        // The step's delay rides the key release, and it is big-endian.
        assert_eq!(
            &region[MACRO_EVENTS_OFFSET + 10..MACRO_EVENTS_OFFSET + 15],
            &[0x41, 0x06, 0x00, 0x00, 120]
        );
        // A zero delay is clamped to the floor the firmware honours.
        assert_eq!(region[MACRO_EVENTS_OFFSET + 5 * MACRO_EVENT_SIZE + 4], 3);

        let view = decode_macro(3, &region).unwrap();
        assert_eq!(view.steps.len(), 2);
        assert_eq!(view.name, "测试");
        assert_eq!(view.events, 6);
        assert_eq!(view.steps[0], "Ctrl+C  120 ms");
        assert_eq!(view.steps[1], "A  3 ms");
        assert!(view.checksum_ok);

        // The checksum covers the count byte and the events, never the name.
        let mut renamed = region;
        renamed[3] ^= 0x20;
        assert!(decode_macro(3, &renamed).unwrap().checksum_ok);
        let mut broken = region;
        broken[MACRO_EVENTS_OFFSET + 1] ^= 0x01;
        assert!(!decode_macro(3, &broken).unwrap().checksum_ok);
    }

    #[test]
    fn a_macro_refuses_to_be_empty_or_longer_than_the_region() {
        assert!(encode_macro("x", &[]).is_err());
        // 70 events is the cap; one step is at most four of them, so nineteen
        // modifier steps already overflow it.
        let many: Vec<MacroStep> = (0..19)
            .map(|_| MacroStep { modifiers: MOD_CTRL, usage: 0x06, delay_ms: 10, keyboard: true })
            .collect();
        assert!(macro_event_count(&many) == 76);
        assert!(encode_macro("x", &many).is_err());
        let fits: Vec<MacroStep> = (0..17)
            .map(|_| MacroStep { modifiers: MOD_CTRL, usage: 0x06, delay_ms: 10, keyboard: true })
            .collect();
        assert!(encode_macro("x", &fits).is_ok());
        // An erased region reads as no macro at all rather than an empty one.
        assert!(decode_macro(0, &[0xffu8; MACRO_STRIDE]).is_none());
    }

    /// `DPIMaxGrade` decides how many of the eight stored stages the DPI button
    /// walks through. Two things are easy to get wrong and both are checked
    /// here: the two bytes after the count are not ours to zero, and a stage
    /// that stops being live cannot stay selected.
    #[test]
    fn the_stage_count_record_keeps_its_neighbours_and_clamps_the_active_stage() {        let mut block = FACTORY.to_vec();
        // Factory declares 7 live stages with the DPI button parked on the last.
        assert_eq!(decode(&block).stage_count, 7);
        let (_, v1, v2, _) = read_record(&block, STAGE_COUNT_ADDR);

        encode(
            &mut block,
            &StatePatch {
                stage_count: Some(3),
                ..Default::default()
            },
        );
        let (count, after1, after2, valid) = read_record(&block, STAGE_COUNT_ADDR);
        assert_eq!(count, 3);
        assert_eq!((after1, after2), (v1, v2), "the bytes beside the count moved");
        assert!(valid);
        // Active stage was 7, which no longer exists; it must not stay there.
        assert!(decode(&block).active_stage < 3);

        // Out-of-range counts are clamped, never written raw: a 0 or a 9 would
        // leave the mouse cycling through nothing, or past the last record.
        for (asked, expected) in [(0u8, 1u8), (9, DPI_STAGE_SLOTS as u8)] {
            let mut b = FACTORY.to_vec();
            encode(&mut b, &StatePatch { stage_count: Some(asked), ..Default::default() });
            assert_eq!(read_record(&b, STAGE_COUNT_ADDR).0, expected);
        }
    }

    #[test]
    fn writing_the_indicator_light_moves_both_bytes() {
        let mut block = FACTORY.to_vec();
        encode(
            &mut block,
            &StatePatch {
                dpi_light_mode: Some(2),
                ..Default::default()
            },
        );
        let s = decode(&block);
        // Mode alone would be ignored by the device: state has to say on too.
        assert_eq!(s.dpi_light.mode, 2);
        assert_eq!(block[DPI_LIGHT_STATE_ADDR], 0x01);

        encode(
            &mut block,
            &StatePatch {
                dpi_light_mode: Some(0),
                ..Default::default()
            },
        );
        assert_eq!(decode(&block).dpi_light.mode, 0);
        assert_eq!(block[DPI_LIGHT_STATE_ADDR], 0x00);
    }

    /// The brightness slider is not linear, so round-tripping the levels the
    /// UI offers is worth pinning down.
    #[test]
    fn brightness_levels_round_trip() {
        for level in 1..=10u8 {
            assert_eq!(decode_light_brightness(encode_light_brightness(level)), level);
        }
    }

    /// The sleep timer is 0xAD, and the six bytes at 0xB5 are a sensor row
    /// whose marker happens to look like a value/checksum pair. Writing a
    /// timeout must not touch either of them — the app used to write 0xB7 and
    /// call it the timeout, which changed the UI and nothing else.
    #[test]
    fn the_sleep_timeout_lives_at_0x00ad_and_leaves_the_sensor_row_alone() {
        let mut block = FACTORY.to_vec();
        let sensor_row = block[SENSOR_MODE_ADDR - 4..SENSOR_MODE_ADDR + 2].to_vec();
        encode(
            &mut block,
            &StatePatch { sleep_timeout: Some(30), ..Default::default() },
        );
        assert_eq!(block[SLEEP_TIME_ADDR], 30);
        assert_eq!(block[SLEEP_TIME_ADDR + 1], (0x55u8).wrapping_sub(30));
        assert_eq!(decode(&block).sleep_timeout, 30);
        // 0xB5..0xBA untouched: the marker pair is not ours to rewrite.
        assert_eq!(&block[SENSOR_MODE_ADDR - 4..SENSOR_MODE_ADDR + 2], &sensor_row[..]);

        // The mode byte is its own pair, and only it moves.
        encode(
            &mut block,
            &StatePatch { sensor_mode: Some(1), ..Default::default() },
        );
        assert_eq!(block[SENSOR_MODE_ADDR], 1);
        assert_eq!(block[SENSOR_MODE_ADDR + 1], 0x54);
        assert_eq!(decode(&block).sensor_mode, 1);
        assert_eq!(block[SLEEP_TIME_ADDR], 30, "the mode write moved the timer");
    }

    #[test]
    fn dpi_round_trips_through_the_sensor_code() {
        for dpi in [400u32, 800, 1600, 6400, 12800, 13000, 26000] {
            let (v0, _, v2) = dpi_to_record_bytes(dpi);
            assert_eq!(record_bytes_to_dpi(v0, v2), snap_dpi(dpi), "dpi {dpi}");
        }
    }

    // ── the import check ────────────────────────────────────────────────
    //
    // A backup of the real device is the block this check exists to let
    // through, and the two ways a file can be wrong are the ones it must
    // stop: nothing in it, and nothing left to click with.

    /// The 192-byte block as this unit actually holds it: 4000 Hz, stage 3,
    /// LOD medium, seven DPI stages, the six factory bindings.
    const UNIT_BLOCK_HEX: &str = concat!(
        "2035074e03520055005501540f0f00371f1f00172f2f00f73f3f00d77f7f0057",
        "0303440b070788bf070788bf0000ff5600ff0056ffff005700ffff57ff8000d6",
        "ff00ff57ffffff58ffffff58015480d503520154ff00ff57005580d503520055",
        "0101005301020052010400500108004c01100044020100520202005102030050",
        "0700004e08040049040a03440000005500000055000000550000005500000055",
        "01ff00ff0709460055084d0154064f0055005500550055064f00550a4bffffff",
    );

    fn hex_block(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn the_factory_block_passes_the_import_check() {
        block_is_sane(&FACTORY).expect("a factory block must be importable");
        block_is_sane(&hex_block(UNIT_BLOCK_HEX)).expect("a real unit's block must be importable");
    }

    #[test]
    fn the_import_check_refuses_a_block_with_no_left_click() {
        let mut block = hex_block(UNIT_BLOCK_HEX);
        for index in 0..BUTTON_COUNT {
            encode_button(&mut block, index, KEY_CLASS_DISABLE, 0x0000);
        }
        assert!(!has_left_click(&block));
        let err = block_is_sane(&block).unwrap_err();
        assert!(err.contains("左键"), "unexpected message: {err}");
    }

    #[test]
    fn the_import_check_refuses_a_block_that_is_not_a_configuration() {
        assert!(block_is_sane(&[0u8; CONFIG_END]).is_err());
        // A truncated block, and one with a key class outside the known range.
        let mut short = hex_block(UNIT_BLOCK_HEX);
        short.truncate(CONFIG_END - 4);
        assert!(block_is_sane(&short).is_err());
        let mut odd = hex_block(UNIT_BLOCK_HEX);
        encode_button(&mut odd, 1, 0x09, 0x0100);
        assert!(block_is_sane(&odd).is_err());
    }

    #[test]
    fn the_import_check_refuses_a_stage_index_out_of_range() {
        let mut block = hex_block(UNIT_BLOCK_HEX);
        write_pair(&mut block, ACTIVE_STAGE_ADDR as usize, 9);
        assert!(block_is_sane(&block).is_err());
    }
}
