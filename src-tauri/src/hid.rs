//! HID transport for the Metaphyuni receiver.
//!
//! Talks to the 17-byte command collection (usage page 0xFF02) directly through
//! `hidapi`, so no vendor DLL, no WebHID and no browser permission prompt is
//! involved. That is what makes the desktop build able to read *and* write the
//! configuration at all: WebView2 has no `navigator.hid`.
//!
//! Two behaviours matter and both came from hardware:
//!
//!  * A read must not accept a stale reply. Replies are matched by address and
//!    length, so a reply left over from the previous request for the SAME address
//!    matches too — that is what once made the DPI stage look frozen at its first
//!    value. Every request therefore drains the input queue first.
//!  * The device may echo a request it will not answer. An echo is byte-identical
//!    to the request, so it is only used when nothing better arrived.

use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::sleep;
use std::time::Duration;

use serde::Serialize;

use hidapi::{HidApi, HidDevice};

use crate::protocol;
use crate::protocol::{
    build_frame, cmd, CID_MID_LEN, CMD_USAGE_PAGE, MAX_PAYLOAD, REPORT_ID, is_supported_pid,
    STATUS_REPORT_ID, AUX_USAGE_PAGE, STATUS_USAGE_PAGE, VENDOR_ID, WRITE_COMMIT_ADDR, CMD_USAGE,
    DEVICE_TYPE_INDEX, TEN_BYTE_PAYLOAD, discharge_estimate, rate_to_code, DongleRgb,
};

/// Gap between two write frames; the device swallows frames sent faster.
const WRITE_GAP: Duration = Duration::from_millis(5);
/// Settle time after the commit frame before the flash is readable again.
///
/// The receiver goes into standby while it writes flash; measured, it needs about
/// two seconds before it answers reads again. Reading too early is not harmful,
/// it just returns nothing and costs a whole wake cycle.
const COMMIT_SETTLE: Duration = Duration::from_millis(2200);

/// Payload byte for the commit frame at [`WRITE_COMMIT_ADDR`].
///
/// A zero-length write is refused by this receiver (byte 2 comes back as `01`),
/// so the commit has to carry something. One `0x00` is the smallest thing the
/// device accepts, and it is accepted at 0x061E while the same frame with no
/// payload is not.
///
/// **What the frame is for, measured:** less than it looks. Staging records and
/// sending no commit at all, then letting the mouse sleep and wake, leaves the
/// change in place just the same (`--persist none`). So the receiver makes
/// writes durable without being asked — either it writes through to flash, or
/// it flushes on the way into standby — and this frame is a courtesy that
/// mirrors the vendor stack rather than the thing that saves the data. That is
/// also why a refused commit no longer fails a write.
const COMMIT_PAYLOAD: u8 = 0x00;

/// What command 0x04 reports. Mirrors the vendor `BatteryStatus` layout.
#[derive(Serialize, Clone, Copy, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Battery {
    /// Charge percentage as the device itself reports it, 0..100.
    ///
    /// The device stores charge in 5 % steps, so this number can sit still
    /// while the cell actually drains. It stays the fallback for every case
    /// the voltage curve cannot speak to: charging, or no voltage at all.
    pub level: u8,
    /// True while the cable supplies power.
    pub charging: bool,
    /// Raw cell voltage; ~3900 mV at half charge, ~4200 when full.
    pub millivolts: u16,
    /// Continuous estimate from the discharge voltage curve, in percent.
    ///
    /// `None` while charging, and when there is no usable voltage: charge
    /// voltage includes the charging effect, so turning it into a percentage
    /// would report a battery that is fuller than it is.
    pub estimate: Option<f32>,
}

/// What the receiver answered, including whether it took the frame.
///
/// The accept/reject flag used to be thrown away, which left a refused write
/// looking identical to a successful one: the device echoes the request either
/// way and only byte 2 differs. Everything that writes now reads it.
#[derive(Clone, Debug)]
pub struct Reply {
    /// Payload bytes, `frame[6..16]`.
    pub payload: Vec<u8>,
    /// True when byte 2 said the device refused this frame.
    pub rejected: bool,
}

/// The receiver's own ids, plus the device type it reports.
#[derive(Serialize, Clone, Copy, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub cid: u8,
    pub mid: u8,
    /// See [`DEVICE_TYPE_INDEX`]: read from one receiver, shown but not relied on.
    pub device_type: u8,
}

/// An open receiver.
pub struct Link {
    /// Kept alive for as long as the handle is used.
    _api: HidApi,
    /// The 17-byte command collection (usage page 0xFF02).
    dev: HidDevice,
    /// The 49-byte report collection (usage page 0xFF06, col07). The receiver
    /// delivers command replies here as well as on the command channel, and the
    /// reference implementation checks both — so does this one.
    aux: Option<HidDevice>,
    /// The status-snapshot collection (usage page 0xFF04, FEATURE report 0x06).
    /// A separate handle is required: a FEATURE request on the command interface
    /// answers `ERROR_INVALID_FUNCTION`, because that interface has no feature
    /// report to give.
    status: Option<HidDevice>,
    /// PID actually opened, so the UI can say which product this is.
    pub product_id: u16,
    /// USB bcdDevice of the opened receiver — its own firmware revision.
    pub release: u16,
}

/// Drop whatever is already queued, so a stale reply cannot answer this request.
///
/// 1 ms rather than 0: a zero-timeout read on Windows leaves the overlapped
/// request pending inside hidapi, which then swallows the *next* real reply.
fn drain(dev: &HidDevice) {
    let mut buf = [0u8; 64];
    for _ in 0..32 {
        match dev.read_timeout(&mut buf, 1) {
            Ok(n) if n > 0 => continue,
            _ => break,
        }
    }
}

/// Accept `frame` as the answer to a read of `addr`/`len`.
///
/// Returns the data when it is a real reply, records a possible echo and returns
/// `None` when the payload is byte-identical to the request (which happens
/// legitimately when the stored value really is `02 02 …`).
fn accept(
    frame: &[u8],
    addr: u16,
    len: usize,
    payload: &[u8],
    echo: &mut Option<Vec<u8>>,
) -> Option<Vec<u8>> {
    if frame.len() < 6 + len || frame[0] != REPORT_ID || frame[1] != cmd::READ {
        return None;
    }
    let got_addr = ((frame[3] as u16) << 8) | frame[4] as u16;
    if got_addr != addr || frame[5] as usize != len {
        return None;
    }
    let data = frame[6..6 + len].to_vec();
    if data == payload {
        *echo = Some(data);
        return None;
    }
    Some(data)
}

impl Link {
    /// Open the first receiver that exposes the command collection.
    pub fn connect() -> Result<Self, String> {
        let api = HidApi::new().map_err(|e| format!("could not initialise hidapi: {e}"))?;

        let mut preferred = None;
        let mut loose = None;
        let mut status_path = None;
        let mut aux_path = None;
        let mut fallback = None;
        for info in api.device_list() {
            if info.vendor_id() != VENDOR_ID || !is_supported_pid(info.product_id()) {
                continue;
            }
            let pid = info.product_id();
            // The USB device release number (bcdDevice). It is the only place
            // a receiver reports its own firmware: the vendor's protocol has
            // ReadVersion (0x12, the mouse) and GetSlaveVersion (0xb3, whatever
            // is on the far end of the link) but nothing that asks the receiver
            // about itself.
            let rel = info.release_number();
            // The vendor id alone proves nothing: every CompX mouse uses it,
            // and several of them expose a 0xFF02 collection that is not a
            // command channel at all. The reference implementation therefore
            // also demands usage 2 on that page, and so does this. The looser
            // matches are kept only as fallbacks — picking up an unrelated
            // 3554 device makes every command sent to it fail silently, which
            // is exactly how this used to look like "no receiver found".
            let page = info.usage_page() == CMD_USAGE_PAGE;
            let exact = page && info.usage() == CMD_USAGE;
            if fallback.is_none() {
                fallback = Some((info.path().to_owned(), pid, rel));
            }
            if exact && preferred.is_none() {
                preferred = Some((info.path().to_owned(), pid, rel));
            } else if page && loose.is_none() {
                loose = Some((info.path().to_owned(), pid, rel));
            }
            if info.usage_page() == STATUS_USAGE_PAGE && status_path.is_none() {
                status_path = Some(info.path().to_owned());
            }
            if info.usage_page() == AUX_USAGE_PAGE && aux_path.is_none() {
                aux_path = Some(info.path().to_owned());
            }
        }

        let (path, product_id, release) = preferred
            .or(loose)
            .or(fallback)
            .ok_or_else(|| "no Metaphyuni receiver found (VID 3554)".to_string())?;
        let dev = api
            .open_path(&path)
            .map_err(|e| format!("the receiver is present but could not be opened: {e}"))?;
        // Battery is optional: without the snapshot interface everything else
        // still works, so a failure here must not fail `connect`.
        let status = status_path.and_then(|p| api.open_path(&p).ok());
        let aux = aux_path.and_then(|p| api.open_path(&p).ok());

        let link = Link { _api: api, dev, aux, status, product_id, release };
        drain(&link.dev);
        Ok(link)
    }

    /// Send one command frame.
    ///
    /// `send_output_report` maps to `HidD_SetOutputReport`, NOT to `WriteFile`.
    /// That distinction is not cosmetic: with `WriteFile` the receiver answers the
    /// first request and then goes silent for every following one, while
    /// `HidD_SetOutputReport` keeps answering — the same call the working Python
    /// reference uses.
    fn send(&self, cmd: u8, addr: u16, payload: &[u8]) -> Result<(), String> {
        let frame = build_frame(cmd, addr, payload);
        self.dev
            .send_output_report(&frame)
            .map_err(|e| format!("writing command 0x{cmd:02x} failed: {e}"))
    }

    /// Read `len` bytes at `addr` in one frame. `None` when the device stays silent.
    ///
    /// IMPORTANT: one long blocking wait per attempt, never a loop of short ones.
    /// Measured on this receiver: a single 350 ms wait always gets the reply, while
    /// slicing the same window into 20 ms waits gets the *first* reply and then
    /// nothing, forever — hidapi keeps an overlapped read pending between calls and
    /// the pending request stops delivering. The reference probe waits 250–400 ms
    /// in one go for exactly this reason.
    pub fn read_block(&self, addr: u16, len: usize, tries: usize) -> Option<Vec<u8>> {
        if len == 0 || len > MAX_PAYLOAD {
            return None;
        }
        // `02 …` is the read opcode the vendor stack sends; only `len` matters to
        // the device, the rest of the payload is ignored.
        let mut payload = vec![0u8; len];
        payload[0] = 0x02;
        if len > 1 {
            payload[1] = 0x02;
        }

        for attempt in 0..tries.max(1) {
            if attempt > 0 {
                // Clear anything left queued so a retry cannot match a stale reply
                // for this same address and length.
                drain(&self.dev);
            }
            if self.send(cmd::READ, addr, &payload).is_err() {
                return None;
            }

            let mut echo: Option<Vec<u8>> = None;
            let mut buf = [0u8; 64];

            // The command channel answers almost immediately when the receiver is
            // awake, so the full window is only paid when it is not.
            if let Ok(n) = self.dev.read_timeout(&mut buf, 350) {
                if let Some(data) = accept(&buf[..n], addr, len, &payload, &mut echo) {
                    return Some(data);
                }
            }
            // The receiver also emits frames on the wide report channel (col07);
            // the reference implementation polls both.
            if let Some(aux) = &self.aux {
                if let Ok(n) = aux.read_timeout(&mut buf, 120) {
                    if let Some(data) = accept(&buf[..n], addr, len, &payload, &mut echo) {
                        return Some(data);
                    }
                }
            }
            if let Some(data) = echo {
                return Some(data);
            }
        }
        None
    }

    /// Read `[start, end)` — 10 bytes per frame, so the whole block costs 20 frames.
    pub fn read_range(&self, start: u16, end: u16) -> Result<Vec<u8>, String> {
        let total = end.saturating_sub(start) as usize;
        let mut out = vec![0u8; total];
        let mut addr = start;
        while addr < end {
            let len = ((end - addr) as usize).min(MAX_PAYLOAD);
            let block = self.read_block(addr, len, 3).ok_or_else(|| {
                format!(
                    "the receiver stopped answering at 0x{addr:04x} — click the mouse and try again"
                )
            })?;
            let at = (addr - start) as usize;
            out[at..at + len].copy_from_slice(&block);
            addr += len as u16;
        }
        Ok(out)
    }

    /// Battery state: percentage, charging flag and millivolts.
    ///
    /// This used to read byte 3 of FEATURE report 0x06, which is a **constant
    /// 0x64** — every mouse therefore reported exactly 100 %, always. The real
    /// value comes from command 0x04 (`BatteryLevel` in the vendor enum), whose
    /// reply carries the same layout the vendor DLL fills into
    /// `BatteryStatus { byte level; byte isCharging; ushort BatVoltage; }`:
    ///
    ///   request  08 04 00 00 00 00 …
    ///   reply    08 04 00 00 00 02  37 00  0f 40 …
    ///                               │     │      └─ 0x0F40 = 3904 mV
    ///                               │     └─ charging flag
    ///                               └─ 55 %
    ///
    /// The voltage is big-endian: read little-endian it would be 16399 mV, which
    /// no single-cell Li-ion could ever reach.
    pub fn battery(&self) -> Option<Battery> {
        let payload = self.command(cmd::BATTERY, 0x0000, &[], 400)?;
        if payload.len() < 4 {
            return None;
        }
        let charging = payload[1] != 0;
        let millivolts = ((payload[2] as u16) << 8) | payload[3] as u16;
        Some(Battery {
            level: payload[0].min(100),
            charging,
            millivolts,
            // Discharge voltage only. While charging, or with no voltage, the
            // device's own level is the honest answer.
            estimate: if charging {
                None
            } else {
                discharge_estimate(millivolts)
            },
        })
    }

    /// The receiver's own ids and device type, from command 0x01.
    ///
    /// Measured: `08 01 00 00 00 08  1d 02 00 00 1d 02 01 00` — CID and MID on
    /// a four-byte stride. The CID is what `DongleEnterPair` wants, so it has
    /// to be read before any pairing attempt rather than assumed.
    ///
    /// This used to return a bare `(cid, mid)` tuple, which meant callers that
    /// needed the type had another round trip and callers that did not had two
    /// unnamed bytes to get the wrong way round.
    pub fn identity(&self) -> Option<Identity> {
        // The vendor DLL fills the 8-byte payload slot for this command; a
        // zero-length frame gets a one-byte answer with nothing in it.
        let payload = self.command(cmd::DEVICE_INFO, 0x0000, &[0u8; CID_MID_LEN], 400)?;
        if payload.len() < CID_MID_LEN {
            return None;
        }
        // Layout of the answer: `[cid] 02 00 00 [mid] 02 ?? ??`. Only the two
        // ids matter, and they sit at 0 and 4.
        Some(Identity {
            cid: payload[0],
            mid: payload[4],
            device_type: payload.get(DEVICE_TYPE_INDEX).copied().unwrap_or(0),
        })
    }

    /// Which onboard profile is live, from command 0x0e (GetCurrentConfig).
    ///
    /// `None` means the device never answered, which the vendor driver treats
    /// as "this device has one profile" and greys its picker out. The app does
    /// the same rather than offering profiles that do not exist.
    pub fn current_config(&self) -> Option<u8> {
        let payload = self.command(cmd::CONFIG_GET, 0x0000, &[], 400)?;
        payload.first().copied()
    }

    /// Switch onboard profile (command 0x0f).
    ///
    /// One payload byte, the index. Verified closed loop against
    /// [`Link::current_config`] on a P1 Ultra 4K receiver: writing an index
    /// moves the reading to it. Anything outside the offered profiles is
    /// refused before it reaches the wire.
    pub fn set_current_config(&self, index: u8) -> Result<(), String> {
        if index as usize >= protocol::ONBOARD_PROFILES {
            return Err(format!(
                "配置档 {index} 超出范围：本设备最多 {} 组",
                protocol::ONBOARD_PROFILES
            ));
        }
        self.write_with_flag(cmd::CONFIG_SET, &[index], "配置档")
            .map(|_| ())
    }

    /// Is long range mode on, from command 0x17 (GetLongRangeMode).
    ///
    /// `None` when nothing answered, which is what happens on a device that
    /// does not have the feature — and on a wired mouse the driver never
    /// offers it either.
    pub fn long_range(&self) -> Option<bool> {
        let payload = self.command(cmd::LONG_RANGE_GET, 0x0000, &[], 400)?;
        payload.first().map(|v| *v > 0)
    }

    /// Turn long range mode on or off (command 0x16).
    ///
    /// Ten payload bytes, `[enable] 0 … 0`: shorter frames are answered with
    /// the reject flag rather than acted on. Verified closed loop — the write
    /// reports accepted and [`Link::long_range`] then reports the new value.
    pub fn set_long_range(&self, on: bool) -> Result<(), String> {
        let mut payload = [0u8; TEN_BYTE_PAYLOAD];
        payload[0] = on as u8;
        self.write_with_flag(cmd::LONG_RANGE_SET, &payload, "长距离模式")
            .map(|_| ())
    }

    /// Erase every user setting in the device's flash (command 0x09).
    ///
    /// This is what the driver's own reset button does, behind its own confirm
    /// dialog. DPI stages, colours, debounce, key mapping and the receiver
    /// light all go back to factory defaults, and there is no undo anywhere in
    /// the protocol — so the confirmation lives in the caller and this function
    /// is deliberately reachable from exactly one command handler.
    ///
    /// Left untested on real hardware on purpose: verification would mean
    /// wiping the settings on the only mouse available.
    pub fn factory_reset(&self) -> Result<(), String> {
        self.write_with_flag(cmd::CLEAR_SETTING, &[], "恢复出厂设置")
            .map(|_| ())
    }

    /// Send a write and require the device to say it took it.
    ///
    /// Every vendor write answers with the request echoed and byte 2 carrying
    /// accept or reject; without checking it a refused write is
    /// indistinguishable from one that landed. Payloads shorter than one frame
    /// are fine — the frame is padded, and only `len` is read back.
    fn write_with_flag(
        &self,
        code: u8,
        payload: &[u8],
        what: &str,
    ) -> Result<Reply, String> {
        match self.exchange(code, 0x0000, payload, 600) {
            None => Err(format!("{what}：设备没有回应，设置可能未生效")),
            Some(reply) if reply.rejected => Err(format!("{what}：设备拒绝了这条指令（未生效）")),
            Some(reply) => Ok(reply),
        }
    }

    /// Pairing state from command 0x06 (GetPairState).
    ///
    /// The vendor's pairing dialog treats 1 as "negotiating" and 2 as "bonded";
    /// 0 is the idle answer on a receiver that is not pairing.
    pub fn pair_state(&self) -> Option<u8> {
        let payload = self.command(cmd::PAIR_STATE, 0x0000, &[], 400)?;
        payload.first().copied()
    }

    // The vendor's own pairing-status read does not exist as a separate code:
    // 0x09 is ClearSetting, the factory reset, so this is where a second
    // "status" command used to sit. Removed rather than renamed — a read-only
    // helper that actually erases flash is a trap waiting for the next caller.

    /// The whole 0x06 answer: `[state][seconds_left]`.
    pub fn pair_state_raw(&self) -> Option<Vec<u8>> {
        self.command(cmd::PAIR_STATE, 0x0000, &[], 400)
    }

    /// Whether a mouse is on the other end of the link, from command 0x03
    /// (ReadOnLine in the vendor DLL).
    ///
    /// Much cheaper than reading a config block: the receiver answers it
    /// itself, without forwarding anything to the mouse, so it costs one short
    /// round trip instead of a full wake. It answers `01` when a mouse is
    /// there and `00` when there is not.
    pub fn mouse_online(&self) -> bool {
        match self.command(cmd::POLL, 0x0000, &[], 400) {
            Some(payload) => payload.first().copied().unwrap_or(0) != 0,
            None => false,
        }
    }

    /// Read the 4K receiver's light (0x15).
    ///
    /// Only the 4K receiver has one; a 1K receiver or a wired mouse simply
    /// never answers, which is why this returns `None` rather than a default.
    pub fn read_dongle_rgb(&self) -> Option<DongleRgb> {
        let payload = self.command(cmd::DONGLE_RGB_GET, 0x0000, &[], 500)?;
        DongleRgb::from_payload(&payload)
    }

    /// Write the 4K receiver's light (0x14).
    ///
    /// Two bytes shorter to describe than to justify: the vendor's own
    /// `CS_UsbServer_Set4KDongleRGB` sends this same ten-byte payload with no
    /// commit frame behind it, and the receiver stores it immediately.
    ///
    /// The accept flag is checked rather than trusted. It matters here because
    /// the reply arrives whether or not the write landed, and anything other
    /// than ten bytes is refused outright.
    pub fn set_dongle_rgb(&self, rgb: DongleRgb) -> Result<(), String> {
        self.write_with_flag(cmd::DONGLE_RGB_SET, &rgb.to_payload(), "接收器灯效")
            .map(|_| ())
    }

    /// Put the receiver into pairing mode (command 0x05) so a mouse can bind to it.
    ///
    /// The payload is `[cid][mid]` — two bytes, as the vendor DLL builds it.
    /// The ids come from `identity()`; when the receiver never answered that,
    /// the old one-byte `[cid]` frame was accepted but did nothing.
    ///
    /// This is the one command here that changes the bond: while the receiver is
    /// in pairing mode the current link may drop until something pairs again.
    /// It is therefore only ever called from an explicit user action.
    pub fn enter_pair(&self, cid: u8, mid: u8) -> Result<(), String> {
        self.send(cmd::ENTER_PAIR, 0x0000, &[cid, mid])
    }

    /// Send an arbitrary command and return the payload of the first reply that
    /// carries the same command code.
    ///
    /// Used for the version queries (`0x12` mouse, `0xb3` slave), which answer
    /// with a small payload rather than a firmware block. Anything that *writes*
    /// should use [`Link::exchange`] instead so it can see the accept flag.
    pub fn command(&self, code: u8, addr: u16, payload: &[u8], wait_ms: i32) -> Option<Vec<u8>> {
        self.exchange(code, addr, payload, wait_ms).map(|r| r.payload)
    }

    /// As [`Link::command`], but keeps the accept/reject flag as well.
    ///
    /// The two differ only in one bit of byte 2, and throwing that bit away is
    /// what let a refused write report success. Byte 2 is captured whole so the
    /// caller can tell.
    pub fn exchange(&self, code: u8, addr: u16, payload: &[u8], wait_ms: i32) -> Option<Reply> {
        // Leftover frames from the previous exchange are the usual reason a
        // query "fails": the first thing read is the last reply, not this one.
        drain(&self.dev);
        if self.send(code, addr, payload).is_err() {
            return None;
        }
        let deadline = std::time::Instant::now() + Duration::from_millis(wait_ms.max(0) as u64);
        let mut buf = [0u8; 64];
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return None;
            }
            let slice = left.as_millis().min(150) as i32;
            match self.dev.read_timeout(&mut buf, slice.max(20)) {
                Ok(n) if n >= 7 && buf[0] == REPORT_ID && buf[1] == code => {
                    // Byte 5 mirrors the length we asked for, not the length of
                    // the answer — trusting it truncates a 4-byte battery reply
                    // to 2 bytes. Keep everything that actually arrived.
                    let end = n.min(6 + MAX_PAYLOAD);
                    let frame = &buf[..n];
                    return Some(Reply {
                        payload: buf[6..end].to_vec(),
                        rejected: crate::protocol::rejected(frame),
                    });
                }
                // Anything else (echo, status frame, timeout) is skipped; only a
                // frame carrying our command code counts as an answer.
                _ => {}
            }
        }
    }

    /// Wake the receiver and confirm reads answer.
    ///
    /// The mouse has a standby mode and the receiver stops answering commands
    /// while it is in it — measured: a read can take 30 s to come back after the
    /// mouse has been idle, and the same reference probe needs repeated
    /// handshake+read pairs, not one. So the sequence here is the one that
    /// empirically works:
    ///
    ///   handshake(4 random bytes + 1d 02 01 00) -> 1-byte read probe -> pause
    ///
    /// A handshake alone does nothing, and the PREPARE/COMMIT burst must NOT be
    /// used: those are write commands and they close reads instead of opening them.
    pub fn ensure_reads(&self, tries: usize) -> bool {
        // Fast path: already awake (the common case while the mouse is in use).
        if self.read_block(0x0000, 1, 1).is_some() {
            return true;
        }
        for _ in 0..tries.max(1) {
            // The tail is what the device itself answers a handshake with; the
            // vendor app replays it, and it is what the reference probe sends.
            let nonce = [
                rand_byte(),
                rand_byte(),
                rand_byte(),
                rand_byte(),
                0x1d,
                0x02,
                0x01,
                0x00,
            ];
            let _ = self.send(cmd::HANDSHAKE, 0, &nonce);
            sleep(Duration::from_millis(50));
            if self.read_block(0x0000, 1, 2).is_some() {
                // Let the commit/standby timer settle before the real read.
                sleep(Duration::from_millis(250));
                return true;
            }
            sleep(Duration::from_millis(700));
        }
        false
    }

    /// Send one arbitrary frame, then report every frame that arrives on either
    /// channel for `listen_ms`.
    ///
    /// `--rawcmd` exists because guessing at a reply format from the typed path
    /// wastes round trips: the battery answer is not a config read and does not
    /// fit `accept`. Seeing the raw bytes settles the layout instead.
    pub fn raw_exchange(&self, code: u8, addr: u16, payload: &[u8], listen_ms: i32) -> Vec<String> {
        let mut out = Vec::new();
        drain(&self.dev);
        if let Some(aux) = &self.aux {
            drain(aux);
        }

        let frame = build_frame(code, addr, payload);
        out.push(format!("tx: {}", crate::hex(&frame)));
        match self.send(code, addr, payload) {
            Ok(()) => out.push("send: ok".to_string()),
            Err(e) => {
                out.push(format!("send: FAILED {e}"));
                return out;
            }
        }

        let start = std::time::Instant::now();
        let budget = listen_ms.max(0) as u64;
        let mut seen = 0usize;
        let mut buf = [0u8; 64];
        while start.elapsed() < Duration::from_millis(budget) && seen < 24 {
            let elapsed = start.elapsed().as_millis();
            match self.dev.read_timeout(&mut buf, 200) {
                Ok(n) if n > 0 => {
                    seen += 1;
                    out.push(format!("cmd[+{elapsed:>4} ms] {n}B {}", crate::hex(&buf[..n.min(24)])));
                }
                _ => {}
            }
            if let Some(aux) = &self.aux {
                match aux.read_timeout(&mut buf, 20) {
                    Ok(n) if n > 0 => {
                        seen += 1;
                        out.push(format!("aux[+{elapsed:>4} ms] {n}B {}", crate::hex(&buf[..n.min(24)])));
                    }
                    _ => {}
                }
            }
        }
        if seen == 0 {
            out.push("nothing received on either channel".to_string());
        }

        match &self.status {
            None => out.push("feature 0x06: no snapshot interface".to_string()),
            Some(dev) => {
                let mut feat = [0u8; 9];
                feat[0] = STATUS_REPORT_ID;
                match dev.get_feature_report(&mut feat) {
                    Ok(n) => out.push(format!("feature 0x06: {n}B {}", crate::hex(&feat[..n.min(9)]))),
                    Err(e) => out.push(format!("feature 0x06: error {e}")),
                }
            }
        }
        out
    }

    /// Set the report rate, e.g. 1000 Hz.
    ///
    /// This uses the two-byte direct write the FerrisPulse adapter verified on
    /// a real 4K receiver (`08 07 00 00 00 02 <code> <0x55-code> …`), which
    /// takes effect on its own — it is NOT the four-byte record path the
    /// configuration block uses, and it must not be followed by the commit
    /// frame. 0x0000 holds `[code][0x55-code]`, so the value and its mirror
    /// travel together.
    ///
    /// Refuses anything outside the measured table: 8000 Hz has never been
    /// seen on these devices, and the codes are not an extrapolation.
    pub fn set_report_rate(&self, hz: u32) -> Result<(), String> {
        let code = rate_to_code(hz).ok_or_else(|| {
            format!("{hz} Hz 没有经过实测，本应用不会写入：只开放已验证的档位")
        })?;
        let mirror = 0x55u8.wrapping_sub(code);
        drain(&self.dev);
        self.send(cmd::WRITE, 0x0000, &[code, mirror])?;
        // The receiver pauses while it stores the value; the read-back below
        // is the only proof it took, so give it the same window a read gets.
        sleep(Duration::from_millis(120));
        match self.read_block(0x0000, 2, 4) {
            Some(v) if v[0] == code => Ok(()),
            Some(v) => Err(format!(
                "回报率已发送，但设备回读为 0x{:02x}（期望 0x{code:02x}）",
                v[0]
            )),
            None => Err("回报率已发送，但设备没有回应回读".to_string()),
        }
    }

    /// Write the configuration back: four bytes per frame, then one commit frame.
    ///
    /// Only the records that differ are sent, but every apply rewrites the block
    /// from scratch — that is what the vendor writer does, and it avoids leaving
    /// a record half-updated. Returns the number of data frames sent.
    ///
    /// Every frame is answered and the answer is checked. Two things used to be
    /// wrong here at once, and together they made a failed apply look like a
    /// successful one:
    ///
    /// 1. `send` is fire-and-forget, so a rejected frame was never noticed.
    /// 2. The commit frame carried an EMPTY payload. Measured on this receiver,
    ///    a zero-length write is refused at every address — `flag=01` for
    ///    0x0600, 0x061C, 0x061E and 0x0620 alike — while the same frame with
    ///    one payload byte is accepted. The staged records therefore never
    ///    reached flash: reads answered from RAM and looked correct until the
    ///    mouse went through standby and reloaded.
    pub fn write_config(&self, next: &[u8], previous: &[u8]) -> Result<usize, String> {
        self.write_chunks(0, next, Some(previous))
    }

    /// Write four-byte records anywhere in flash, then commit once.
    ///
    /// [`Link::write_config`] only covers the 192-byte configuration block,
    /// and not everything worth writing lives there: a button's keyboard
    /// combination sits in its own 32-byte slot at 0x0100 + 0x20 × index. Same
    /// frames, same one-byte commit, different address range.
    ///
    /// `previous` is what is believed to be there now; chunks that already
    /// match are skipped, which for a 32-byte combination usually leaves only
    /// the first two or three frames to send.
    pub fn write_chunks(
        &self,
        addr: u16,
        next: &[u8],
        previous: Option<&[u8]>,
    ) -> Result<usize, String> {
        let mut sent = 0usize;
        let mut off = 0usize;
        while off + 4 <= next.len() {
            let chunk = &next[off..off + 4];
            let already = previous
                .map(|old| old.len() >= off + 4 && old[off..off + 4] == *chunk)
                .unwrap_or(false);
            if !already {
                let at = addr + off as u16;
                match self.exchange(cmd::WRITE, at, chunk, 600) {
                    None => {
                        return Err(format!(
                            "写入 0x{at:04x} 后设备没有回应，后面的内容没有继续写入"
                        ))
                    }
                    Some(r) if r.rejected => {
                        return Err(format!("设备拒绝了 0x{at:04x} 这条记录（未生效）"))
                    }
                    Some(_) => {}
                }
                sleep(WRITE_GAP);
                sent += 1;
            }
            off += 4;
        }
        if sent == 0 {
            return Ok(0);
        }
        // The commit is sent the way the vendor does it, but it is not what
        // makes the records durable: staging them and sending nothing survives
        // a sleep cycle too (measured, `--persist none`). A receiver that
        // refuses it has still taken the data, so this no longer fails.
        match self.exchange(cmd::WRITE, WRITE_COMMIT_ADDR, &[COMMIT_PAYLOAD], 600) {
            Some(_) => sleep(COMMIT_SETTLE),
            None => sleep(Duration::from_millis(300)),
        }
        Ok(sent)
    }
}

/// Non-crypto pseudo-random byte for the handshake nonce.
fn rand_byte() -> u8 {
    static STATE: AtomicU32 = AtomicU32::new(0);
    let mut v = STATE.load(Ordering::Relaxed);
    if v == 0 {
        v = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0x9e37_79b9)
            | 1;
    }
    // xorshift32
    v ^= v << 13;
    v ^= v >> 17;
    v ^= v << 5;
    STATE.store(v, Ordering::Relaxed);
    (v & 0xff) as u8
}
