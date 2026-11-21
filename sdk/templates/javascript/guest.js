// The sample app guest in JavaScript (jco componentize / ComponentizeJS), WP-STR-07.T1.
//
// The world is wit/world.wit (a copy of crates/framework/spot-wasm/wit/test-world.wit). The WIT
// export `on-event` becomes the ES module export `onEvent`; the WIT `result<string, error>` is
// the ComponentizeJS tagged form `{ tag: 'ok', val }` / `{ tag: 'err', val }`.
//
// The behaviour is the sample app reduced to one DocType, one lifecycle step and one
// script-callable function. Hours are parsed to hundredths and formatted back in integer
// arithmetic (ADR-12: no floating point for quantities).

// The bookable hours range, in hundredths (0.25 to 24.00).
const HOURS_MIN = 25;
const HOURS_MAX = 2400;

export function onEvent(event) {
  const request = JSON.parse(event);
  const op = request.op ?? "";
  const name = request.name ?? "";
  if (op === "step" && name === "sample:check_hours") {
    return checkHours(request.hours ?? "");
  }
  if (op === "call" && name === "sample:double_hours") {
    return doubleHours(request.hours ?? "");
  }
  return refusal("sample.unknown_op", `no operation ${op}:${name}`);
}

function checkHours(hours) {
  const value = hundredths(hours);
  if (value === null) {
    return refusal("sample.bad_hours", "hours must be a decimal string");
  }
  if (value < HOURS_MIN || value > HOURS_MAX) {
    return refusal("sample.hours_out_of_range", "Hours must be between 0.25 and 24.");
  }
  return { tag: "ok", val: '{"ok":true,"step":"sample:check_hours"}' };
}

function doubleHours(hours) {
  const value = hundredths(hours);
  if (value === null) {
    return refusal("sample.bad_hours", "hours must be a decimal string");
  }
  return { tag: "ok", val: JSON.stringify({ value: formatHours(value * 2) }) };
}

function refusal(code, message) {
  return { tag: "err", val: { code, message } };
}

// A non-negative decimal with at most two places, as hundredths, or null.
function hundredths(text) {
  const match = /^([0-9]+)(?:\.([0-9]{1,2}))?$/.exec(text);
  if (match === null) {
    return null;
  }
  const fraction = (match[2] ?? "").padEnd(2, "0");
  return Number(match[1]) * 100 + Number(fraction);
}

// Hundredths as a canonical two-place decimal.
function formatHours(hundredths) {
  const whole = Math.trunc(hundredths / 100);
  const fraction = String(hundredths % 100).padStart(2, "0");
  return `${whole}.${fraction}`;
}
