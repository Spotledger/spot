# `spot console`

An interactive or scripted session over one site's documents, as a named user. Use it to inspect and repair a site from the server without a browser.

```
spot console --site <SITE> --user <EMAIL> [--password-file <PATH>]
             [--exec "<statement>" ...] [--file <SCRIPT>] [--keep-going]
             [--format json|table] [--history] [--timeout <SECONDS>]
```

> **The console is as powerful as the account you sign in with.** It has no rights of its own. Whatever the user can do over the API, the console can do, and a `Tenant Administrator` can do everything. Sign in with an account that holds only the rights the work needs, and do not leave a session open.

## It is an ordinary client of the engine

- It signs in through the same sign-in service as `/auth/login`. A wrong password, an unknown user and a locked account get the same refusal. Failed attempts go to the same throttle and the same identity audit records as the API's. The throttle counters live in the process, as the API's do, so a lockout is counted within one console session; see the sign-in limits in the session page.
- Every statement runs as that user through the native API's own router, in process (no network): the permission engine, the field guard (a hidden field stays hidden), owner scoping, number series, the controllers and the document lifecycle all apply.
- There is no system actor, no `--as-system`, no raw SQL and no way to skip a check. A `call` reaches only the functions the user may call.
- A user that has not chosen a password yet cannot use the console. A user with a second factor is asked for the code at the terminal; a script cannot supply one.
- A partitioned node is not opened (the serving node checks ownership).

## Signing in

The password is never an argument or an environment variable. At a terminal the console asks for it with echo off (three tries). A script gives `--password-file`, whose first line is the password; the file must not be readable by group or others (`chmod 600`). The password is overwritten in memory after use.

## Statements

| Statement | Does |
| --- | --- |
| `get DOCTYPE ID` | One document. |
| `list DOCTYPE [where FIELD OP VALUE ...] [limit N] [fields a,b]` | Documents, at most `N` (default 100, at most 1000). Several `where` clauses are combined with AND. |
| `create DOCTYPE {json}` | A new document. |
| `update DOCTYPE ID {json}` | Changes the fields named in the object. The console reads the current version first and sends it, so a concurrent change is refused. |
| `delete DOCTYPE ID` | Deletes a document. |
| `call APP.MODULE.FUNCTION [{json}]` | A whitelisted function. |
| `report NAME [{json}]` | Runs a report; the object holds `filters` and `limit`. |
| `whoami`, `help`, `exit` | The signed-in user, the grammar, leave. |

Operators: `=` `!=` `<` `<=` `>` `>=` `prefix` `contains` `suffix` `is_null` `is_not_null`. A value with spaces is double quoted (`where subject = "two words"`). A bare value that reads as a number, `true`, `false` or `null` is that; anything else is text. Write decimals as quoted strings. The JSON object is the rest of the statement, from its first `{`.

`--format json` (the default) prints one JSON object per line. `--format table` prints a table; a long cell is cut at 60 characters.

## Scripts

`--exec "stmt"` (repeatable) and `--file script` run without a prompt. In a file, one statement per line; blank lines and lines starting with `#` are ignored. The script stops at the first failure and exits with that statement's code; `--keep-going` continues and still exits with the first failure's code. A script runs at most 10 000 statements and has an overall time limit (`--timeout`, default 600 seconds, at most 86 400). A statement is at most 1 MiB.

| Exit | Meaning |
| --- | --- |
| 0 | Done. |
| 1 | A statement or an option could not be read. |
| 2 | Refused: sign-in failed, permission denied, the maintenance gate is closed, the audit trail is unavailable. Nothing was changed by the refused statement. |
| 3 | A statement failed (not found, invalid, stale version, timeout). |

An error prints its stable code (`permission.denied`, `document.not_found`) and never the statement's values. A document the user cannot see answers `document.not_found`, the same as one that is not there.

## The maintenance gate

While the site's maintenance gate is closed, `create`, `update`, `delete` and `call` are refused with `server.maintenance`. If the gate cannot be read, they are refused too: the console fails closed. Reads still work. The gate lives in the migration control schema (`spot_migration_control`); until the site's database has it, the console is read-only.

## Audit

The audit chain is the site's hash-chained identity trail (`spot audit verify` checks it). The console writes, **before** each statement that changes data (and each `report`, and each read of a DocType that has protected fields), one record, and after it a second one with the outcome:

- `kind` `console_create`, `console_update`, `console_delete`, `console_call`, `console_report` or `console_read` (the outcome record adds `_outcome`);
- `path` is the channel, always `console`;
- `user` is the application user's record id, `attempted` is the OS account name the process runs as, read from the operating system (the real uid, named through `/etc/passwd`, or `uid:N`); `USER` and `LOGNAME` are not consulted, so setting them changes nothing. On Windows it is `USERNAME`, which a process environment can change;
- `reason` is the DocType, function or report, and in the outcome record its stable error code or `ok`.

If the "before" record cannot be written, the statement is refused and nothing runs. Records carry no document value. The document engine's own audit and events apply as for the API.

## History

None by default, because statements can hold personal data. `--history` appends each statement you ran to `spot/console-history` in your state directory (`$XDG_STATE_HOME`, else `~/.local/state`; `%LOCALAPPDATA%` on Windows), created with mode 0600. Delete it when you are done.
