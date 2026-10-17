// Anywhere Camera: the add-on's background part.
//
// Web pages can't open a connection to the Anywhere app on this computer by
// themselves (the app only accepts connections that come from this add-on).
// So this script holds the one connection, asks you once per website whether
// it may use the camera and microphone from your other computer, and passes
// pictures and sound to the tabs that are allowed and asked for them.

const api = globalThis.browser ?? globalThis.chrome;
const BRIDGE = "ws://127.0.0.1:7721";

let ws = null;
let connected = false;
let status = { viewer: null, camera: null, problem: null };
let lastWant = "";
// Each tab (or frame) that uses our devices: port → what it wants.
const users = new Map();
// Open "may this site use…" questions: id → resolve(answer).
const questions = new Map();
let nextQuestion = 1;

function toBase64(bytes) {
  let s = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    s += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
  }
  return btoa(s);
}

function want() {
  let video = false;
  let audio = false;
  for (const u of users.values()) {
    video ||= u.video;
    audio ||= u.audio;
  }
  return { video, audio };
}

function sendWant() {
  const w = want();
  const key = JSON.stringify(w);
  if (connected && key !== lastWant) {
    ws.send(JSON.stringify({ type: "want", ...w }));
    lastWant = key;
  }
}

function statusMessage() {
  return { type: "status", connected, ...status };
}

function broadcastStatus() {
  const m = statusMessage();
  for (const port of users.keys()) {
    try {
      port.postMessage(m);
    } catch (_) {
      /* tab went away */
    }
  }
}

function connect() {
  if (ws || users.size === 0) return;
  try {
    ws = new WebSocket(BRIDGE);
  } catch (_) {
    ws = null;
    setTimeout(connect, 3000);
    return;
  }
  ws.binaryType = "arraybuffer";
  ws.onopen = () => {
    connected = true;
    lastWant = "";
    sendWant();
    broadcastStatus();
  };
  ws.onmessage = (ev) => {
    if (typeof ev.data === "string") {
      try {
        const m = JSON.parse(ev.data);
        if (m.type === "status") {
          status = { viewer: m.viewer ?? null, camera: m.camera ?? null, problem: m.problem ?? null };
          broadcastStatus();
        }
      } catch (_) {
        /* ignore */
      }
      return;
    }
    const bytes = new Uint8Array(ev.data);
    const isVideo = bytes[0] === 1;
    let data = null;
    for (const [port, u] of users) {
      if (isVideo ? u.video : u.audio) {
        data ??= toBase64(bytes);
        try {
          port.postMessage({ type: "media", data });
        } catch (_) {
          /* tab went away */
        }
      }
    }
  };
  ws.onclose = () => {
    ws = null;
    const was = connected;
    connected = false;
    if (was) broadcastStatus();
    if (users.size > 0) setTimeout(connect, 2000);
  };
  ws.onerror = () => {};
}

function originOf(port) {
  const s = port.sender ?? {};
  if (s.origin && s.origin !== "null") return s.origin;
  try {
    return new URL(s.url).origin;
  } catch (_) {
    return "";
  }
}

async function sites() {
  const got = await api.storage.local.get("sites");
  return got.sites ?? {};
}

// Ask once per website, in a small window of the add-on's own (the page
// can't click it for you).
async function allowed(origin) {
  const known = (await sites())[origin];
  if (known === "allow") return true;
  if (known === "deny") return false;
  const id = nextQuestion++;
  const answer = new Promise((resolve) => questions.set(id, resolve));
  let win = null;
  try {
    win = await api.windows.create({
      url: api.runtime.getURL(`ask.html?id=${id}&origin=${encodeURIComponent(origin)}`),
      type: "popup",
      width: 460,
      height: 300,
    });
  } catch (_) {
    questions.delete(id);
    return false;
  }
  const closed = (wid) => {
    if (win && wid === win.id && questions.has(id)) {
      questions.get(id)(false);
      questions.delete(id);
    }
  };
  api.windows.onRemoved.addListener(closed);
  const ok = await answer;
  api.windows.onRemoved.removeListener(closed);
  return ok;
}

api.runtime.onMessage.addListener((m, _sender, reply) => {
  if (m?.type === "answer" && questions.has(m.id)) {
    questions.get(m.id)(Boolean(m.allow));
    questions.delete(m.id);
    if (m.remember) {
      sites().then((s) => {
        s[m.origin] = m.allow ? "allow" : "deny";
        api.storage.local.set({ sites: s });
      });
    }
    reply({ ok: true });
  } else if (m?.type === "popup-status") {
    reply({ ...statusMessage(), inUse: users.size });
  }
  return false;
});

api.runtime.onConnect.addListener((port) => {
  if (port.name !== "anywhere") return;
  const origin = originOf(port);
  port.onMessage.addListener(async (m) => {
    if (m.type === "start") {
      const ok = origin.startsWith("https://") || origin.startsWith("http://localhost") || origin.startsWith("http://127.0.0.1")
        ? await allowed(origin)
        : false;
      if (ok) {
        if (!users.has(port)) users.set(port, { video: false, audio: false });
        connect();
      }
      try {
        port.postMessage({ type: "answer", id: m.id, allowed: ok });
        if (ok) port.postMessage(statusMessage());
      } catch (_) {
        /* tab went away */
      }
    } else if (m.type === "want" && users.has(port)) {
      users.set(port, { video: Boolean(m.video), audio: Boolean(m.audio) });
      sendWant();
    } else if (m.type === "keyframe" && connected) {
      ws.send(JSON.stringify({ type: "keyframe" }));
    }
  });
  port.onDisconnect.addListener(() => {
    users.delete(port);
    sendWant();
    if (users.size === 0 && ws) ws.close();
  });
});
