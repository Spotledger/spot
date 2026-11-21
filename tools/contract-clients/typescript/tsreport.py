"""The generation report: nothing is translated approximately or dropped
silently (tools/contract-clients/README.md)."""

from __future__ import annotations


class Report:
    def __init__(self) -> None:
        self.approx: set[str] = set()
        self.dropped: set[str] = set()
        self.unrepresentable: set[str] = set()
        self.findings: set[str] = set()
        self.counts: dict[str, int] = {}

    def approximate(self, where: str, what: str) -> None:
        self.approx.add(f"APPROXIMATE {where}: {what}")

    def drop(self, where: str, what: str) -> None:
        self.dropped.add(f"DROPPED {where}: {what}")

    def refuse(self, where: str) -> None:
        self.unrepresentable.add(f"UNREPRESENTABLE {where}")

    def finding(self, text: str) -> None:
        self.findings.add(f"FINDING {text}")

    @property
    def failed(self) -> bool:
        return bool(self.unrepresentable or self.findings)

    def text(self) -> str:
        lines = ["contract client generator (typescript) report"]
        for key in sorted(self.counts):
            lines.append(f"count {key}: {self.counts[key]}")
        for bucket in (self.approx, self.dropped, self.unrepresentable, self.findings):
            lines.extend(sorted(bucket))
        lines.append(
            f"summary: {len(self.approx)} approximate, {len(self.dropped)} dropped, "
            f"{len(self.unrepresentable)} unrepresentable, {len(self.findings)} findings"
        )
        return "\n".join(lines) + "\n"
