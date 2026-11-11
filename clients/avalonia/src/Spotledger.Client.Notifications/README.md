# Spotledger.Client.Notifications

Notices and document staleness over the realtime client (WP-UI-02.T5, contract `native-api/realtime.md` v0.8.0). Both are **hints**: every screen is correct if no event ever arrives.

## Data flow

```
RealtimeClient --(user:<id>, kind notification)--> NotificationFeed --> bell, flyout, ToastTray --> ToastView
RealtimeClient --(doc:<doctype>:<id>, doc_*)-----> DocumentStalenessWatcher --> DocumentWatch --> StalenessBar
```

The UI-free core (`NoticeParser`, `NotificationFeed`, `ToastTray`, `DocumentStalenessWatcher`) joins rooms through `IRoomSource` (`RealtimeRoomSource` wraps `RealtimeClient`, whose join is reference counted). Time is an injected `TimeProvider`.

## Notices

- Payload `{event_id, event: "user_notice", data}`. Delivery is at-least-once, so a repeated `event_id` is ignored. An `event` this version does not know is ignored.
- `data` is app supplied and untrusted. The parser reads `title`, `body`, `severity` (`info|success|warning|error`), and a link as `doctype` plus `id`. Text is reduced to bounded plain text (title 120, body 500 characters; controls and bidirectional overrides removed). A link is a document reference matched against a strict name pattern and opened only through the shell's open-document command; it is never a URL. The contract does not yet name these `data` keys (see the task's open questions).
- The feed keeps the last 200 notices, newest first. Contract 0.8.0 has no notice list endpoint, so **nothing is invented on first connect or after `resync_required`**: the feed shows what arrives while connected, and a resync is only acknowledged.
- Toasts: at most one visible, the rest queued (20 at most); auto-dismiss after 6 s of the injected clock; hover and focus hold the countdown (the time left is kept); the toast never takes keyboard focus and is a polite live region. Do-not-disturb turns toasts off; the bell still counts.

## What is persisted

Per user and per server, in `<local app data>/Spotledger/notifications/<hash>.json` (one small file, written to a temp file and renamed): the ids of read notices (the last 200, so a replayed notice stays read), the do-not-disturb flag and the "reload clean forms" flag. **Never a notice title or body.** A file that cannot be read or written means the state lives in memory for this run.

## Staleness of an open form

`DocumentStalenessWatcher.Watch(form)` joins `doc:<doctype>:<id>` for one open form tab; disposing the watch (tab closed) leaves, so the number of joins equals the number of open tabs. For `doc_updated`, `doc_posted`, `doc_renamed` and `doc_cancelled`, an event whose `version` is not newer than the form's `LoadedVersion` is ignored (this also swallows the echo of the user's own save: versions are compared, not actors). A newer version gives the bar `ChangedClean` ("Reload") or, for a form with unsaved edits, `ChangedDirty` ("Reload (discard my changes)", behind a confirmation). Nothing reloads by itself unless the "reload clean forms" setting is on (default off), and never for a dirty form. `doc_cancelled` and `doc_deleted` change the banner and stick. A resync marks the form `MaybeStale`.

The forms project has no merge path today, so there is no "keep my changes" choice.
