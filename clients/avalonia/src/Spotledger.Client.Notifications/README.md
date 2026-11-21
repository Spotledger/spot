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


## Hosting (WP-UI-02.T6)

- **Shell.** The shell project cannot reference this one (the dependency runs the other way), so it sees the centre through `INotificationSurface` (unread count, the bell's accessible name, the flyout and toast controls, the two settings, a close request). `NotificationSurface` is the implementation: feed + `NotificationsFlyout` + `ToastTray`/`ToastView`. `ShellViewModel.AttachNotifications` replaces the bare `NotificationCount` source; the shell view hosts the flyout under the bell and the toast in the bottom reading-direction corner. The command is `view.notifications` (Ctrl+Shift+B on the desktop profile, Ctrl+Alt+B on the browser profile; also the bell, the View menu and the launcher). Do not disturb and "reload unedited forms" are the Tools commands `notifications.doNotDisturb` and `notifications.autoReload` (the shell has no settings surface yet).
- **Session.** The desktop `NotificationHost` builds the feed when a user's session starts (read state is a file per server and user) and disposes it on sign-out, which leaves `user:<id>` and closes the realtime connection of the session.
- **Forms.** `WatchedForm` adapts a `DocumentFormViewModel` to `IWatchedDocument`; `FormStaleness` joins `doc:<doctype>:<id>` once per open tab (a new document joins when first saved), puts the `StalenessBar` in the form's `Banner` slot and leaves when the tab closes. For a dirty form the bar also offers "Merge my changes": `MergeServerChangesAsync` loads the server's document, keeps every edited field and child table, applies the server's value for everything else silently, and lists each field changed on the server and by the person as a conflict (both values shown). The server's value replaces an edited field only if the person picks "Take theirs"; until they apply their choices the toolbar actions are disabled. A child table is compared as a whole, so a table changed on both sides is one conflict.
