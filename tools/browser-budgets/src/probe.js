// Injected page instrumentation for the browser budget driver.
//
// Runs in every frame before the page's own scripts (Playwright
// `addInitScript`), so it needs no cooperation from the build and adds no hook
// to the `window.spotBrowser` contract. It exposes `window.__spotProbe`:
//
//   framesStart() / framesStop()   animation-frame intervals in ms (the scroll measure)
//   keysStart()   / keysStop()     key down to the end of the second painted frame
//   wasmBytes()                    sum of the WebAssembly linear memories in bytes
//
// The method (two-frame latency, frame interval probing, Wasm memory size) is
// the one SPK-07 used; this file is independent of the lab.
(() => {
  if (globalThis.__spotProbe) return;

  const frames = [];
  let recording = false;
  let last = 0;

  const tick = (timestamp) => {
    if (!recording) return;
    if (last) frames.push(timestamp - last);
    last = timestamp;
    requestAnimationFrame(tick);
  };

  const memories = new Set();
  const remember = (instance) => {
    try {
      const memory = instance && instance.exports && instance.exports.memory;
      if (memory && memory.buffer) memories.add(memory);
    } catch {
      /* a module without an exported memory is not a Wasm memory consumer */
    }
  };

  // Count every linear memory the page creates, however it is instantiated.
  for (const name of ['instantiate', 'instantiateStreaming']) {
    const original = WebAssembly[name];
    if (typeof original !== 'function') continue;
    WebAssembly[name] = async function patched(...args) {
      const result = await original.apply(WebAssembly, args);
      remember(result && result.instance ? result.instance : result);
      return result;
    };
  }
  const Instance = WebAssembly.Instance;
  if (typeof Instance === 'function') {
    WebAssembly.Instance = new Proxy(Instance, {
      construct(target, args) {
        const instance = new target(...args);
        remember(instance);
        return instance;
      },
    });
  }

  const keys = [];
  let keying = false;
  globalThis.addEventListener('keydown', () => {
    if (!keying) return;
    const started = performance.now();
    requestAnimationFrame(() => {
      requestAnimationFrame(() => {
        if (keying) keys.push(performance.now() - started);
      });
    });
  }, true);

  globalThis.__spotProbe = {
    framesStart() {
      frames.length = 0;
      last = 0;
      recording = true;
      requestAnimationFrame(tick);
    },
    framesStop() {
      recording = false;
      return frames.slice();
    },
    keysStart() {
      keys.length = 0;
      keying = true;
    },
    keysStop() {
      keying = false;
      return keys.slice();
    },
    wasmBytes() {
      let total = 0;
      for (const memory of memories) total += memory.buffer.byteLength;
      return total;
    },
  };

  // Wait for `count` painted frames (two is SPK-07's "the change was drawn").
  globalThis.__spotFrames = (count) => new Promise((resolve) => {
    let seen = 0;
    const step = () => {
      seen += 1;
      if (seen >= count) resolve();
      else requestAnimationFrame(step);
    };
    requestAnimationFrame(step);
  });
})();
