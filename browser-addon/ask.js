// The "may this site use Anywhere Camera?" window.
const api = globalThis.browser ?? globalThis.chrome;
const q = new URLSearchParams(location.search);
const id = Number(q.get("id"));
const origin = q.get("origin") ?? "";
document.getElementById("site").textContent = origin.replace(/^https?:\/\//, "");

function answer(allow) {
  const remember = document.getElementById("remember").checked;
  api.runtime.sendMessage({ type: "answer", id, origin, allow, remember }).finally(() => window.close());
}
document.getElementById("yes").addEventListener("click", () => answer(true));
document.getElementById("no").addEventListener("click", () => answer(false));
