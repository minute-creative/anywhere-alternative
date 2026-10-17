// Anywhere Camera: the go-between inside each page.
//
// The part that adds "Anywhere Camera" to the page (page.js) runs in the
// page's own world, where it can change what the website sees but can't
// talk to the add-on. This script runs beside it in the add-on's world and
// passes messages both ways.

(() => {
  const api = globalThis.browser ?? globalThis.chrome;
  let port = null;

  // Safari can't place page.js in the page's world from the manifest;
  // add it as a script the page loads (it marks the page when it runs).
  if (!document.documentElement?.hasAttribute("data-anywhere-camera")) {
    const s = document.createElement("script");
    s.src = api.runtime.getURL("page.js");
    s.onload = () => s.remove();
    (document.head || document.documentElement).appendChild(s);
  }

  function fromBase64(b64) {
    const s = atob(b64);
    const out = new Uint8Array(s.length);
    for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i);
    return out;
  }

  function toPage(m, transfer) {
    window.postMessage({ anywhereToPage: m }, "*", transfer ?? []);
  }

  function ensurePort() {
    if (port) return port;
    port = api.runtime.connect({ name: "anywhere" });
    port.onMessage.addListener((m) => {
      if (m.type === "media") {
        const bytes = fromBase64(m.data);
        toPage({ type: "media", data: bytes.buffer }, [bytes.buffer]);
      } else {
        toPage(m);
      }
    });
    port.onDisconnect.addListener(() => {
      port = null;
      toPage({ type: "status", connected: false, viewer: null, camera: null, problem: null });
    });
    return port;
  }

  window.addEventListener("message", (e) => {
    if (e.source !== window || !e.data || typeof e.data !== "object" || !e.data.anywhereToExt) return;
    const m = e.data.anywhereToExt;
    if (!["start", "want", "keyframe"].includes(m.type)) return;
    try {
      ensurePort().postMessage(m);
    } catch (_) {
      // The add-on was updated or reloaded: this page needs a reload.
      port = null;
      if (m.type === "start") toPage({ type: "answer", id: m.id, allowed: false, stale: true });
    }
  });
})();
