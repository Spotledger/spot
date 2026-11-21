# ADR-900: Keep the test fixture tiny

## Why

The docsite tests need a contract tree with the real layout but almost no
content, so the suite stays fast and each assertion names one rule.