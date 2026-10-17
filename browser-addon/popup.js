// The add-on's button: what's connected, and which sites may use it.
const api = globalThis.browser ?? globalThis.chrome;

async function show() {
  const s = await api.runtime.sendMessage({ type: "popup-status" }).catch(() => null);
  const state = document.getElementById("state");
  if (!s || s.inUse === 0) {
    state.textContent = "Not in use right now.";
  } else if (!s.connected) {
    state.textContent = "Anywhere isn't running on this computer. Open it, then try again.";
  } else if (s.problem) {
    state.textContent = s.problem;
  } else if (s.camera) {
    state.textContent = `Showing ${s.camera} from ${s.viewer}.`;
  } else if (s.viewer) {
    state.textContent = `Connected to ${s.viewer}.`;
  } else {
    state.textContent = "Watch this computer from your other computer to use its camera.";
  }

  const got = await api.storage.local.get("sites");
  const sites = got.sites ?? {};
  const list = document.getElementById("sites");
  list.textContent = "";
  const names = Object.keys(sites).sort();
  if (names.length === 0) {
    const li = document.createElement("li");
    li.textContent = "None yet. Sites ask the first time.";
    list.appendChild(li);
  }
  for (const origin of names) {
    const li = document.createElement("li");
    const label = document.createElement("span");
    label.textContent = `${origin.replace(/^https?:\/\//, "")}: ${sites[origin] === "allow" ? "allowed" : "blocked"}`;
    const forget = document.createElement("button");
    forget.textContent = "Forget";
    forget.addEventListener("click", async () => {
      delete sites[origin];
      await api.storage.local.set({ sites });
      show();
    });
    li.append(label, forget);
    list.appendChild(li);
  }
}
show();
