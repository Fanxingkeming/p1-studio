/**
 * Metaphyuni玄熊猫P1管理器 — UI controller.
 *
 * Everything here talks to the Rust side through `invoke`; there is no WebHID and
 * no polling of the browser. Two rules shape the code:
 *
 *  1. Never show a value the device did not report. Missing data renders as "—".
 *  2. Never claim a write succeeded. `apply` returns the snapshot the device gave
 *     back, and any mismatch surfaces as an error the user can read.
 */

const tauri = window.__TAURI__?.core;
const invoke = (cmd, args) => tauri.invoke(cmd, args);
const tauriEvent = window.__TAURI__?.event;

const $ = (id) => document.getElementById(id);

const PAGE_META = {
  overview: ['概览', '设备状态与快捷操作'],
  dpi: ['DPI', '档位数值与当前档位'],
  buttons: ['按键', '六个物理键的功能分配'],
  performance: ['性能', '防抖、修正与传感器选项'],
  power: ['省电', '自动休眠与传感器模式'],
  connection: ['连接', '当前链路、蓝牙状态与 2.4G 配对'],
  firmware: ['固件', '当前版本、固件包校验与升级通道'],
  diagnostics: ['诊断', '原始配置块与运行日志'],
};

// Report-rate codes, in the order the device stores them.
//
// These six came from the FerrisPulse Metapanda adapter, which captured them
// from the official driver and then confirmed each one on a real 4K receiver
// by writing it and reading it back. They are not an encoding of the Hz
// number, so 8000 Hz cannot be derived from them — an unmeasured code stays
// "unknown" here rather than being guessed at from 4000.
const REPORT_RATE = {
  8: '125 Hz',
  4: '250 Hz',
  2: '500 Hz',
  1: '1000 Hz',
  16: '2000 Hz',
  32: '4000 Hz',
};

const state = {
  info: null,
  caps: null,
  snapshot: null,
  draft: {},
  busy: false,
  online: false,
  /** Consecutive polls that found nothing; drives the poll back-off. */
  missed: 0,
  connection: null,
  lastRaw: '',
  /** True while the pairing window is open; suspends the normal 600 ms poll. */
  pairing: false,
};

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ── theme modes ──────────────────────────────────────────────────────────
// Three modes: 跟随系统 / 浅色 / 深色. The Rust settings file is the source of
// truth (it survives a cleared WebView profile); localStorage is only the
// instant cache the inline <head> script reads before first paint.

const THEME_KEY = 'theme.mode';
const THEME_MODES = ['system', 'light', 'dark'];
const themeMedia = window.matchMedia('(prefers-color-scheme: dark)');

/** `system` resolves through the OS; the other two are literal. */
function resolvedTheme(mode) {
  const dark = mode === 'dark' || (mode !== 'light' && themeMedia.matches);
  return dark ? 'dark' : 'light';
}

function applyTheme(mode) {
  document.documentElement.dataset.theme = resolvedTheme(mode);
  for (const btn of document.querySelectorAll('.theme-switch button')) {
    const on = btn.dataset.mode === mode;
    btn.classList.toggle('active', on);
    btn.setAttribute('aria-checked', String(on));
  }
}

async function setTheme(mode) {
  if (!THEME_MODES.includes(mode)) return;
  try { localStorage.setItem(THEME_KEY, mode); } catch {}
  applyTheme(mode);
  // Persisting is best-effort: the UI must switch even if the settings file
  // cannot be written (read-only dir, etc.).
  try {
    await invoke('save_settings', { theme: mode });
  } catch (e) {
    logLine(`主题设置保存失败：${e}`);
  }
}

async function initTheme() {
  // The settings file wins; localStorage fills in when Rust is not there
  // (page opened outside the app) or the file has no theme yet.
  let mode = null;
  if (tauri) {
    try {
      const s = await invoke('load_settings');
      if (s && THEME_MODES.includes(s.theme)) mode = s.theme;
    } catch {}
  }
  if (!mode) {
    try { mode = localStorage.getItem(THEME_KEY); } catch {}
  }
  applyTheme(THEME_MODES.includes(mode) ? mode : 'system');

  // Only the auto mode reacts to OS switches — an explicit choice should not
  // be overridden by the user changing their Windows colour scheme.
  themeMedia.addEventListener('change', () => {
    let current = null;
    try { current = localStorage.getItem(THEME_KEY); } catch {}
    if ((current || 'system') === 'system') applyTheme('system');
  });

  for (const btn of document.querySelectorAll('.theme-switch button')) {
    btn.addEventListener('click', () => setTheme(btn.dataset.mode));
  }
}

// ── small helpers ────────────────────────────────────────────────────────

function toast(kind, message) {
  const el = $('toast');
  el.textContent = message;
  el.className = `toast ${kind}`;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => el.classList.add('hidden'), kind === 'err' ? 9000 : 3200);
}

function logLine(text) {
  const el = $('log');
  const stamp = new Date().toLocaleTimeString('zh-CN', { hour12: false });
  el.textContent = `${stamp}  ${text}\n${el.textContent}`.slice(0, 6000);
}

function banner(text, isError = false, action = null) {
  const el = $('banner');
  const textEl = $('banner-text');
  const actionEl = $('banner-action');
  if (!text) {
    el.classList.add('hidden');
    actionEl.classList.add('hidden');
    return;
  }
  textEl.textContent = text;
  el.className = `banner${isError ? ' err' : ''}`;
  if (action) {
    actionEl.textContent = action.label;
    actionEl.onclick = action.run;
    actionEl.classList.remove('hidden');
  } else {
    actionEl.classList.add('hidden');
  }
}

const WAKE_HINT =
  '接收器没有回应：可能是鼠标在待机（动一下就好），也可能它已经切到蓝牙档 —— ' +
  '蓝牙档下本软件本来就收不到数据，属正常。';

/** Wake the receiver, then reload everything. */
async function wakeAndReload() {
  banner('正在唤醒设备，最多需要十几秒…');
  logLine('主动唤醒…');
  const ok = await invoke('wake');
  logLine(ok ? '唤醒成功' : '唤醒失败：设备仍无响应');
  if (!ok) {
    banner('仍然没有响应。请按一下鼠标按键再试，或重新插拔接收器。', true, {
      label: '重试',
      run: wakeAndReload,
    });
    return;
  }
  banner('正在读取设备…');
  await loadDevice();
  await loadConfig();
}

const sleepLabel = (stored, unitSeconds = 10) => {
  const seconds = stored * unitSeconds;
  return seconds < 60 ? `${seconds} 秒` : `${Math.round(seconds / 60)} 分钟`;
};

const rateLabel = (byte) => {
  if (byte === null || byte === undefined) return '—';
  const known = REPORT_RATE[byte];
  return known ? known : `未知 (0x${byte.toString(16).padStart(2, '0')})`;
};

// ── draft bookkeeping ───────────────────────────────────────────────────

const EDITABLE = ['dpiStages', 'stageCount', 'activeStage', 'debounce', 'sleepTimeout', 'sensorMode', 'motionSync', 'rippleControl', 'angleSnapping', 'lod', 'dpiLightMode', 'dpiLightBrightness', 'dpiLightSpeed'];
/// How many DPI stages are stored on the device; the record count at 0x0080
/// decides how many of them the DPI button actually cycles through.
const MAX_STAGES = 8;

/**
 * Adopt a freshly read snapshot.
 *
 * The DPI indicator light arrives nested (`dpiLight: {mode, brightness,
 * speed}`) because that is how the block decodes, but the draft and the patch
 * both speak flat keys — flattening once, here, keeps `pending()` a plain
 * key-by-key compare instead of a mix of the two shapes.
 */
function adoptSnapshot(snapshot) {
  state.snapshot = {
    ...snapshot,
    dpiLightMode: snapshot.dpiLight?.mode ?? 0,
    dpiLightBrightness: snapshot.dpiLight?.brightness ?? 5,
    dpiLightSpeed: snapshot.dpiLight?.speed ?? 3,
  };
  // Pending key bindings belong to the snapshot they were chosen against. Once
  // the device reports a different one — a profile switch, a refresh, a write
  // from elsewhere — those choices no longer describe anything, and applying
  // them would stamp the old profile's intent onto the new one.
  state.keyDraft = {};
}

function pending() {
  const out = {};
  for (const key of EDITABLE) {
    if (state.draft[key] === undefined) continue;
    if (JSON.stringify(state.draft[key]) !== JSON.stringify(state.snapshot?.[key])) out[key] = state.draft[key];
  }
  return out;
}

let lastPendingKeys = '';

function refreshApplyButton() {
  const keys = Object.keys(pending());
  $('btn-apply').disabled = state.busy || !state.online || keys.length === 0;
  $('btn-apply').textContent = keys.length ? `应用更改 (${keys.length})` : '应用更改';
  // Logged only on change: a pending key nobody edited is a bug, and this is the
  // only place it would show up outside a dev console.
  const sig = keys.join(',');
  if (sig !== lastPendingKeys) {
    lastPendingKeys = sig;
    if (sig) {
      logLine(`待应用：${sig} → ${JSON.stringify(pending())}`);
    }
  }
}

function edit(key, value) {
  state.draft[key] = value;
  // DPI stages and the active index travel together, exactly like the vendor app.
  if (key === 'dpiStages' || key === 'activeStage') {
    state.draft.dpiStages = effectiveStages();
    state.draft.activeStage = effectiveStage();
  }
  refreshApplyButton();
}

const effectiveStages = () => state.draft.dpiStages ?? state.snapshot?.dpiStages ?? [];
const effectiveStage = () => state.draft.activeStage ?? state.snapshot?.activeStage ?? 0;
const effectiveCount = () =>
  state.draft.stageCount ?? state.snapshot?.stageCount ?? effectiveStages().length;

/**
 * Grow or shrink the live stage list, filling new slots from a ladder.
 *
 * The eight records always exist in flash; this only changes how many of them
 * the mouse cycles through. New slots get `last * 2` like the vendor presets
 * (400/800/1600/3200 …) rather than a duplicate, so stepping the count up and
 * pressing the DPI button lands on a sensitivity that is actually different.
 */
function resizeStages(n) {
  const count = Math.min(MAX_STAGES, Math.max(1, n));
  const stages = [...effectiveStages()];
  while (stages.length < count) {
    const last = stages[stages.length - 1] ?? 400;
    stages.push(Math.min(26000, Math.max(50, Math.round((last * 2) / 50) * 50)));
  }
  stages.length = count;
  state.draft.stageCount = count;
  state.draft.dpiStages = stages;
  state.draft.activeStage = Math.min(effectiveStage(), count - 1);
  return count;
}

// ── device art ──────────────────────────────────────────────────────────
// The overview card shows the real product render instead of generic line
// art. Images come from assets/devices.json; a mouse that is not in the
// manifest keeps the built-in SVG.
let deviceImages = null;

async function loadDeviceImages() {
  if (deviceImages) return deviceImages;
  try {
    deviceImages = await (await fetch('assets/devices.json')).json();
  } catch {
    deviceImages = { devices: [] };
  }
  return deviceImages;
}

/** Match the reported device name against the manifest (case-insensitive). */
function pickDeviceImage(name) {
  const hay = (name || '').toLowerCase();
  for (const entry of deviceImages?.devices ?? []) {
    if ((entry.match || []).some((key) => hay.includes(String(key).toLowerCase()))) {
      return entry;
    }
  }
  return null;
}

async function applyDeviceArt(name) {
  const img = $('device-img');
  const svg = $('device-svg');
  const entry = pickDeviceImage(name);
  if (!entry) {
    img.classList.add('hidden');
    svg.classList.remove('hidden');
    return;
  }
  img.src = entry.image;
  img.classList.remove('hidden');
  svg.classList.add('hidden');
}

// ── data flow ───────────────────────────────────────────────────────────

async function loadDevice() {
  try {
    const info = await invoke('device_info');
    state.info = info;
    state.online = true;
    // The rail header names whatever is actually plugged in. It used to be a
    // hard-coded "玄派 · P1 Ultra 4K" in the HTML, so a P1 Pro Max receiver
    // never changed it.
    const full = `${info.brand} · ${info.name}`;
    const rail = $('brand-device');
    rail.textContent = full;
    rail.title = `${full} · ${info.mode} · PID 0x${info.productId.toString(16).toUpperCase()} · 回报率上限 ${info.maxRateHz} Hz`;
    $('dev-name').textContent = full;
    $('dev-mode').textContent = info.mode;
    $('m-fw-mouse').textContent = info.firmwareMouse ?? '—';
    $('m-fw-dongle').textContent = info.firmwareDongle ?? '—';
    $('conn-dot').className = 'dot on';
    $('conn-text').textContent = info.receiver ? `${info.name} 接收器已连接` : '已连接';
    renderFirmwareInfo(info);
    renderDongleRgb(info);
    renderLongRange(info);
    renderProfiles(info);
    await applyDeviceArt(info.name);
    logLine(`已连接 ${info.name}（PID 0x${info.productId.toString(16)}）`);
  } catch (e) {
    await markOffline(String(e));
  }
}

/** The firmware page mirrors the versions plus the ids a firmware image is checked against. */
function renderFirmwareInfo(info) {
  $('fw-mouse').textContent = info.firmwareMouse ?? '—';
  $('fw-dongle').textContent = info.firmwareDongle ?? '—';
  $('fw-mcu-mouse').textContent = info.mcuMouse || '—';
  $('fw-mcu-dongle').textContent = info.mcuDongle || '—';
  $('fw-ids').textContent = info.cid != null ? `${info.cid} / ${info.mid}` : '—';
}

async function loadConfig(quick = false) {
  try {
    const snapshot = await invoke('read_all', { quick });
    adoptSnapshot(snapshot);
    state.draft = {};
    renderAll();
    banner('');
    logLine(`读取配置成功：${snapshot.dpiStages.length} 档 DPI`);
  } catch (e) {
    const message = `${e}`;
    logLine(`读取配置失败：${message}`);
    banner(/待机|没有响应|没有应答|停在下|stopped|asleep|not answer/i.test(message) ? WAKE_HINT : message, true, {
      label: '唤醒设备',
      run: wakeAndReload,
    });
  }
}

async function loadConnection() {
  await loadBluetooth();
  try {
    const conn = await invoke('connection');
    state.connection = conn;
    renderConnection();
  } catch (e) {
    state.connection = null;
    // A silent receiver is the normal standby state, so this is not an error
    // banner — the badge just says it cannot see the mouse right now.
    $('conn-badge').textContent = '未连接';
    $('conn-badge').className = 'link-badge off';
    ['kv-link', 'kv-online', 'kv-cid', 'kv-mid', 'kv-pair'].forEach((id) => {
      $(id).textContent = '—';
    });
    $('kv-pid').textContent = state.info
      ? `0x${state.info.productId.toString(16)}`
      : '—';
    $('conn-hint').textContent =
      '收不到数据：鼠标可能正在待机（动一下就好），也可能已经切到蓝牙档 —— 蓝牙档下接收器不参与通信，本软件收不到数据属正常。';
    logLine(`读取连接状态失败：${e}`);
  }
}

function renderConnection() {
  const c = state.connection;
  if (!c) return;
  const label = { wired: '有线 · USB', wireless: '2.4G 无线', unknown: '未知' }[c.link] || '未知';
  $('kv-link').textContent = label;
  $('kv-online').textContent = c.online ? '在线' : '无响应';
  $('kv-cid').textContent = c.cid === null || c.cid === undefined ? '—' : `0x${c.cid.toString(16)}`;
  $('kv-mid').textContent = c.mid === null || c.mid === undefined ? '—' : `0x${c.mid.toString(16)}`;
  $('kv-pair').textContent = PAIR_LABEL[c.pairState] ?? `状态 ${c.pairState}`;
  $('kv-pid').textContent = `0x${c.productId.toString(16)}`;

  const badge = $('conn-badge');
  badge.textContent = c.online ? `${label} · 已连接` : `${label} · 鼠标无响应`;
  badge.className = `link-badge ${c.online ? 'on' : 'off'}`;

  // A silent receiver has two very different explanations and only the user
  // knows which: the mouse is asleep, or it sits on the Bluetooth channel where
  // the dongle is out of the loop entirely. Name both instead of guessing.
  $('conn-hint').textContent = c.online
    ? ''
    : '收不到数据：要么是鼠标在待机（动一下就好），要么是它已经切到蓝牙档 —— 蓝牙档下本软件本来就收不到数据，属正常。';
}

/**
 * Bluetooth mode lives outside the 2.4 GHz receiver entirely: the mouse shows
 * up as its own HID-over-GATT device, so this scan is independent of whether
 * the dongle answers. It is the only way to see the mouse while the bottom
 * switch sits on the Bluetooth position — and the battery service is the one
 * reading available there.
 *
 * It walks the radio, so it is not cheap: cache it for a while and let the
 * polling loop reuse the result instead of rescanning on every miss.
 */
const BLE_SCAN_TTL = 20000;
async function bluetoothSnapshot(force = false) {
  const now = Date.now();
  if (!force && state.bluetooth && now - (state.bleAt ?? 0) < BLE_SCAN_TTL) {
    return state.bluetooth;
  }
  const devices = await invoke('bluetooth');
  state.bluetooth = devices ?? [];
  state.bleAt = now;
  return state.bluetooth;
}

async function loadBluetooth(force = true) {
  const badge = $('ble-badge');
  badge.textContent = '检测中…';
  badge.className = 'link-badge off';
  try {
    // `bluetoothSnapshot` already caches what it found; this only renders it.
    await bluetoothSnapshot(force);
    renderBluetooth();
  } catch (e) {
    state.bluetooth = [];
    $('ble-note').textContent = `蓝牙检测失败：${e}`;
    for (const id of ['ble-name', 'ble-addr', 'ble-connected', 'ble-battery', 'ble-services', 'ble-vendor']) {
      $(id).textContent = '—';
    }
    badge.textContent = '检测失败';
    logLine(`蓝牙检测失败：${e}`);
  }
}

function renderBluetooth() {
  const list = state.bluetooth ?? [];
  const badge = $('ble-badge');
  if (list.length === 0) {
    badge.textContent = '未发现蓝牙设备';
    badge.className = 'link-badge off';
    for (const id of ['ble-name', 'ble-addr', 'ble-connected', 'ble-battery', 'ble-services', 'ble-vendor']) {
      $(id).textContent = '—';
    }
    // Two honest reasons: the switch is not on Bluetooth, or the mouse was
    // never paired with this PC. Say both instead of blaming the app.
    $('ble-note').textContent =
      '没有发现玄派蓝牙设备：要么鼠标底部位没拨到蓝牙档，要么这台电脑还没和它配对过（配对步骤见下方）。';
    return;
  }
  const d = list[0];
  $('ble-name').textContent = d.name || '—';
  $('ble-addr').textContent = d.address || '—';
  $('ble-connected').textContent = d.connected ? '已连接' : '未连接';
  $('ble-battery').textContent = d.battery == null ? '—' : `${d.battery}%`;
  $('ble-services').textContent = d.services.length ? d.services.join(' · ') : '—';
  $('ble-vendor').textContent = d.vendorServices.length ? d.vendorServices.join(' · ') : '无';
  badge.textContent = d.connected ? `蓝牙已连接 · ${d.name}` : `蓝牙未连接 · ${d.name}`;
  badge.className = `link-badge ${d.connected ? 'on' : 'off'}`;
  $('ble-note').textContent = d.error
    ? `读取 GATT 出错：${d.error}`
    : d.battery == null
      ? '已发现设备，但电池服务未返回数值（设备可能已断开或电量服务未就绪）。'
      : '电量来自标准蓝牙电池服务（0x180F），与 2.4G 通道读到的值是同一个电池。';
  logLine(`蓝牙：${d.name} ${d.address} 电量 ${d.battery ?? '—'}%`);
}

// How long we keep watching. The receiver itself counts down from 23 s, so a
// little more than that covers the whole window without over-running it.
const PAIR_WINDOW_MS = 26000;

// What the receiver reports in 0x06. Measured, not guessed:
//   0 = nothing bonded   1 = window open, counting down
//   2 = bonded           3 = bonded and idle (the state before a re-pair)
const PAIR_LABEL = {
  0: '空闲（没有配对）',
  1: '配对中',
  2: '已配对',
  3: '已配对',
};

/**
 * Re-pairing is destructive: opening the window drops the live link, and a
 * mouse that never answers it may not come back on its own. So the user gets
 * the whole plan up front, and the app then waits through the receiver's own
 * countdown instead of declaring "failed" 300 ms after pressing the button.
 */
async function startPairing() {
  if (state.pairing) return;
  const note = $('pair-note');
  const wasOnline = state.connection?.online === true;
  const ok = confirm(
    (wasOnline
      ? '注意：鼠标现在正通过这个接收器正常工作。\n' +
        '重新配对会先断开它几秒；如果鼠标和这个接收器本来就是一对，不需要重新配对。\n\n'
      : '') +
      '开始配对？\n\n' +
      '官方驱动的原话是：\n' +
      '「请让鼠标进入配对状态（同时按住左中右键 3 秒，直至对码指示灯快闪），并靠近接收器」\n\n' +
      '请先让鼠标快闪起来（切到 2.4G 档 → 按住左中右键 3 秒），再点「确定」。'
  );
  if (!ok) return;

  const btn = $('btn-pair');
  btn.disabled = true;
  btn.textContent = '配对中…';
  state.pairing = true;
  logLine('打开配对窗口…');

  let bonded = false;
  // The receiver only says "pairing" (1) while its window is open, and it
  // drops back to "bonded" (2) both when a mouse joins and when the countdown
  // simply runs out. So the tell is whether 2 arrives *early*: with seconds
  // left on the clock. The mouse answering is the second, independent tell.
  let opened = false;
  let leftWhenClosed = 0;
  let sawMouse = false;
  try {
    const start = await invoke('pair_begin');
    logLine(
      `接收器进入对码状态（CID 0x${start.cid.toString(16)} MID 0x${start.mid.toString(16)}）`
    );
    note.textContent =
      '接收器已进入对码状态，正在等待鼠标…' +
      '（若鼠标的对码灯已经不闪了，请再长按一次 左键+中键+右键 3 秒）';
    const deadline = Date.now() + PAIR_WINDOW_MS;
    while (Date.now() < deadline) {
      await sleep(600);
      const poll = await invoke('pair_poll');
      if (!opened && poll.state === 1) {
        opened = true;
        note.textContent = '接收器正在等待鼠标，请让鼠标保持快闪并靠近接收器。';
        logLine('配对状态：配对中');
      }
      if (poll.state === 1) {
        leftWhenClosed = poll.left;
      }
      if (opened && poll.state === 2 && leftWhenClosed > 1) {
        // Closed early: a mouse joined before the countdown ran out.
        bonded = true;
        logLine(`对码窗口提前结束（还剩 ${leftWhenClosed} 秒）`);
        break;
      }
      if (opened && poll.online) {
        sawMouse = true;
        bonded = true;
        break;
      }
      if (poll.left > 0) {
        btn.textContent = `配对中…${poll.left}s`;
      }
    }

    if (bonded) {
      const how = sawMouse ? '鼠标已应答接收器' : '接收器提前结束了对码窗口';
      note.textContent = `配对成功：${how}。鼠标几秒内会连回来，可以直接移动它试试。`;
      logLine(`配对成功（${how}）`);
      toast('ok', '配对成功');
    } else if (!opened) {
      note.textContent =
        '接收器没有进入对码状态。请换一个 USB 口插好接收器后再试；' +
        '部分接收器必须直接插在电脑上（不要经过 USB 集线器）。';
      logLine('配对失败：接收器没有打开对码窗口');
      toast('err', '接收器没有进入对码状态');
    } else {
      note.textContent =
        '对码窗口已关闭，期间没有鼠标应答。请确认鼠标在 2.4G 档、有电、靠近接收器，' +
        '并长按 左键+中键+右键 到指示灯快闪后再试一次。';
      logLine('配对超时：对码窗口内没有鼠标应答');
      toast('err', '这次没有等到鼠标');
    }
  } catch (e) {
    note.textContent = `无法进入配对模式：${e}`;
    logLine(`配对失败：${e}`);
    toast('err', `配对失败：${e}`);
  } finally {
    state.pairing = false;
    btn.disabled = false;
    btn.textContent = '开始配对';
    await loadConnection();
  }
}

/** A missing receiver and a sleeping mouse need different instructions. */
const isMissingDevice = (message) =>
  /VID 3554|没有找到|no Metaphyuni|could not be opened|无法打开接收器|没有检测到接收器/i.test(
    message || ''
  );

async function markOffline(reason) {
  state.online = false;
  $('conn-dot').className = 'dot off';
  $('conn-text').textContent = '未连接';
  // Nothing to write to while the link is down.
  $('btn-rate-apply').disabled = true;
  $('btn-rgb-apply').disabled = true;
  $('btn-rgb-revert').disabled = true;
  // Long range, the profile picker and the factory reset all write straight to
  // the device, so nothing should be clickable while the link is down.
  $('in-longrange').disabled = true;
  $('in-profile').disabled = true;
  $('btn-factory-reset').disabled = true;
  // Same for the key table and the indicator light: both write immediately.
  $('btn-keys-apply').disabled = true;
  $('btn-keys-default').disabled = true;

  // The receiver being silent has two very different explanations. If the
  // mouse is sitting on the Bluetooth channel it is still there — just on a
  // different radio — so look for it over BLE before declaring it missing.
  let ble = null;
  try {
    const devices = await bluetoothSnapshot(false);
    ble = devices.find((d) => d.connected) ?? devices[0] ?? null;
  } catch {
    ble = null;
  }

  if (ble) {
    $('dev-name').textContent = ble.name || '玄派鼠标（蓝牙）';
    $('dev-mode').textContent = ble.connected ? '蓝牙 · 已连接' : '蓝牙 · 未连接';
    // The rail follows the device even when it arrives over Bluetooth.
    $('brand-device').textContent = ble.name || '玄派鼠标（蓝牙）';
    $('conn-text').textContent = ble.connected ? '蓝牙已连接' : '蓝牙未连接';
    if (ble.battery != null) {
      $('m-battery').textContent = String(ble.battery);
      $('m-battery-bar').style.width = `${ble.battery}%`;
      $('m-battery-hint').textContent = '来自蓝牙电池服务（0x180F）';
    }
    banner(
      '鼠标正在蓝牙档：蓝牙链路只暴露标准电量服务，DPI／回报率等配置读写需要切回 2.4G 或有线档。',
      true,
      { label: '打开连接页', run: () => switchPage('connection') },
    );
    logLine(`蓝牙模式：${ble.name}（${ble.address}）电量 ${ble.battery ?? '—'}%`);
    renderBluetooth();
    return;
  }

  const label = isMissingDevice(reason) ? '未检测到接收器' : '未检测到设备';
  $('dev-name').textContent = label;
  $('dev-mode').textContent = '—';
  const rail = $('brand-device');
  rail.textContent = '未连接设备';
  rail.removeAttribute('title');
  // Only the value elements carry an id. Clearing every <span> in a <dd> also
  // wiped the "%" and "CPI" unit labels and the firmware row labels, so select
  // by id and leave the static text alone.
  [...document.querySelectorAll('.metrics [id]')].forEach((el) => {
    el.textContent = '—';
  });
  $('m-battery-bar').style.width = '0%';
  $('m-battery-hint').textContent = '';
  const message = reason || WAKE_HINT;
  banner(message, true, {
    label: isMissingDevice(message) ? '重新检测' : '唤醒设备',
    run: isMissingDevice(message) ? redetect : wakeAndReload,
  });
}

/** Drop the cached handle and look for the receiver again from scratch. */
async function redetect() {
  banner('正在重新检测设备…');
  logLine('重新检测设备…');
  try {
    await invoke('forget');
  } catch {
    /* forget only clears the cached handle; failing to do so is harmless */
  }
  await loadDevice();
  await loadConnection();
  try {
    const s = await invoke('status', { full: true });
    if (s.online) {
      state.online = true;
      state.missed = 0;
      banner('');
      renderStatus(s);
      await loadConfig();
      toast('ok', '已找到设备');
      return;
    }
  } catch {
    /* fall through to the offline banner below */
  }
  await markOffline(WAKE_HINT);
}

// Polling discipline: one poll in flight at a time, and the interval stretches
// while the mouse stays silent. A fixed 600 ms tick against a device that needs
// ~0.5 s to time out means the app never idles — which is what made an absent
// mouse look like a hung program.
let pollBusy = false;
let pollCount = 0;

async function pollStatus() {
  // During pairing the receiver is busy negotiating; polling it for a status
  // snapshot would steal the very replies the pairing window is waiting for.
  if (pollBusy || state.pairing) return;
  pollBusy = true;
  try {
    // The battery costs an extra command round trip; the poll only needs to know
    // whether the mouse answers at all.
    const s = await invoke('status', { full: false });
    if (!s.online) {
      state.missed = (state.missed || 0) + 1;
      if (state.online) await markOffline(WAKE_HINT);
      return;
    }
    state.missed = 0;
    if (!state.online) {
      // Came back: re-read everything so the panel is not showing stale values.
      state.online = true;
      banner('');
      await loadDevice();
      await loadConfig();
      // The quick poll never carries the battery; without one full read here it
      // would stay at "—" until the next manual refresh.
      try {
        renderStatus(await invoke('status', { full: true }));
      } catch {
        /* battery is optional; the next periodic full poll retries */
      }
      return;
    }
    // Every ~15 s pay one extra round trip for the battery so the gauge does
    // not sit on a stale number (or an em dash after a sleepy boot) forever.
    pollCount += 1;
    if (pollCount % 25 === 0) {
      try {
        renderStatus(await invoke('status', { full: true }));
      } catch {
        /* keep the last values */
      }
    }
    renderStatus(s);
  } catch {
    state.missed = (state.missed || 0) + 1;
    /* a single missed poll is normal while the mouse idles */
  } finally {
    pollBusy = false;
  }
}

/** 600 ms while it answers, up to 5 s while it does not. */
function schedulePoll() {
  const missed = state.missed || 0;
  const delay = missed === 0 ? 600 : Math.min(5000, 600 * 2 ** Math.min(missed, 3));
  setTimeout(async () => {
    await pollStatus();
    schedulePoll();
  }, delay);
}

function renderStatus(s) {
  // `status` returns the full Battery struct { level, charging, millivolts };
  // older builds sent a bare number, so both shapes are accepted here.
  const batt = s.battery;
  const level = batt == null ? null : (typeof batt === 'object' ? batt.level : batt);
  // The poll skips the battery round trip on purpose; when it is absent keep the
  // last value rather than blanking the panel on every tick.
  if (level != null) {
    // The device reports charge in 5 % steps, so "85" can sit there while the
    // cell drains. When there is a discharge voltage, the curve estimate is
    // the finer reading; while charging the voltage is meaningless and the
    // device's own level is the honest answer.
    const shown =
      batt && typeof batt === 'object' && batt.estimate != null
        ? Math.round(batt.estimate)
        : level;
    $('m-battery').textContent = shown;
    $('m-battery-bar').style.width = `${shown}%`;
    const notes = [];
    if (batt && typeof batt === 'object') {
      if (batt.charging) {
        notes.push('充电中');
      } else if (batt.estimate != null) {
        notes.push(`设备报告 ${level}%`);
      }
      if (batt.millivolts) notes.push(`${batt.millivolts} mV`);
    }
    $('m-battery-hint').textContent = notes.join(' · ');
  }
  // Same policy as the battery: one failed sub-read while the mouse dozes off
  // must not blank a value that was true a second ago.
  if (s.dpi != null) $('m-dpi').textContent = s.dpi;
  if (s.dpiIndex != null) $('m-stage').textContent = s.dpiIndex;
  if (s.rateByte != null) $('m-rate').textContent = rateLabel(s.rateByte);

  // The device is the authority on the active stage, so follow it live — this is
  // what makes the highlighted row track the physical DPI button.
  if (state.snapshot && s.dpiIndex != null) {
    const stage = s.dpiIndex - 1;
    if (stage !== state.snapshot.activeStage && state.draft.activeStage === undefined) {
      state.snapshot.activeStage = stage;
      renderDpi();
    }
  }

  // The rate the mouse is running now, straight from the header read, so the
  // picker follows the device even before the full block is read again.
  if (s.rateByte != null && state.snapshot) {
    state.snapshot.reportRate = s.rateByte;
    state.snapshot.rateHz = REPORT_RATE[s.rateByte]
      ? Number(REPORT_RATE[s.rateByte].replace(' Hz', ''))
      : null;
    updateRateButton();
  }
}

// ── rendering ───────────────────────────────────────────────────────────

function renderAll() {
  renderOverview();
  renderDpi();
  renderButtons();
  renderPerformance();
  renderPower();
  refreshApplyButton();
}

function renderOverview() {
  const s = state.snapshot;
  const draft = { ...s, ...state.draft };
  if (!s) return;
  $('kv-debounce').textContent = `${draft.debounce ?? '—'} ms`;
  $('kv-sleep').textContent = `闲置 ${sleepLabel(draft.sleepTimeout ?? 0)} 后`;
  $('kv-motion').textContent = draft.motionSync ? '开' : '关';
  $('kv-ripple').textContent = draft.rippleControl ? '开' : '关';
}

function renderDpi() {
  const list = $('dpi-list');
  const count = effectiveCount();
  const stages = effectiveStages();
  const active = effectiveStage();
  $('dpi-count').textContent = count || '—';

  const select = $('in-stage-count');
  if (select.options.length !== MAX_STAGES) {
    select.innerHTML = '';
    for (let n = 1; n <= MAX_STAGES; n += 1) {
      const opt = document.createElement('option');
      opt.value = String(n);
      opt.textContent = `${n} 档`;
      select.append(opt);
    }
  }
  select.value = String(count);
  select.onchange = () => {
    resizeStages(Number(select.value));
    renderDpi();
    renderOverview();
    refreshApplyButton();
  };

  list.innerHTML = '';

  stages.slice(0, count).forEach((cpi, index) => {
    const row = document.createElement('div');
    row.className = `dpi-row${index === active ? ' active' : ''}`;

    const idx = document.createElement('div');
    idx.className = 'dpi-idx';
    idx.textContent = index + 1;

    const input = document.createElement('input');
    input.type = 'number';
    input.className = 'dpi-input';
    input.min = '50';
    input.max = '26000';
    input.step = '50';
    input.value = String(cpi);
    input.title = '50 – 26000 CPI，步进 50';
    // The X and Y axes are stored separately in the same record. They are
    // normally identical; when they are not, show it rather than pretending
    // the mouse only has one sensitivity.
    const xCell = document.createElement('div');
    xCell.className = 'dpi-xy';
    xCell.append(input);
    if (state.snapshot?.xyIndependent) {
      const yTag = document.createElement('span');
      yTag.className = 'dpi-y';
      yTag.textContent = `× ${state.snapshot.dpiStagesY?.[index] ?? cpi}`;
      yTag.title = '该档的 Y 轴 CPI（只读：本应用只写 X/Y 相同的档位）';
      xCell.append(yTag);
    }
    input.addEventListener('change', () => {
      const value = Math.min(26000, Math.max(50, Math.round((Number(input.value) || 50) / 50) * 50));
      input.value = String(value);
      const next = [...effectiveStages()];
      next[index] = value;
      edit('dpiStages', next);
      renderDpi();
      renderOverview();
    });

    const slider = document.createElement('input');
    slider.type = 'range';
    slider.className = 'dpi-slider';
    slider.min = '50';
    slider.max = '26000';
    slider.step = '50';
    slider.value = String(cpi);
    slider.addEventListener('input', () => {
      input.value = slider.value;
    });
    slider.addEventListener('change', () => {
      const next = [...effectiveStages()];
      next[index] = Number(slider.value);
      edit('dpiStages', next);
      renderDpi();
      renderOverview();
    });

    const colorCell = document.createElement('div');
    colorCell.className = 'dpi-color';
    const hex = state.snapshot?.colors?.[index] ?? '#000000';
    const swatch = document.createElement('span');
    swatch.className = 'swatch';
    swatch.style.background = hex;
    swatch.title = `设备中的档位颜色 ${hex}（只读）`;
    colorCell.append(swatch, document.createTextNode(hex.slice(1).toUpperCase()));

    const action = document.createElement(index === active ? 'span' : 'button');
    if (index === active) {
      action.className = 'dpi-badge';
      action.textContent = '当前使用';
    } else {
      action.className = 'btn dpi-btn';
      action.textContent = '设为当前';
      action.addEventListener('click', () => {
        edit('activeStage', index);
        renderDpi();
      });
    }

    row.append(idx, xCell, slider, colorCell, action);
    list.append(row);
  });
}

// ── button assignment ───────────────────────────────────────────────────

/**
 * Pending key assignments: index -> action label. Kept apart from the draft
 * because buttons go out through `set_button`, one write per key with its own
 * left-click guard, not through the whole-block `apply`.
 */
state.keyDraft = {};

/** Marks the row's dropdown choice that opens the combination editor. */
const COMBO_CHOICE = '__combo__';
/** …and the one that opens the macro editor. */
const MACRO_CHOICE = '__macro__';

function keyGroups() {
  const b = state.caps?.buttons ?? {};
  return {
    actions: b.actions ?? ['左键', '右键', '中键', '后退', '前进', 'DPI 循环'],
    unverified: b.unverified ?? [],
    modifiers: b.modifiers ?? [],
    keys: b.keys ?? [],
    consumer: b.consumer ?? [],
  };
}

function keyRows() {
  return state.snapshot?.buttons ?? [];
}

/** The label a row's dropdown should show right now. */
function keyChoice(row) {
  const draft = state.keyDraft[row.index];
  if (draft === undefined) return row.action;
  if (typeof draft === 'string') return draft;
  return draft.macro ? MACRO_CHOICE : COMBO_CHOICE;
}

/** `宏 · 3 步 × 2` — enough to tell two drafts apart in the closed dropdown. */
function macroPreview(entry) {
  if (!entry.macro) return '';
  const name = entry.name ? `${entry.name} · ` : '';
  return `宏：${name}${entry.steps.length} 步`;
}

/** `Ctrl+C`, `播放/暂停` … from the numbers the backend expects back. */
function comboText(entry) {
  const g = keyGroups();
  const mods = g.modifiers.filter((m) => entry.mods & m.bit).map((m) => m.label);
  const list = entry.consumer ? g.consumer : g.keys;
  const hit = list.find((k) => k.usage === entry.usage);
  return [...mods, hit ? hit.label : `0x${entry.usage.toString(16)}`].join('+');
}

function comboPreview(entry) {
  if (!entry.combo) return '';
  const label = comboText(entry);
  return label || '尚未选择按键';
}

/**
 * The 修饰键 + 按键 / 多媒体 rows behind a class-5 binding.
 *
 * Kept inline rather than in a dialog: the whole point of this row is which
 * physical button it belongs to, and being able to see all six side by side is
 * what makes "left click is still bound somewhere" obvious.
 */
function buildComboEditor(row) {
  const g = keyGroups();
  const entry = state.keyDraft[row.index];
  const box = document.createElement('div');
  box.className = 'key-editor';

  const modRow = document.createElement('div');
  modRow.className = 'combo-mods';
  for (const mod of g.modifiers) {
    const label = document.createElement('label');
    const input = document.createElement('input');
    input.type = 'checkbox';
    input.checked = !!(entry.mods & mod.bit);
    input.addEventListener('change', () => {
      entry.mods = (entry.mods & ~mod.bit) | (input.checked ? mod.bit : 0);
      preview.textContent = comboPreview(entry);
    });
    label.append(input, document.createTextNode(mod.label));
    modRow.append(label);
  }

  const keySelect = document.createElement('select');
  const keyGroup = document.createElement('optgroup');
  keyGroup.label = '键盘键';
  for (const key of g.keys) keyGroup.append(new Option(key.label, `k:${key.usage}`));
  const mediaGroup = document.createElement('optgroup');
  mediaGroup.label = '多媒体 / 常用程序';
  for (const key of g.consumer) mediaGroup.append(new Option(key.label, `c:${key.usage}`));
  keySelect.append(keyGroup, mediaGroup);
  keySelect.value = `${entry.consumer ? 'c' : 'k'}:${entry.usage}`;
  keySelect.addEventListener('change', () => {
    const [kind, raw] = keySelect.value.split(':');
    entry.usage = Number(raw);
    entry.consumer = kind === 'c';
    preview.textContent = comboPreview(entry);
  });

  const preview = document.createElement('b');
  preview.className = 'combo-preview';
  preview.textContent = comboPreview(entry);

  const hint = document.createElement('span');
  hint.className = 'note';
  hint.textContent = '本机实测有效：写入后按下这个按键即是该组合。改回普通动作不会留下副作用。';

  const keyRow = document.createElement('div');
  keyRow.className = 'combo-keys';
  keyRow.append(keySelect, preview);
  box.append(modRow, keyRow, hint);
  return box;
}

// ── macros ──────────────────────────────────────────────────────────────

/** Events a step turns into: modifiers are events of their own. */
function macroEvents(steps) {
  return steps.reduce((n, step) => n + (step.mods ? 4 : 2), 0);
}

function macroLimit() {
  return state.caps?.macro ?? { maxEvents: 70, minDelay: 3, maxDelay: 5000, nameChars: 15 };
}

/** Labels for the loop-after-the-last-step choices the firmware knows. */
const MACRO_REPEATS = [
  { value: 1, label: '执行 1 次' },
  { value: 2, label: '执行 2 次' },
  { value: 3, label: '执行 3 次' },
  { value: 5, label: '执行 5 次' },
  { value: 0xfe, label: '按住时循环' },
  { value: 0xff, label: '按任意键停止' },
];

/**
 * The step list behind a class-6 binding.
 *
 * One row per step: which modifiers, which key, how long to hold. Keeping a
 * step to a single key is deliberate — it makes the event count something the
 * user can see (every step is 2 or 4 events of the 70 the firmware stores)
 * instead of a number that fails at write time.
 */
function buildMacroEditor(row) {
  const g = keyGroups();
  const limit = macroLimit();
  const entry = state.keyDraft[row.index];
  const box = document.createElement('div');
  box.className = 'key-editor macro-editor';

  // Name + repeat, side by side.
  const head = document.createElement('div');
  head.className = 'macro-head';
  const name = document.createElement('input');
  name.type = 'text';
  name.placeholder = '宏名称';
  name.maxLength = limit.nameChars;
  name.value = entry.name ?? '';
  name.addEventListener('input', () => { entry.name = name.value; });
  const repeat = document.createElement('select');
  for (const option of MACRO_REPEATS) repeat.append(new Option(option.label, String(option.value)));
  repeat.value = String(entry.repeat ?? 1);
  repeat.addEventListener('change', () => { entry.repeat = Number(repeat.value); });
  head.append(name, repeat);

  const list = document.createElement('div');
  list.className = 'macro-steps';

  const counter = document.createElement('span');
  counter.className = 'note';
  const refreshCount = () => {
    const used = macroEvents(entry.steps);
    counter.textContent = `${entry.steps.length} 步 · ${used} / ${limit.maxEvents} 个事件`;
    counter.classList.toggle('over', used > limit.maxEvents);
  };

  const stepRow = (step, at) => {
    const line = document.createElement('div');
    line.className = 'macro-step';

    const mods = document.createElement('div');
    mods.className = 'combo-mods';
    for (const mod of g.modifiers) {
      const label = document.createElement('label');
      const input = document.createElement('input');
      input.type = 'checkbox';
      input.checked = !!(step.mods & mod.bit);
      input.addEventListener('change', () => {
        step.mods = (step.mods & ~mod.bit) | (input.checked ? mod.bit : 0);
        refreshCount();
      });
      label.append(input, document.createTextNode(mod.label));
      mods.append(label);
    }

    const key = document.createElement('select');
    const keys = document.createElement('optgroup');
    keys.label = '键盘键';
    for (const k of g.keys) keys.append(new Option(k.label, `k:${k.usage}`));
    const buttons = document.createElement('optgroup');
    buttons.label = '鼠标键';
    for (const [bit, text] of [[0x01, '左键'], [0x02, '右键'], [0x04, '中键'], [0x08, '后退'], [0x10, '前进']]) {
      buttons.append(new Option(text, `m:${bit}`));
    }
    key.append(keys, buttons);
    key.value = `${step.keyboard ? 'k' : 'm'}:${step.usage}`;
    key.addEventListener('change', () => {
      const [kind, raw] = key.value.split(':');
      step.keyboard = kind === 'k';
      step.usage = Number(raw);
    });

    const delay = document.createElement('input');
    delay.type = 'number';
    delay.min = String(limit.minDelay);
    delay.max = String(limit.maxDelay);
    delay.step = '10';
    delay.value = String(step.delayMs ?? 100);
    delay.title = '这一步按住多久（毫秒）';
    delay.addEventListener('input', () => {
      step.delayMs = Math.max(limit.minDelay, Number(delay.value) || limit.minDelay);
    });

    const unit = document.createElement('span');
    unit.className = 'note';
    unit.textContent = 'ms';

    const drop = document.createElement('button');
    drop.className = 'btn ghost tiny';
    drop.textContent = '×';
    drop.title = '删除这一步';
    drop.disabled = entry.steps.length <= 1;
    drop.addEventListener('click', () => {
      entry.steps.splice(at, 1);
      renderButtons();
    });

    line.append(mods, key, delay, unit, drop);
    return line;
  };

  entry.steps.forEach((step, at) => list.append(stepRow(step, at)));

  const actions = document.createElement('div');
  actions.className = 'macro-actions';
  const add = document.createElement('button');
  add.className = 'btn ghost tiny';
  add.textContent = '＋ 添加一步';
  add.disabled = macroEvents(entry.steps) + 2 > limit.maxEvents;
  add.addEventListener('click', () => {
    entry.steps.push({ mods: 0, usage: g.keys[0]?.usage ?? 0x04, keyboard: true, delayMs: 100 });
    renderButtons();
  });
  actions.append(add, counter);
  refreshCount();

  const hint = document.createElement('span');
  hint.className = 'note';
  hint.textContent = '宏存在 0x0300 起的独立区域（每槽 384 字节），按键记录用类别 0x06 指向它。'
    + '本机实测有效：把两步 F24 的宏绑到前进键，按下后系统收到两次 F24、间隔与设定的 250 ms 一致 —— '
    + '也就是说延时字段也被采纳。改回普通动作不会留下副作用。';

  box.append(head, list, actions, hint);
  return box;
}

function refreshKeysButton() {
  const dirty = Object.keys(state.keyDraft).length;
  const btn = $('btn-keys-apply');
  btn.disabled = state.busy || !state.online || dirty === 0;
  btn.textContent = dirty ? `应用修改 (${dirty})` : '应用修改';
}

function renderButtons() {
  const table = $('key-table');
  const note = $('keys-note');
  table.textContent = '';
  const rows = keyRows();
  if (!rows.length) {
    note.textContent = '未连接设备，按键表读取中…';
    return;
  }
  const g = keyGroups();
  const allLabels = [...g.actions, ...g.unverified];

  for (const row of rows) {
    const line = document.createElement('div');
    line.className = 'key-row';
    // An assignment this build cannot name is shown but left alone: writing
    // back a dropdown choice would silently replace a vendor macro.
    const custom = row.action.startsWith('自定义');
    if (custom) line.classList.add('key-custom');
    if (state.keyDraft[row.index] !== undefined) line.classList.add('key-dirty');

    const name = document.createElement('span');
    name.className = 'key-name';
    name.textContent = row.name;

    const select = document.createElement('select');
    // Whatever the mouse holds stays selectable, so the dropdown never claims
    // something different from what is stored, and re-saving it is a no-op. A
    // combination is read back out of its own slot, whose label lives beside
    // the buttons rather than in the button record.
    if (!allLabels.includes(row.action)) {
      const ref = (state.snapshot?.shortcuts ?? []).find((s) => s.slot === row.index);
      const text = ref?.label
        ? `组合键 · ${ref.label}（保持不变）`
        : `${row.action}（保持不变）`;
      select.append(new Option(text, row.action));
    }
    const plain = document.createElement('optgroup');
    plain.label = '常用';
    for (const label of g.actions) plain.append(new Option(label, label));
    const extra = document.createElement('optgroup');
    extra.label = '未在本机验证';
    for (const label of g.unverified) extra.append(new Option(label, label));
    const special = document.createElement('optgroup');
    special.label = '键盘 / 多媒体 / 宏';
    special.append(new Option('组合键或多媒体键…', COMBO_CHOICE));
    special.append(new Option('宏（多步按键序列）…', MACRO_CHOICE));
    select.append(plain, extra, special);
    if (state.keyDraft[row.index]?.combo) {
      select.append(new Option(comboPreview(state.keyDraft[row.index]), COMBO_CHOICE));
    }
    if (state.keyDraft[row.index]?.macro) select.append(new Option(macroPreview(state.keyDraft[row.index]), MACRO_CHOICE));
    select.value = keyChoice(row);
    select.disabled = !state.online;
    select.addEventListener('change', () => {
      if (select.value === row.action) {
        delete state.keyDraft[row.index];
      } else if (select.value === COMBO_CHOICE) {
        state.keyDraft[row.index] = {
          combo: true, mods: 0, usage: g.keys[0]?.usage ?? 0x04, consumer: false,
        };
      } else if (select.value === MACRO_CHOICE) {
        state.keyDraft[row.index] = {
          macro: true,
          name: '',
          repeat: 1,
          steps: [{ mods: 0, usage: g.keys[0]?.usage ?? 0x04, keyboard: true, delayMs: 100 }],
        };
      } else {
        state.keyDraft[row.index] = select.value;
      }
      renderButtons();
      refreshKeysButton();
    });

    line.append(name, select);
    if (state.keyDraft[row.index]?.combo) line.append(buildComboEditor(row));
    if (state.keyDraft[row.index]?.macro) line.append(buildMacroEditor(row));
    table.append(line);
  }

  const custom = rows.filter((r) => r.action.startsWith('自定义')).length;
  const invalid = rows.filter((r) => !r.valid).length;
  note.textContent = custom || invalid
    ? `读到 ${rows.length} 个按键；${custom} 个绑定超出本应用可写的动作范围（按 class 解码后显示），${invalid} 个记录校验不符。`
    : `读到 ${rows.length} 个按键，全部记录校验吻合。`;
  refreshKeysButton();
}

/**
 * Apply pending key bindings.
 *
 * Rows gaining a 左键 go first, rows losing theirs last, so a batch that moves
 * left click onto another button never passes through a state where no button
 * can click. `set_button` re-reads the block and re-checks the guard on every
 * call, so this ordering is a convenience, not the only line of defence.
 */
async function applyKeyDraft() {
  const entries = Object.entries(state.keyDraft);
  if (!entries.length) return;
  const rows = keyRows();
  const isLeft = (value) => value === '左键';
  // Combos cannot steal the left click, so they sort last by construction.
  entries.sort((a, b) => Number(isLeft(b[1])) - Number(isLeft(a[1])));

  state.busy = true;
  $('btn-keys-apply').textContent = '正在写入…';
  try {
    for (const [index, value] of entries) {
      const who = rows[index]?.name ?? index;
      if (typeof value === 'object' && value.macro) {
        await invoke('set_button_macro', {
          index: Number(index),
          name: value.name ?? '',
          repeat: value.repeat ?? 1,
          steps: value.steps.map((s) => ({
            modifiers: s.mods, usage: s.usage, delayMs: s.delayMs, keyboard: s.keyboard,
          })),
        });
        logLine(`按键 ${who} → ${macroPreview(value)}`);
      } else if (typeof value === 'object' && value.combo) {
        await invoke('set_button_combo', {
          index: Number(index),
          modifiers: value.mods,
          usage: value.usage,
          consumer: value.consumer,
        });
        logLine(`按键 ${who} → ${comboText(value)}`);
      } else {
        await invoke('set_button', { index: Number(index), action: value });
        logLine(`按键 ${who} → ${value}`);
      }
    }
    state.keyDraft = {};
    await loadConfig();
  } catch (e) {
    banner(`${e}`, true);
    logLine(`按键写入失败：${e}`);
    await loadConfig();
  } finally {
    state.busy = false;
    refreshKeysButton();
  }
}

/**
 * Reset every button to the factory binding. One shot per row through the
 * same guarded path as a manual change, so the table never bypasses the
 * left-click check even on the way back to defaults.
 */
async function resetKeyDefaults() {
  const defaults = ['左键', '右键', '中键', '后退', '前进', 'DPI 循环'];
  if (!confirm('把全部按键恢复为出厂绑定？')) return;
  state.keyDraft = {};
  const rows = keyRows();
  rows.forEach((row, i) => {
    if (row.action !== defaults[i]) state.keyDraft[i] = defaults[i];
  });
  if (Object.keys(state.keyDraft).length) await applyKeyDraft();
}

/**
 * Read a vendor firmware image and say whether it belongs on this device.
 *
 * The whole point of this page is the compatibility verdict. The flashing
 * protocol is driven by commands stored inside the image, so a mismatch is not
 * a polite error from the device — writing a CX52650N image to a CX52850P
 * leaves you with an unresponsive mouse.
 */
async function inspectFirmware(file) {
  state.firmwareFile = file;
  state.flashing = false;
  const box = $('fw-result');
  box.classList.remove('hidden');
  box.innerHTML = '<p class="note">正在解析…</p>';
  try {
    // Only the header is needed, and the WebView will not tell us the file's
    // real path — so read the first kilobyte and send the bytes themselves.
    const slice = await file.slice(0, 1024).arrayBuffer();
    const img = await invoke('parse_firmware', {
      name: file.name,
      size: file.size,
      header: Array.from(new Uint8Array(slice)),
    });
    state.firmware = img;
    const rows = [
      ['文件', img.fileName],
      ['大小', `${img.fileSize.toLocaleString()} 字节（固件体 ${img.firmwareLength.toLocaleString()}）`],
      ['固件版本', `<b>${img.version}</b>`],
      ['适用', `${img.deviceTypeLabel}（类型 ${img.deviceType}）`],
      ['主控芯片', `<b>${img.icName || '—'}</b>`],
      ['CID / MID', `${img.cid} / ${img.mid}`],
      ['产品名', img.productName || '—'],
      ['传感器', img.sensorName || '—'],
      ['头部 CRC / 长度', `0x${img.headCrc.toString(16)} / ${img.headerLength}`],
      ['bootloader 输入端点', img.bootInputEndpoint || '—'],
      ['bootloader 输出端点', img.bootOutputEndpoint || '—'],
      ['进入升级命令', img.resetToUpdateModeCmd || '—'],
      ['准备下载命令', img.prepareDownloadCmd || '—'],
      ['数据下载命令', img.dataDownloadCmd || '—'],
    ];
    box.innerHTML = `
      <div class="fw-verdict ${img.compatible ? 'ok' : 'no'}">${img.compatibility}</div>
      <ul class="kv">
        ${rows.map(([k, v]) => `<li><span>${k}</span><b>${v}</b></li>`).join('')}
      </ul>
      <p class="note">
        上面的三条命令与 bootloader 端点都来自这个文件本身。它们是厂商升级库实际使用的参数，
        列出来是为了让你核对这个包是不是真的对应本机 —— 解析不会向设备写入任何东西。
      </p>`;
    $('fw-actions').classList.toggle('hidden', !img.compatible);
    if (img.compatible) $('fw-status').textContent =
      '包已通过校验。刷写会让设备自动重启进 bootloader —— 有线模式、电量充足、全程不要断电。';
    logLine(`固件包 ${img.fileName}：${img.compatibility}`);
    toast(img.compatible ? 'ok' : 'err', img.compatible ? '固件包与本机匹配' : '固件包与本机不匹配');
  } catch (e) {
    box.innerHTML = `<div class="fw-verdict no">${e}</div>`;
    logLine(`固件包解析失败：${e}`);
    toast('err', String(e));
  }
}

// ── flashing ─────────────────────────────────────────────────────────────
// The vendor's 32-bit engine does the actual flash inside a helper process;
// these two commands are the UI's only way in. `firmware_probe` is
// read-only; `firmware_flash` streams progress through the `fw-event` channel.

async function probeFirmware() {
  if (!state.firmware || state.flashing) return;
  const file = state.firmwareFile;
  const btn = $('btn-fw-probe');
  const status = $('fw-status');
  btn.disabled = true;
  status.textContent = '正在解析固件包并询问厂商升级库…';
  try {
    const bytes = Array.from(new Uint8Array(await file.arrayBuffer()));
    const probe = await invoke('firmware_probe', { bytes });
    if (probe.bootCount === 1) {
      status.textContent = `bootloader 就绪：${probe.bootDevices[0]}。可以开始刷写。`;
      $('btn-fw-flash').disabled = false;
    } else if (probe.bootCount === 0) {
      status.textContent =
        '还没有设备进入 bootloader 模式。这是正常的 —— 点击「开始刷写」时，' +
        '设备会收到固件头里的重启命令后自行进入，无需手动操作。';
    } else {
      status.textContent =
        `发现了 ${probe.bootCount} 台匹配的 bootloader 设备，为避免刷错端，已阻止刷写。` +
        '请只保留一台设备后重试。';
    }
    for (const l of probe.logs) logLine(`升级库：${l}`);
  } catch (e) {
    status.textContent = `探测失败：${e}`;
    toast('err', String(e));
  } finally {
    btn.disabled = false;
  }
}

async function flashFirmware() {
  const file = state.firmwareFile;
  if (!file || state.flashing) return;
  const ok = confirm(
    '即将刷写固件。开始后设备会自动重启进入 bootloader 并整包写入。\n\n' +
      '请确认：\n' +
      '  · 鼠标处于有线模式，数据线已插牢\n' +
      '  · 电量充足\n' +
      '  · 全程不要拔线、关机、休眠\n\n' +
      '确认开始？'
  );
  if (!ok) return;
  state.flashing = true;
  const btn = $('btn-fw-flash');
  const probe = $('btn-fw-probe');
  btn.disabled = true;
  probe.disabled = true;
  const status = $('fw-status');
  const bar = $('fw-bar');
  bar.style.width = '0%';
  // `listen` hands back a Promise that resolves to the unlisten function, so
  // it has to be awaited before it can be called. Calling the Promise itself
  // throws, and the throw lands in the `finally` below — which used to strand
  // `state.flashing` at true and leave both buttons disabled for good.
  let unlisten = null;
  const listenForEvents = () => tauriEvent.listen('fw-event', (e) => {
    const v = e.payload || {};
    if (v.ev === 'progress') {
      bar.style.width = `${v.pct}%`;
      status.textContent = `写入中… ${v.pct}%`;
    } else if (v.ev === 'state') {
      const names = { 1: '擦除备份区', 2: '擦除主区', 3: '拷贝到主区', 4: '校验备份区', 5: '校验主区', 32: '等待设备重启', 136: '升级成功' };
      status.textContent = `设备状态：${names[v.code] || `代码 ${v.code}`}`;
    } else if (v.ev === 'result' && v.ok) {
      bar.style.width = '100%';
      status.textContent = '刷写完成。设备会自行重启，几秒后重新读取版本核对。';
    } else if (v.ev === 'logs') {
      for (const l of v.lines || []) logLine(`升级库：${l}`);
    }
  });
  try {
    unlisten = await listenForEvents();
    const bytes = Array.from(new Uint8Array(await file.arrayBuffer()));
    const result = await invoke('firmware_flash', { bytes });
    if (result && result.ok) {
      toast('ok', '固件刷写完成');
      logLine('固件刷写完成');
    } else {
      bar.style.width = '0%';
      status.textContent = '刷写失败。通常可以直接重试；详细日志见上方记录。';
      toast('err', '固件刷写失败');
      logLine(`固件刷写失败：${JSON.stringify(result)}`);
    }
  } catch (e) {
    bar.style.width = '0%';
    status.textContent = `刷写失败：${e}`;
    toast('err', String(e));
    logLine(`固件刷写失败：${e}`);
  } finally {
    // Only a real function may be called here. Anything else would throw and
    // skip the three lines below it — the exact failure this block exists to
    // prevent — so the check comes first and the reset never depends on it.
    if (typeof unlisten === 'function') unlisten();
    state.flashing = false;
    btn.disabled = false;
    probe.disabled = false;
  }
}

function renderPerformance() {
  const draft = { ...state.snapshot, ...state.draft };
  if (!state.snapshot) return;
  $('in-debounce').value = String(draft.debounce ?? 8);
  $('out-debounce').textContent = `${draft.debounce ?? 8} ms`;
  $('in-motion').checked = Boolean(draft.motionSync);
  $('in-ripple').checked = Boolean(draft.rippleControl);
  $('in-angle').checked = Boolean(draft.angleSnapping);
  // Values are the stored bytes, deliberately not the label order: 1 is the
  // middle setting, so sorting the options by value would read 低/中/高 as
  // 1/2/3 and write the wrong one.
  $('in-lod').value = String(draft.lod ?? 1);
  $('in-dpil-mode').value = String(draft.dpiLightMode ?? 0);
  $('in-dpil-bright').value = String(draft.dpiLightBrightness ?? 5);
  $('out-dpil-bright').textContent = String(draft.dpiLightBrightness ?? 5);
  $('in-dpil-speed').value = String(draft.dpiLightSpeed ?? 3);
  $('out-dpil-speed').textContent = String(draft.dpiLightSpeed ?? 3);
  renderRateOptions();
}

/**
 * Rebuild the report-rate picker from what this endpoint has actually proven.
 *
 * 4K receivers carry all six rates; the 1K and wired endpoints were only
 * measured at 125 and 1000 Hz, so those are all they are offered. Whatever
 * the device is running right now is selected even when it is outside the
 * list — a mouse set to a rate we cannot produce must still be readable.
 */
function renderRateOptions() {
  const rates = (state.info && state.info.reportRates) || [];
  const select = $('in-rate');
  const current = state.snapshot ? state.snapshot.rateHz : null;
  if (rates.length === 0) {
    select.innerHTML = '';
    select.disabled = true;
    $('btn-rate-apply').disabled = true;
    $('rate-note').textContent = '当前端点未开放回报率写入。';
    return;
  }
  const values = rates.slice();
  // The device may be running a rate the endpoint does not offer. Show it —
  // but as a read-only entry, since it cannot be written back.
  if (current && !values.includes(current)) {
    values.push(current);
    values.sort((a, b) => a - b);
    select.innerHTML = values
      .map(
        (hz) =>
          `<option value="${hz}">${hz} Hz${rates.includes(hz) ? '' : '（只读）'}</option>`
      )
      .join('');
  } else {
    select.innerHTML = values.map((hz) => `<option value="${hz}">${hz} Hz</option>`).join('');
  }
  select.value = String(current || values[0]);
  select.disabled = false;
  $('rate-note').textContent = `本端点开放：${rates.map((r) => `${r} Hz`).join(' / ')}。写入会让接收器短暂静默，约 0.3 秒后回读确认。`;
  updateRateButton();
}

/** Enable "应用" only for a rate that is writable and not already in effect. */
function updateRateButton() {
  const rates = (state.info && state.info.reportRates) || [];
  const value = Number($('in-rate').value);
  const current = state.snapshot ? state.snapshot.rateHz : null;
  const btn = $('btn-rate-apply');
  if (!state.online) {
    btn.disabled = true;
    btn.title = '鼠标未在线';
    return;
  }
  if (!rates.includes(value)) {
    btn.disabled = true;
    btn.title = '这个档位在本端点上没有实测依据，不提供写入';
    return;
  }
  // Nothing to do: the device is already there, and a write costs it a pause.
  btn.disabled = value === current;
  btn.title = btn.disabled ? '设备已是该回报率' : '';
}

// ── receiver LED ─────────────────────────────────────────────────────────
// Only a 4K receiver has one; the card stays hidden for anything else, because
// the probe in `device_info` — not a guess in the catalogue — is what decides.

/** The three colours are the three steps the chosen mode switches between. */
const RGB_STEPS = {
  1: ['颜色 1', '颜色 2', '颜色 3'],
  2: ['电量高', '电量中', '电量低'],
  3: ['颜色 1', '颜色 2', '颜色 3'],
};

function renderDongleRgb(info) {
  const rgb = info.dongleRgb;
  const card = $('card-rgb');
  card.classList.toggle('hidden', !rgb);
  if (!rgb) return;
  state.rgbOriginal = {
    mode: rgb.mode, color1: rgb.color1, color2: rgb.color2, color3: rgb.color3,
  };
  state.rgbSaved = { ...state.rgbOriginal };
  $('in-rgb-mode').value = String(rgb.mode);
  $('in-rgb-1').value = rgb.color1;
  $('in-rgb-2').value = rgb.color2;
  $('in-rgb-3').value = rgb.color3;
  updateRgbLabels();
  $('rgb-note').textContent = `当前：${rgb.modeLabel}。改完点应用，写入后会读回核对。`;
  updateRgbButton();
}

function updateRgbLabels() {
  const names = RGB_STEPS[Number($('in-rgb-mode').value)] || RGB_STEPS[1];
  $('lbl-rgb-1').textContent = names[0];
  $('lbl-rgb-2').textContent = names[1];
  $('lbl-rgb-3').textContent = names[2];
}

/** Only offer a write when something actually differs from what is stored. */
function updateRgbButton() {
  const cur = state.rgbOriginal;
  const same = cur
    && Number($('in-rgb-mode').value) === cur.mode
    && $('in-rgb-1').value === cur.color1
    && $('in-rgb-2').value === cur.color2
    && $('in-rgb-3').value === cur.color3;
  $('btn-rgb-apply').disabled = !state.online || same;
  $('btn-rgb-apply').title = same ? '与设备当前灯效一致' : '';
  $('btn-rgb-revert').disabled = !state.online || same;
}

function currentRgbPick() {
  return {
    mode: Number($('in-rgb-mode').value),
    color1: $('in-rgb-1').value,
    color2: $('in-rgb-2').value,
    color3: $('in-rgb-3').value,
  };
}

/** Put the values the receiver reports back into the controls. */
function showRgb(rgb) {
  state.rgbOriginal = {
    mode: rgb.mode, color1: rgb.color1, color2: rgb.color2, color3: rgb.color3,
  };
  $('in-rgb-mode').value = String(rgb.mode);
  $('in-rgb-1').value = rgb.color1;
  $('in-rgb-2').value = rgb.color2;
  $('in-rgb-3').value = rgb.color3;
  updateRgbLabels();
  updateRgbButton();
}

async function applyDongleRgb() {
  const btn = $('btn-rgb-apply');
  const pick = currentRgbPick();
  state.busy = true;
  refreshApplyButton();
  btn.disabled = true;
  btn.textContent = '写入中…';
  try {
    const got = await invoke('set_dongle_rgb', pick);
    showRgb(got);
    $('rgb-note').textContent = `已写入并读回：${got.modeLabel}。`;
    toast('ok', '接收器灯效已写入');
    logLine(`接收器灯效写入：模式 ${pick.mode} ${pick.color1} ${pick.color2} ${pick.color3} → 回读 ${got.modeLabel}`);
  } catch (e) {
    $('rgb-note').textContent = `写入失败：${e}`;
    toast('err', `${e}`);
    logLine(`接收器灯效写入失败：${e}`);
  } finally {
    state.busy = false;
    btn.textContent = '应用';
    updateRgbButton();
    refreshApplyButton();
  }
}

async function revertDongleRgb() {
  const saved = state.rgbSaved;
  if (!saved) return;
  const btn = $('btn-rgb-apply');
  state.busy = true;
  refreshApplyButton();
  btn.disabled = true;
  btn.textContent = '还原中…';
  try {
    const got = await invoke('set_dongle_rgb', saved);
    showRgb(got);
    $('rgb-note').textContent = `已还原为进入时的灯效：${got.modeLabel}。`;
    logLine(`接收器灯效还原：${got.modeLabel} ${got.color1} ${got.color2} ${got.color3}`);
  } catch (e) {
    $('rgb-note').textContent = `还原失败：${e}`;
    toast('err', `${e}`);
  } finally {
    state.busy = false;
    btn.textContent = '应用';
    updateRgbButton();
    refreshApplyButton();
  }
}

// ── advanced settings ────────────────────────────────────────────────────
//
// Long range mode, onboard profiles and the factory reset. The first two are
// written only after the user changes something, and always read back — none
// of them has an undo, and the third has no undo at all.

/**
 * Show the long range toggle only when the device actually answers 0x17.
 *
 * Silence is how the driver decides a device has no such feature, and the
 * official app keeps it off the wired link entirely, so an unanswered probe
 * means "hide it" rather than "show it switched off".
 */
function renderLongRange(info) {
  const card = $('card-longrange');
  card.classList.toggle('hidden', info.longRange === null || info.longRange === undefined);
  if (card.classList.contains('hidden')) return;
  state.longRangeOriginal = !!info.longRange;
  $('in-longrange').checked = info.longRange;
  $('longrange-note').textContent = info.longRange
    ? '当前：已开启（延迟更高，距离更远）。'
    : '当前：关闭（默认，延迟最低）。';
  updateLongRangeButton();
}

function updateLongRangeButton() {
  $('in-longrange').disabled = !state.online || state.busy;
}

/** Show the profile picker only when the device answers 0x0e with an index. */
function renderProfiles(info) {
  const card = $('card-profiles');
  const single = info.profileCount === 0;
  card.classList.toggle('hidden', single);
  if (single) return;
  const select = $('in-profile');
  if (select.options.length !== info.profileCount) {
    select.innerHTML = '';
    for (let i = 0; i < info.profileCount; i++) {
      const opt = document.createElement('option');
      opt.value = String(i);
      opt.textContent = `配置档 ${i + 1}`;
      select.appendChild(opt);
    }
  }
  select.disabled = !state.online;
  state.profileOriginal = info.currentConfig;
  select.value = String(info.currentConfig);
  $('profile-note').textContent = `当前：配置档 ${info.currentConfig + 1}。切换后整套设置一起变。`;
}

/**
 * The toggle applies on change instead of through a separate button: it is a
 * single boolean, so an Apply step would only be a second click for the same
 * effect. It still reads back, and puts the checkbox back where it was when the
 * device refuses.
 */
async function setLongRange() {
  const on = $('in-longrange').checked;
  if (state.busy || on === !!state.longRangeOriginal) return;
  state.busy = true;
  refreshApplyButton();
  const box = $('in-longrange');
  box.disabled = true;
  try {
    const got = await invoke('set_long_range', { on });
    state.longRangeOriginal = got;
    if (got !== on) $('in-longrange').checked = got;
    $('longrange-note').textContent = got
      ? '已开启（延迟更高，距离更远）。'
      : '已关闭（默认，延迟最低）。';
    toast('ok', `长距离模式已${got ? '开启' : '关闭'}`);
    logLine(`长距离模式 → ${got ? '开启' : '关闭'}（设备回读确认）`);
  } catch (e) {
    $('in-longrange').checked = !!state.longRangeOriginal;
    $('longrange-note').textContent = `写入失败：${e}`;
    toast('err', `${e}`);
    logLine(`长距离模式写入失败：${e}`);
  } finally {
    state.busy = false;
    box.disabled = !state.online;
    updateLongRangeButton();
    refreshApplyButton();
  }
}

async function switchProfile() {
  const index = Number($('in-profile').value);
  if (state.busy || index === state.profileOriginal) return;
  state.busy = true;
  refreshApplyButton();
  const select = $('in-profile');
  select.disabled = true;
  try {
    const got = await invoke('set_current_config', { index });
    state.profileOriginal = got;
    select.value = String(got);
    $('profile-note').textContent = `当前：配置档 ${got + 1}。`;
    toast('ok', `已切换到配置档 ${got + 1}`);
    logLine(`配置档切换 → ${got + 1}（设备回读确认）`);
    // DPI stages, colours and every other setting live inside the profile, so
    // everything the other pages are showing is stale now. `loadConfig`
    // re-renders from what the device reports afterwards.
    await loadConfig();
  } catch (e) {
    select.value = String(state.profileOriginal);
    $('profile-note').textContent = `切换失败：${e}`;
    toast('err', `${e}`);
    logLine(`配置档切换失败：${e}`);
  } finally {
    state.busy = false;
    select.disabled = !state.online;
    refreshApplyButton();
  }
}

/**
 * Factory reset behind two confirmations.
 *
 * Everything user-set lives in the device's flash and the protocol has no undo,
 * so a stray click here costs every setting on the mouse. The belt-and-braces
 * is deliberate: one dialog can be clicked through without reading it.
 */
async function factoryReset() {
  if (!state.online) {
    toast('err', '设备离线，无法执行');
    return;
  }
  if (state.busy) return;
  // Two gates, deliberately. One dialog can be clicked straight through; asking
  // for typed text cannot, and this is the only irreversible command in the app.
  const ok = confirm(
    '即将把设备恢复出厂设置。\n\n' +
      '以下所有配置都会被清空：\n' +
      '  · DPI 档位与当前档\n' +
      '  · RGB 灯效颜色\n' +
      '  · 按键防抖与运动同步\n' +
      '  · 休眠策略\n' +
      '  · 长距离模式与板载配置档\n' +
      '  · 接收器指示灯\n\n' +
      '协议里没有撤销命令，也没有备份副本。继续？'
  );
  if (!ok) return;
  const typed = window.prompt('这是不可撤销的操作。请输入「恢复出厂」四个字继续：');
  if (typed === null || typed.trim() !== '恢复出厂') {
    toast('err', '输入不匹配，已取消');
    return;
  }
  state.busy = true;
  refreshApplyButton();
  $('btn-factory-reset').disabled = true;
  try {
    await invoke('factory_reset');
    $('reset-note').textContent = '已清除，设备正在以出厂设置重启。';
    toast('ok', '已恢复出厂设置');
    logLine('恢复出厂：命令已被设备确认接收，闪存已清零');
    // Every page below shows flash contents, so all of it is stale now.
    await loadDevice();
    await loadConfig();
    renderAll();
  } catch (e) {
    $('reset-note').textContent = `执行失败：${e}`;
    toast('err', `${e}`);
    logLine(`恢复出厂失败：${e}`);
  } finally {
    state.busy = false;
    $('btn-factory-reset').disabled = !state.online;
    refreshApplyButton();
  }
}

/**
 * Write the report rate on its own.
 *
 * It deliberately does not go through `apply`: the rate is a two-byte value
 * the device applies immediately, while the rest of the configuration needs
 * four-byte records plus a commit frame. One entry point, one read-back.
 */
async function applyReportRate() {
  const hz = Number($('in-rate').value);
  if (!hz) return;
  state.busy = true;
  refreshApplyButton();
  $('btn-rate-apply').disabled = true;
  $('btn-rate-apply').textContent = '写入中…';
  try {
    const result = await invoke('set_report_rate', { hz });
    const got = result.rateHz;
    toast('ok', got ? `回报率已写入并确认为 ${got} Hz` : `已写入（设备回读 0x${result.byte.toString(16)}）`);
    logLine(`回报率写入 ${hz} Hz：设备回读 0x${result.byte.toString(16)}`);
    await loadConfig();
  } catch (e) {
    toast('err', `${e}`);
    logLine(`回报率写入失败：${e}`);
    await loadConfig();
  } finally {
    state.busy = false;
    $('btn-rate-apply').textContent = '应用';
    renderAll();
    refreshApplyButton();
  }
}

function renderPower() {
  if (!state.snapshot) return;
  const draft = { ...state.snapshot, ...state.draft };
  $('in-sleep').value = String(draft.sleepTimeout ?? 6);
  $('in-mode').value = String(draft.sensorMode ?? 0);
}

// ── actions ─────────────────────────────────────────────────────────────

async function applyChanges() {
  const patch = pending();
  if (Object.keys(patch).length === 0) return;
  state.busy = true;
  refreshApplyButton();
  $('btn-apply').textContent = '正在写入…';
  banner('正在写入设备。写入期间接收器会短暂进入待机，最多需要十几秒读回验证，请不要关闭窗口。');
  try {
    const result = await invoke('apply', { patch });
    adoptSnapshot(result.snapshot);
    state.draft = {};
    renderAll();
    banner('');
    toast('ok', `已写入并验证通过（${result.frames} 帧数据 + 1 帧提交）`);
    logLine(`应用成功：${result.frames} 帧`);
  } catch (e) {
    banner(`${e}`, true);
    toast('err', `${e}`);
    logLine(`应用失败：${e}`);
    // The device may have changed partially; re-read so the panel tells the truth.
    await loadConfig();
  } finally {
    state.busy = false;
    refreshApplyButton();
  }
}

async function restoreStages() {
  const factory = [800, 1600, 2400, 3200, 6400, 13000, 26000];
  if (!confirm(`把 7 档 DPI 恢复为出厂值？\n${factory.join(' / ')}`)) return;
  state.busy = true;
  refreshApplyButton();
  try {
    const result = await invoke('apply', {
      patch: { dpiStages: factory, stageCount: factory.length, activeStage: 1 },
    });
    adoptSnapshot(result.snapshot);
    state.draft = {};
    renderAll();
    toast('ok', 'DPI 档位已恢复出厂值');
    logLine('恢复出厂 DPI 档位成功');
  } catch (e) {
    toast('err', `${e}`);
    logLine(`恢复失败：${e}`);
    await loadConfig();
  } finally {
    state.busy = false;
    refreshApplyButton();
  }
}

async function dumpBlock() {
  try {
    const dump = await invoke('export_block');
    state.lastRaw = dump.hex;
    $('raw-block').textContent = dump.hex.replace(/(.{32})/g, '$1\n');
    logLine('已读取 192 字节配置块');
  } catch (e) {
    toast('err', `${e}`);
  }
}

async function exportConfig() {
  await dumpBlock();
  if (!state.lastRaw) return;
  const blob = new Blob([JSON.stringify({ device: 'P1 Ultra 4K', block: state.lastRaw }, null, 2)], {
    type: 'application/json',
  });
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = `p1studio-config-${Date.now()}.json`;
  a.click();
  URL.revokeObjectURL(url);
  toast('ok', '配置已导出（JSON，含 192 字节原始块）');
}

async function importConfig() {
  const input = document.createElement('input');
  input.type = 'file';
  input.accept = 'application/json,.json,.txt';
  input.addEventListener('change', async () => {
    const file = input.files?.[0];
    if (!file) return;
    try {
      const text = await file.text();
      let hex = text.trim();
      try {
        const parsed = JSON.parse(text);
        if (typeof parsed.block === 'string') hex = parsed.block;
      } catch {
        /* plain hex file */
      }
      if (!confirm('用文件内容覆盖设备配置？此操作会写入整个 192 字节块。')) return;
      const result = await invoke('import_block', { hex });
      adoptSnapshot(result.snapshot);
      state.draft = {};
      renderAll();
      toast('ok', `配置已写入（${result.frames} 帧并验证通过）`);
      logLine('导入配置成功');
    } catch (e) {
      toast('err', `${e}`);
      logLine(`导入失败：${e}`);
    }
  });
  input.click();
}

// ── wiring ──────────────────────────────────────────────────────────────

function switchPage(page) {
  for (const btn of document.querySelectorAll('.nav-item')) {
    btn.classList.toggle('active', btn.dataset.page === page);
  }
  for (const section of document.querySelectorAll('.page')) {
    section.classList.toggle('active', section.id === `page-${page}`);
  }
  const [title, sub] = PAGE_META[page];
  $('page-title').textContent = title;
  $('page-sub').textContent = sub;
}

async function boot() {
  // First, so the visible theme is correct before any device probing starts.
  await initTheme();
  if (!tauri) {
    banner('未检测到本地桥接：请在 Metaphyuni玄熊猫P1管理器 应用内打开本页面。', true);
    return;
  }
  for (const btn of document.querySelectorAll('.nav-item')) {
    btn.addEventListener('click', () => switchPage(btn.dataset.page));
  }
  $('btn-apply').addEventListener('click', applyChanges);
  $('btn-refresh').addEventListener('click', async () => {
    await loadDevice();
    await loadConfig();
    toast('ok', '已重新读取');
  });
  $('btn-ble-refresh').addEventListener('click', () => loadBluetooth(true));
  $('btn-conn-refresh').addEventListener('click', async () => {
    await loadConnection();
    toast('ok', '连接状态已刷新');
  });
  $('btn-pair').addEventListener('click', startPairing);
  $('btn-bt-settings').addEventListener('click', async () => {
    try {
      await invoke('open_bluetooth_settings');
    } catch (e) {
      toast('err', `${e}`);
    }
  });
  $('btn-restore-stages').addEventListener('click', restoreStages);
  $('btn-export').addEventListener('click', exportConfig);
  $('btn-import').addEventListener('click', importConfig);
  $('btn-rate-apply').addEventListener('click', applyReportRate);
  $('btn-rgb-apply').addEventListener('click', applyDongleRgb);
  $('btn-rgb-revert').addEventListener('click', revertDongleRgb);
  $('in-longrange').addEventListener('change', setLongRange);
  $('in-profile').addEventListener('change', switchProfile);
  $('btn-factory-reset').addEventListener('click', factoryReset);
  $('in-rgb-mode').addEventListener('change', () => {
    updateRgbLabels();
    updateRgbButton();
  });
  for (const id of ['in-rgb-1', 'in-rgb-2', 'in-rgb-3']) {
    $(id).addEventListener('input', updateRgbButton);
  }
  $('in-rate').addEventListener('change', updateRateButton);
  $('btn-fw-pick').addEventListener('click', () => $('fw-file').click());
  $('btn-fw-probe').addEventListener('click', probeFirmware);
  $('btn-fw-flash').addEventListener('click', flashFirmware);
  $('fw-file').addEventListener('change', (e) => {
    const file = e.target.files?.[0];
    if (file) inspectFirmware(file);
    // Allow re-picking the same file after an error.
    e.target.value = '';
  });

  $('btn-dump').addEventListener('click', dumpBlock);
  $('btn-copy').addEventListener('click', async () => {
    if (!state.lastRaw) await dumpBlock();
    if (!state.lastRaw) return;
    await navigator.clipboard.writeText(state.lastRaw);
    toast('ok', '已复制 192 字节十六进制');
  });

  $('in-debounce').addEventListener('input', () => {
    $('out-debounce').textContent = `${$('in-debounce').value} ms`;
  });
  $('in-debounce').addEventListener('change', () => {
    edit('debounce', Number($('in-debounce').value));
    renderOverview();
  });
  $('in-motion').addEventListener('change', () => {
    edit('motionSync', $('in-motion').checked);
    renderOverview();
  });
  $('in-ripple').addEventListener('change', () => {
    edit('rippleControl', $('in-ripple').checked);
    renderOverview();
  });
  $('in-angle').addEventListener('change', () => {
    edit('angleSnapping', $('in-angle').checked);
    renderOverview();
  });
  $('in-lod').addEventListener('change', () => {
    edit('lod', Number($('in-lod').value));
  });
  $('in-dpil-mode').addEventListener('change', () => {
    edit('dpiLightMode', Number($('in-dpil-mode').value));
  });
  $('in-dpil-bright').addEventListener('input', () => {
    $('out-dpil-bright').textContent = $('in-dpil-bright').value;
  });
  $('in-dpil-bright').addEventListener('change', () => {
    edit('dpiLightBrightness', Number($('in-dpil-bright').value));
  });
  $('in-dpil-speed').addEventListener('input', () => {
    $('out-dpil-speed').textContent = $('in-dpil-speed').value;
  });
  $('in-dpil-speed').addEventListener('change', () => {
    edit('dpiLightSpeed', Number($('in-dpil-speed').value));
  });

  $('btn-keys-apply').addEventListener('click', () => void applyKeyDraft());
  $('btn-keys-default').addEventListener('click', () => void resetKeyDefaults());
  $('in-sleep').addEventListener('change', () => {
    edit('sleepTimeout', Number($('in-sleep').value));
    renderOverview();
  });
  $('in-mode').addEventListener('change', () => {
    edit('sensorMode', Number($('in-mode').value));
  });

  try {
    state.caps = await invoke('capabilities');
  } catch {
    state.caps = null;
  }

  const modes = state.caps?.sensorModes ?? [
    { value: 0, label: '基本' }, { value: 1, label: '竞技固件' },
  ];
  $('in-mode').innerHTML = modes
    .map((m) => `<option value="${m.value}">${m.value} · ${m.label}</option>`)
    .join('');

  const options = state.caps?.sleepTimeout ?? [3, 6, 12, 18, 30, 60, 90, 120, 180];
  const unit = state.caps?.sleepUnitSeconds ?? 10;
  $('in-sleep').innerHTML = options
    .map((v) => `<option value="${v}">${sleepLabel(v, unit)}</option>`)
    .join('');

  // The window must be usable even when there is nothing to talk to. So boot
  // does the cheap probes first and only pays for the 192-byte configuration
  // read once the mouse has actually answered — otherwise the opening sequence
  // sits in a multi-second wake loop and looks like a failed launch.
  await loadDeviceImages();
  banner('正在检测设备…');
  await loadDevice();
  await loadConnection();
  try {
    const s = await invoke('status', { full: true });
    if (s.online) {
      state.online = true;
      state.missed = 0;
      renderStatus(s);
      await loadConfig();
    } else {
      await markOffline(WAKE_HINT);
    }
  } catch (e) {
    await markOffline(`${e}`);
  }

  schedulePoll();
  setInterval(() => {
    if (!state.busy) refreshApplyButton();
  }, 500);
}

// Keyboard shortcuts: Ctrl+Enter applies, F5 / Ctrl+R re-reads. The apply button
// is the one action worth reaching without the mouse.
window.addEventListener('keydown', (event) => {
  if ((event.ctrlKey || event.metaKey) && event.key === 'Enter') {
    event.preventDefault();
    if (!$('btn-apply').disabled) void applyChanges();
    return;
  }
  if (event.key === 'F5' || ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'r')) {
    event.preventDefault();
    $('btn-refresh').click();
  }
});

window.addEventListener('DOMContentLoaded', boot);
