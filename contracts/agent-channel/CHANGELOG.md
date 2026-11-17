# Changelog: agent channel

Every change is an ADR plus a semantic version bump. Additive changes bump the minor version and keep the `/agent/v1` prefix.

## 0.1.0 (draft, awaiting architecture-owner approval)

First version (WP-CON-03.T62; the architecture owner gave the lead authority over contract changes on 31 Oct 2026 and for 12 Nov 2026). Five operations and eleven `agent.*` error codes.
- **Operations:** `agent_enrol`, `agent_poll`, `agent_results`, `agent_heartbeat`, `agent_update_get`; all initiated by the agent.
- **Rules:** every request signed over method, path, body digest, timestamp and nonce; the gateway cannot forge a command (administrator-signed envelopes verified locally, with the checks listed before execution); at-most-once execution; 64 KiB redacted output; revocation and key rotation; 2 minutes of clock skew with a monotonic fallback; signed update manifests and downgrade refusal.
- `check.py` (rules 1 to 10, with self-tests). The operations are not served until WP-ADM-02.T2 and T3 land.
- **compatibility.** New contract; nothing older exists.
