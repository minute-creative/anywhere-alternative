// Anywhere Camera: adds "Anywhere Camera" and "Anywhere Microphone" to the
// web page's list of cameras and microphones.
//
// Runs in the page's own world (so the website's code sees the change).
// When the website picks one of them, the pictures and sound come from the
// computer you're watching from: the add-on's background part receives them
// from the Anywhere app on this computer, and this script turns them into
// an ordinary camera track (decoding H.264 with the browser's own decoder)
// and microphone track.
(() => {
  if (window.__anywhereCamera) return;
  window.__anywhereCamera = true;
  document.documentElement?.setAttribute("data-anywhere-camera", "1");
  const md = navigator.mediaDevices;
  if (!md || typeof MediaDevices === "undefined") return;

  const CAM_ID = "anywhere-camera";
  const MIC_ID = "anywhere-microphone";
  const GROUP = "anywhere-alternative";
  const W = 1280;
  const H = 720;

  // Counters for troubleshooting (type __anywhereStats in the console).
  const stats = { video: 0, audio: 0, decoded: 0, errors: 0, lastError: "" };
  Object.defineProperty(window, "__anywhereStats", { value: stats });

  const proto = MediaDevices.prototype;
  const origEnumerate = proto.enumerateDevices;
  const origGetUserMedia = proto.getUserMedia;

  // ---- talking to the add-on (through relay.js) --------------------------
  let status = { connected: false, viewer: null, camera: null, problem: null };
  let nextId = 1;
  const answers = new Map();
  function toExt(m) {
    window.postMessage({ anywhereToExt: m }, "*");
  }
  window.addEventListener("message", (e) => {
    if (e.source !== window || !e.data || typeof e.data !== "object" || !e.data.anywhereToPage) return;
    const m = e.data.anywhereToPage;
    if (m.type === "answer" && answers.has(m.id)) {
      answers.get(m.id)(m);
      answers.delete(m.id);
    } else if (m.type === "status") {
      status = m;
      drawPlaceholder();
    } else if (m.type === "media") {
      onMedia(new Uint8Array(m.data));
    }
  });
  function ask(video, audio) {
    const id = nextId++;
    return new Promise((resolve) => {
      answers.set(id, resolve);
      toExt({ type: "start", id, video, audio });
    });
  }

  // ---- the device list ---------------------------------------------------
  function fake(kind, id, label) {
    const base = typeof InputDeviceInfo !== "undefined" ? InputDeviceInfo.prototype : MediaDeviceInfo.prototype;
    const d = Object.create(base);
    const caps =
      kind === "videoinput"
        ? { deviceId: id, groupId: GROUP, width: { min: 1, max: W }, height: { min: 1, max: H }, frameRate: { min: 1, max: 30 }, aspectRatio: { min: 0.5, max: 2 }, facingMode: [], resizeMode: ["none"] }
        : { deviceId: id, groupId: GROUP, channelCount: { min: 1, max: 2 }, sampleRate: { min: 48000, max: 48000 }, sampleSize: { min: 16, max: 16 }, echoCancellation: [false], autoGainControl: [false], noiseSuppression: [false] };
    Object.defineProperties(d, {
      deviceId: { value: id, enumerable: true },
      kind: { value: kind, enumerable: true },
      label: { value: label, enumerable: true },
      groupId: { value: GROUP, enumerable: true },
      toJSON: { value: () => ({ deviceId: id, kind, label, groupId: GROUP }) },
      getCapabilities: { value: () => caps },
    });
    return d;
  }

  proto.enumerateDevices = async function enumerateDevices() {
    const list = await origEnumerate.call(this);
    return [
      ...list,
      fake("videoinput", CAM_ID, "Anywhere Camera"),
      fake("audioinput", MIC_ID, "Anywhere Microphone"),
    ];
  };

  function asks(c, id) {
    if (!c || c === true || typeof c !== "object") return false;
    const d = c.deviceId;
    if (!d) return false;
    const vals = typeof d === "string" ? [d] : Array.isArray(d) ? d : [d.exact, d.ideal].flat().filter(Boolean);
    return vals.includes(id);
  }

  async function hasReal(kind) {
    try {
      const list = await origEnumerate.call(md);
      return list.some((d) => d.kind === kind);
    } catch (_) {
      return true;
    }
  }

  proto.getUserMedia = async function getUserMedia(constraints) {
    const c = constraints ?? {};
    let cam = asks(c.video, CAM_ID);
    let mic = asks(c.audio, MIC_ID);
    // A computer with no camera (a Mac mini) or no microphone: ours is the
    // only one, so it is also the default.
    if (c.video && !cam && !(await hasReal("videoinput"))) cam = true;
    if (c.audio && !mic && !(await hasReal("audioinput"))) mic = true;
    if (!cam && !mic) return origGetUserMedia.call(this, constraints);

    const answer = await ask(cam, mic);
    if (!answer.allowed) {
      const why = answer.stale
        ? "The Anywhere add-on was updated; reload this page."
        : "Not allowed to use Anywhere Camera on this site (change it in the Anywhere add-on).";
      throw new DOMException(why, "NotAllowedError");
    }
    const rest = { ...c };
    if (cam) delete rest.video;
    if (mic) delete rest.audio;
    const tracks = [];
    if (rest.video || rest.audio) {
      const real = await origGetUserMedia.call(this, rest);
      tracks.push(...real.getTracks());
    }
    if (cam) tracks.push(videoTrack());
    if (mic) tracks.push(audioTrack());
    return new MediaStream(tracks);
  };

  // ---- who uses our tracks (the camera turns off when nobody does) -------
  let videoUsers = 0;
  let audioUsers = 0;
  let lastWant = "";
  function updateWant() {
    const w = { video: videoUsers > 0, audio: audioUsers > 0 };
    const key = JSON.stringify(w);
    if (key !== lastWant) {
      lastWant = key;
      toExt({ type: "want", ...w });
    }
  }
  function track(t, kind) {
    let live = true;
    const done = () => {
      if (!live) return;
      live = false;
      if (kind === "video") videoUsers--;
      else audioUsers--;
      updateWant();
    };
    if (kind === "video") videoUsers++;
    else audioUsers++;
    updateWant();
    const stop = t.stop.bind(t);
    t.stop = () => {
      done();
      stop();
    };
    const clone = t.clone.bind(t);
    t.clone = () => track(clone(), kind);
    t.addEventListener("ended", done);
    return t;
  }

  // ---- video ---------------------------------------------------------------
  let source = null; // the one source track per page; each user gets a clone
  let writer = null; // Chrome/Edge: where VideoFrames go
  let canvas = null; // Safari: we draw here and the canvas is the camera
  let ctx = null;
  let decoder = null;
  let waitingForKey = true;
  let lastPicture = 0;
  let placeholderTimer = null;

  function makeSource() {
    if (typeof MediaStreamTrackGenerator === "function") {
      const gen = new MediaStreamTrackGenerator({ kind: "video" });
      writer = gen.writable.getWriter();
      source = gen;
    } else {
      canvas = document.createElement("canvas");
      canvas.width = W;
      canvas.height = H;
      ctx = canvas.getContext("2d");
      source = canvas.captureStream(30).getVideoTracks()[0];
    }
    placeholderTimer = setInterval(drawPlaceholder, 500);
    drawPlaceholder();
  }

  function videoTrack() {
    if (!source) makeSource();
    return track(source.clone(), "video");
  }

  // A card with a plain message while there is no picture yet.
  const card = typeof OffscreenCanvas === "function" ? new OffscreenCanvas(W, H) : null;
  function drawPlaceholder() {
    if (!source || Date.now() - lastPicture < 1500) return;
    let line;
    if (!status.connected) line = "Open Anywhere on this computer";
    else if (status.problem) line = status.problem;
    else if (!status.viewer) line = "Watch this computer from your other computer";
    else line = `Starting the camera on ${status.viewer}…`;
    const target = ctx ?? card?.getContext("2d");
    if (!target) return;
    target.fillStyle = "#1f2937";
    target.fillRect(0, 0, W, H);
    target.fillStyle = "#ffffff";
    target.textAlign = "center";
    target.font = "600 44px -apple-system, Segoe UI, sans-serif";
    target.fillText("Anywhere Camera", W / 2, H / 2 - 30);
    target.font = "28px -apple-system, Segoe UI, sans-serif";
    target.fillStyle = "#d1d5db";
    target.fillText(line.length > 80 ? `${line.slice(0, 77)}…` : line, W / 2, H / 2 + 30);
    if (writer && card) {
      const f = new VideoFrame(card, { timestamp: performance.now() * 1000 });
      writer.write(f).catch(() => {});
    }
  }

  // Ask the other computer for a full picture, at most twice a second.
  let lastKeyAsk = 0;
  function askKeyframe() {
    if (Date.now() - lastKeyAsk > 500) {
      lastKeyAsk = Date.now();
      toExt({ type: "keyframe" });
    }
  }

  // Can this browser decode the camera's H.264? (Chrome, Edge and Safari
  // can; some open-source builds of Chromium can't.)
  let supported = null;
  if (typeof VideoDecoder === "function") {
    VideoDecoder.isConfigSupported({ codec: "avc1.42E01F" })
      .then((r) => (supported = Boolean(r.supported)))
      .catch(() => (supported = false));
  } else {
    supported = false;
  }

  function ensureDecoder() {
    if (decoder && decoder.state !== "closed") return true;
    if (supported !== true) {
      if (supported === false) status = { ...status, problem: "This browser can't play the camera's video (H.264). Use Chrome, Edge or Safari." };
      return false;
    }
    decoder = new VideoDecoder({
      output: (frame) => {
        stats.decoded++;
        lastPicture = Date.now();
        if (writer) {
          writer.write(frame).catch(() => frame.close());
        } else if (ctx) {
          ctx.drawImage(frame, 0, 0, W, H);
          frame.close();
        } else {
          frame.close();
        }
      },
      error: (e) => {
        stats.errors++;
        stats.lastError = String(e?.message ?? e);
        decoder = null;
        waitingForKey = true;
        askKeyframe();
      },
    });
    decoder.configure({ codec: "avc1.42E01F", optimizeForLatency: true });
    return true;
  }

  // ---- sound ---------------------------------------------------------------
  let audioSource = null;
  let audioWriter = null;
  let ring = null; // Safari: samples waiting for the speaker-less player
  let ringRead = 0;
  let ringWrite = 0;

  function makeAudio() {
    if (typeof MediaStreamTrackGenerator === "function" && typeof AudioData === "function") {
      const gen = new MediaStreamTrackGenerator({ kind: "audio" });
      audioWriter = gen.writable.getWriter();
      audioSource = gen;
      return;
    }
    const ac = new AudioContext({ sampleRate: 48000 });
    ring = new Float32Array(48000 * 2); // one second, stereo
    const node = ac.createScriptProcessor(1024, 0, 2);
    node.onaudioprocess = (e) => {
      const l = e.outputBuffer.getChannelData(0);
      const r = e.outputBuffer.getChannelData(1);
      for (let i = 0; i < l.length; i++) {
        if (ringRead !== ringWrite) {
          l[i] = ring[ringRead];
          r[i] = ring[ringRead + 1];
          ringRead = (ringRead + 2) % ring.length;
        } else {
          l[i] = 0;
          r[i] = 0;
        }
      }
    };
    const dest = ac.createMediaStreamDestination();
    node.connect(dest);
    audioSource = dest.stream.getAudioTracks()[0];
    const resume = () => ac.resume();
    resume();
    document.addEventListener("click", resume, { once: true, capture: true });
  }

  function audioTrack() {
    if (!audioSource) makeAudio();
    return track(audioSource.clone(), "audio");
  }

  // ---- incoming pictures and sound ----------------------------------------
  function onMedia(bytes) {
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const kind = bytes[0];
    const key = (bytes[1] & 1) === 1;
    const ts = Number(view.getBigUint64(2));
    const payload = bytes.subarray(10);
    if (kind === 1) {
      stats.video++;
      if (!source || !ensureDecoder()) return;
      if (waitingForKey && !key) return;
      if (decoder.decodeQueueSize > 4) {
        // Falling behind: skip to the next full picture.
        waitingForKey = true;
        askKeyframe();
        return;
      }
      waitingForKey = false;
      try {
        decoder.decode(new EncodedVideoChunk({ type: key ? "key" : "delta", timestamp: ts, data: payload }));
      } catch (_) {
        waitingForKey = true;
        askKeyframe();
      }
    } else if (kind === 2) {
      stats.audio++;
      if (audioWriter) {
        const frames = payload.byteLength / 4;
        const data = new AudioData({ format: "s16", sampleRate: 48000, numberOfFrames: frames, numberOfChannels: 2, timestamp: ts, data: payload });
        audioWriter.write(data).catch(() => data.close());
      } else if (ring) {
        const s = new Int16Array(payload.buffer.slice(payload.byteOffset, payload.byteOffset + payload.byteLength));
        for (let i = 0; i < s.length; i++) {
          ring[ringWrite] = s[i] / 32768;
          ringWrite = (ringWrite + 1) % ring.length;
        }
      }
    }
  }
})();
