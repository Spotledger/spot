"""The sample app guest in Python (componentize-py, WP-STR-07.T1).

The world is `wit/world.wit` (a copy of `crates/framework/spot-wasm/wit/test-world.wit`). The
generated bindings module `wit_world` (produced by `componentize-py ... bindings`) exposes the
`WitWorld` protocol: one method, `on_event(event: str) -> str`, that raises
`componentize_py_types.Err(wit_world.Error)` for the world's `result<string, error>`.

The behaviour is the sample app reduced to one DocType, one lifecycle step and one
script-callable function. Hours are parsed to hundredths and formatted back in integer
arithmetic (ADR-12: no floating point for quantities).
"""

import json

import wit_world
from componentize_py_types import Err
from wit_world import Error

# The bookable hours range, in hundredths (0.25 to 24.00).
HOURS_MIN = 25
HOURS_MAX = 2400


def _hundredths(hours: str) -> int:
    """A non-negative decimal with at most two places, as hundredths."""
    whole, _, fraction = hours.partition(".")
    if not whole.isdigit() or len(fraction) > 2 or (fraction and not fraction.isdigit()):
        raise ValueError(hours)
    return int(whole) * 100 + int((fraction + "00")[:2])


def _format_hours(hundredths: int) -> str:
    """Hundredths as a canonical two-place decimal."""
    return f"{hundredths // 100}.{hundredths % 100:02d}"


class WitWorld(wit_world.WitWorld):
    """The world's single export, `on-event`.

    componentize-py looks for a class named after the world (`test-world` -> `WitWorld`) in the
    module it is handed.
    """

    def on_event(self, event: str) -> str:
        request = json.loads(event)
        op = request.get("op", "")
        name = request.get("name", "")
        if op == "step" and name == "sample:check_hours":
            return self._check_hours(request.get("hours", ""))
        if op == "call" and name == "sample:double_hours":
            return self._double_hours(request.get("hours", ""))
        raise Err(Error(code="sample.unknown_op", message=f"no operation {op}:{name}"))

    def _check_hours(self, hours: str) -> str:
        try:
            value = _hundredths(hours)
        except ValueError:
            raise Err(
                Error(code="sample.bad_hours", message="hours must be a decimal string")
            ) from None
        if value < HOURS_MIN or value > HOURS_MAX:
            raise Err(
                Error(
                    code="sample.hours_out_of_range",
                    message="Hours must be between 0.25 and 24.",
                )
            )
        return '{"ok":true,"step":"sample:check_hours"}'

    def _double_hours(self, hours: str) -> str:
        try:
            value = _hundredths(hours)
        except ValueError:
            raise Err(
                Error(code="sample.bad_hours", message="hours must be a decimal string")
            ) from None
        return json.dumps({"value": _format_hours(value * 2)}, separators=(",", ":"))
