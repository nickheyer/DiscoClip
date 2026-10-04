// Runs in every document and worker of the captured page before any of its own scripts.
// Copies every buffer the page's player appends to a SourceBuffer out to the engine
// through the __discoclip binding, drives playback to the end as fast as the browser
// plays, and reports where playback stands four times a second.
(() => {
  const g = globalThis;
  if (g.__discoclipHooked) return;
  g.__discoclipHooked = true;
  const post = (message) => {
    try {
      g.__discoclip(JSON.stringify(message));
    } catch (e) {}
  };
  const CHUNK = 512 * 1024;
  const RATE = 16;
  const MS = g.MediaSource;
  if (!MS || !g.SourceBuffer) {
    post({ t: 'nomse' });
    return;
  }
  let nextId = 1;
  const ids = new WeakMap();
  const owners = new WeakMap();
  const base64 = (bytes) => {
    let text = '';
    for (let i = 0; i < bytes.length; i += 0x8000) {
      text += String.fromCharCode.apply(null, bytes.subarray(i, Math.min(i + 0x8000, bytes.length)));
    }
    return btoa(text);
  };
  const addSourceBuffer = MS.prototype.addSourceBuffer;
  MS.prototype.addSourceBuffer = function (mime) {
    const buffer = addSourceBuffer.call(this, mime);
    const id = nextId++;
    ids.set(buffer, id);
    owners.set(buffer, this);
    post({ t: 'open', sb: id, mime: String(mime) });
    return buffer;
  };
  const endOfStream = MS.prototype.endOfStream;
  MS.prototype.endOfStream = function (...args) {
    post({ t: 'eos' });
    return endOfStream.apply(this, args);
  };
  const SB = g.SourceBuffer;
  const appendBuffer = SB.prototype.appendBuffer;
  SB.prototype.appendBuffer = function (data) {
    const id = ids.get(this);
    if (id !== undefined && data) {
      const bytes =
        data instanceof ArrayBuffer
          ? new Uint8Array(data)
          : new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
      for (let offset = 0; offset < bytes.length; offset += CHUNK) {
        post({ t: 'data', sb: id, b: base64(bytes.subarray(offset, Math.min(offset + CHUNK, bytes.length))) });
      }
      post({ t: 'append', sb: id, n: bytes.length });
    }
    return appendBuffer.call(this, data);
  };
  const abort = SB.prototype.abort;
  SB.prototype.abort = function () {
    const id = ids.get(this);
    if (id !== undefined) post({ t: 'abort', sb: id });
    return abort.call(this);
  };
  const changeType = SB.prototype.changeType;
  if (changeType) {
    SB.prototype.changeType = function (mime) {
      const id = ids.get(this);
      if (id !== undefined) post({ t: 'type', sb: id, mime: String(mime) });
      return changeType.call(this, mime);
    };
  }
  if (typeof document === 'undefined') return;

  // The media sources behind blob URLs, and the shadow roots players hide their
  // elements in.
  const blobs = new Map();
  const createObjectURL = URL.createObjectURL;
  URL.createObjectURL = function (object) {
    const url = createObjectURL.call(this, object);
    if (object instanceof MS) blobs.set(url, object);
    return url;
  };
  const roots = new Set();
  const attachShadow = Element.prototype.attachShadow;
  Element.prototype.attachShadow = function (init) {
    const root = attachShadow.call(this, init);
    roots.add(root);
    return root;
  };
  const sourceOf = (el) => {
    try {
      if (el.srcObject) {
        if (el.srcObject instanceof MS) return el.srcObject;
        if (g.MediaSourceHandle && el.srcObject instanceof g.MediaSourceHandle) return { readyState: 'open' };
      }
      const src = el.currentSrc || el.src || '';
      if (blobs.has(src)) return blobs.get(src);
      if (src.startsWith('blob:')) return { readyState: 'open' };
    } catch (e) {}
    return null;
  };
  const mediaElements = () => {
    const found = [];
    const scopes = [document, ...roots];
    for (const scope of scopes) {
      try {
        for (const el of scope.querySelectorAll('video, audio')) found.push(el);
      } catch (e) {}
    }
    return found;
  };
  const drive = (el) => {
    try {
      el.muted = true;
      el.defaultMuted = true;
    } catch (e) {}
    try {
      if (el.playbackRate !== RATE) el.playbackRate = RATE;
      if (el.defaultPlaybackRate !== RATE) el.defaultPlaybackRate = RATE;
    } catch (e) {}
    try {
      if (el.paused && !el.ended && el.readyState >= 1) {
        const playing = el.play();
        if (playing && playing.catch) playing.catch(() => {});
      }
    } catch (e) {}
  };
  setInterval(() => {
    const els = [];
    for (const el of mediaElements()) {
      const source = sourceOf(el);
      if (source) drive(el);
      const buffered = [];
      try {
        for (let i = 0; i < el.buffered.length; i++) buffered.push([el.buffered.start(i), el.buffered.end(i)]);
      } catch (e) {}
      els.push({
        mse: !!source,
        cur: el.currentTime,
        dur: el.duration,
        ended: el.ended,
        paused: el.paused,
        rs: el.readyState,
        ms: source ? source.readyState : null,
        buf: buffered,
      });
    }
    post({ t: 'tick', els });
  }, 250);

  // Starts a player that waits to be clicked: the play controls and the media elements
  // themselves are clicked, and every media element is asked to play.
  g.__discoclipNudge = () => {
    const clicked = [];
    const scopes = [document, ...roots];
    for (const scope of scopes) {
      try {
        for (const el of scope.querySelectorAll(
          'video, audio, button, [role="button"], [aria-label*="play" i], [title*="play" i], [class*="play" i], [id*="play" i]'
        )) {
          const label = ((el.getAttribute && (el.getAttribute('aria-label') || el.getAttribute('title'))) || el.className || el.id || '').toString();
          const isMedia = el.tagName === 'VIDEO' || el.tagName === 'AUDIO';
          if (!isMedia && !/play/i.test(label)) continue;
          if (/playlist|player-settings|display/i.test(label) && !isMedia) continue;
          clicked.push(el);
        }
      } catch (e) {}
    }
    for (const el of clicked) {
      try {
        if (el.scrollIntoView) el.scrollIntoView({ block: 'center' });
      } catch (e) {}
      try {
        el.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true, view: g }));
      } catch (e) {}
    }
    for (const el of mediaElements()) {
      try {
        el.muted = true;
        const playing = el.play();
        if (playing && playing.catch) playing.catch(() => {});
      } catch (e) {}
    }
    return clicked.length;
  };
})();
