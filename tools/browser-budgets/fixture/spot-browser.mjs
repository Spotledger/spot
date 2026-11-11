// Fixture shell for `run.sh --self-test` and for `--quick` on a machine with no
// published browser build.
//
// It implements exactly the page contract clients/avalonia/docs/browser-publish.md
// documents (`ready`, `openForm`, `openList`, `metrics()`), nothing else: the
// driver cannot tell it apart from the real shell, which is the point. The
// numbers it produces are a real measurement of this fixture page and are
// marked `fixture: true` in every result; they are never evidence for a budget.
//
// The 25-byte module below is `(module (memory (export "memory") 2))`; it gives
// the driver a real WebAssembly linear memory to count in the memory measure.

const marks = { moduleLoaded: performance.now() };
marks.scriptStart = performance.now();

const WASM = Uint8Array.from([
  0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
  0x05, 0x03, 0x01, 0x00, 0x02,
  0x07, 0x0a, 0x01, 0x06, 0x6d, 0x65, 0x6d, 0x6f, 0x72, 0x79, 0x02, 0x00,
]);

const tabsElement = document.getElementById('tabs');
const rowsElement = document.getElementById('rows');
const listElement = document.getElementById('list');
const field = document.getElementById('field');
const status = document.getElementById('status');

const counters = { openForm: 0, openList: 0 };
const marksUs = { dotnetStart: 0, firstPaint: 0, ready: 0 };

const ROWS = 400;
const page = document.createDocumentFragment();
for (let index = 1; index <= ROWS; index += 1) {
  const row = document.createElement('tr');
  const amount = String(index * 125).padStart(7, '0');
  row.innerHTML = `<td>${index}</td><td>CODE-${String(index).padStart(4, '0')}</td>`
    + `<td>Fixture item ${index}</td><td>${index % 7 + 1}</td><td>125.00</td><td>${amount}</td>`;
  page.appendChild(row);
}
rowsElement.appendChild(page);

document.addEventListener('click', (event) => {
  if (listElement.contains(event.target)) listElement.focus();
}, true);

const ready = new Promise((resolve) => {
  const finish = () => {
    marks.runtimeCreated = performance.now();
    marks.ready = performance.now();
    marksUs.ready = Math.round(marks.ready * 1000);
    status.textContent = 'fixture: ready';
    resolve();
  };
  requestAnimationFrame(() => requestAnimationFrame(finish));
});

function openTab(title) {
  for (const tab of tabsElement.children) tab.classList.remove('active');
  const element = document.createElement('div');
  element.className = 'tab active';
  element.textContent = title;
  tabsElement.appendChild(element);
  while (tabsElement.children.length > 12) tabsElement.removeChild(tabsElement.firstChild);
  status.textContent = `fixture: ${tabsElement.children.length} tab(s), ${title} active`;
  return true;
}

window.spotBrowser = {
  ready,
  async openForm(doctype, id) {
    await ready;
    counters.openForm += 1;
    return openTab(`${doctype} ${id}`);
  },
  async openList(doctype) {
    await ready;
    counters.openList += 1;
    return openTab(doctype);
  },
  metrics() {
    return JSON.stringify({
      schema: 1,
      ready: true,
      marksUs,
      counters,
      labels: { calc: 'managed', storage: 'available' },
      js: { marksMs: { ...marks } },
    });
  },
};

// A real linear memory, instantiated after the shell is up, so `wasmBytes()` in
// the driver's probe has something to count.
const module = new WebAssembly.Module(WASM);
const instance = new WebAssembly.Instance(module, {});
instance.exports.memory.grow(1);

field.addEventListener('keydown', () => { field.dataset.keys = String(Number(field.dataset.keys || 0) + 1); });
