# Spotledger sample app: Projects and Tasks

The framework's acceptance app (WP-DX-05.T1): a plain project-and-task tracker
whose five DocTypes exercise every platform feature a business app uses. All
names, labels and field sets are Spotledger's own (ADR-03, ADR-32) — it is
deliberately not an ERP module: no accounting, no stock.

## What is here

| Path | What |
| --- | --- |
| `app.toml` | The app manifest (ADR-17): identity, the `native:sample` controller reference, the controller steps at their hook points |
| `doctypes/*.toml` | `project`, `task`, `time_entry`, `label`, `checklist_item` |
| `permissions/*.toml` | Role grants (`Project Manager`, `Team Member`, `Viewer`), field grants, the `confidential` protection group |
| `reports/*.toml` | `hours_by_task`: hours per Task since a date (`from_date`), for Project Managers and Team Members |
| `seed.toml` | Demo data: 2 projects, 6 tasks, 8 labels, 5 time entries (drafts) |

The controller code that walks the ADR-51 lifecycle lives in
`crates/apps/spot-sample-app`: the `validate_save` hours check (0.25–24) and the
`after_post`/`after_cancel` hours roll-up onto `task.logged_hours` for Time
Entries, plus the Task owner before-save step. It is written only against
`spot-app-api` (gate G4) and runs against the app API's stub host in tests.

## What the DocTypes exercise

- naming by series (`project.code`: `PRJ-.YYYY.-.####`; `time_entry.entry_no`:
  `TE-.YYYY.-.#####`) — rendered by the naming engine (WP-MET-03) once it lands
- links, a filtered lookup (`task.assignee` picks enabled users only), a
  dynamic link (`task.related_to` over Project or Task)
- child tables (`task.checklist`) and a table-multiselect (`project.labels`)
- a submittable document with cancel and correct (`time_entry`)
- attach (`project.attachments`), long text (`project.description`,
  `task.details`, `time_entry.note`), a colour-formatted field (`label.colour`)
- a read-only derived field (`task.logged_hours`, set only by the controller)
- permissions with scope and owner rules, and field-level security
  (`task.details` in the `confidential` protection group, read by managers only)

## Running it

These commands take a fresh checkout to a running sample app. The fenced blocks marked
`<!-- test -->` are executed, in order and in one shell, by `crates/framework/spot-cli/tests/readme.rs`
against a real PostgreSQL 18, so this section cannot drift from the binary. Run them from the
repository root. `spot` needs an administrative PostgreSQL URL (a role with `CREATEDB`); the URL
below is the placeholder to replace.

Build the binary and put it on the path (not run by the test, which supplies the binary):

```bash
cargo build -p spot-cli
export PATH="$PWD/target/debug:$PATH"
```

Configure the host. `apps` is the catalogue of app directories a site may install. The platform
app comes first because the sample app links to its `user` and `file` DocTypes:

<!-- test -->
```bash
mkdir runtime
cat > runtime/common.toml <<'EOF'
apps = ["../apps/platform", "../apps/sample"]

[server]
bind = ["127.0.0.1:8000"]

[postgres]
admin_url = "postgres://spot@127.0.0.1:5432/postgres"
EOF
chmod 600 runtime/common.toml
export SPOT_ROOT="$PWD/runtime"
spot check || echo "no site yet, as expected"
```

Create a site with only the platform app. The Administrator's one-time first sign-in secret is
printed once; only its digest is stored:

<!-- test -->
```bash
ADMIN_KEY=$(spot new-site demo --bare | sed 's/.*: //')
spot list-apps
```

Install the sample app. Without `--apply` the command only prints the plan (every step with its
risk class and the plan hash) and changes nothing; with it the schema is changed online, the
install is recorded in the site file and, with `--seed`, the demo projects, tasks, labels and
time entries are loaded. Running either command again is a no-op:

<!-- test -->
```bash
spot install-app sample
spot install-app sample --apply --seed
spot install-app sample --apply --seed
spot migrate
spot check
```

Add a user. The command prints a one-time first sign-in secret and stores only its digest. A
password is read with echo off, or from standard input (never from an argument) — here the
operator chooses it, which also spends the secret:

<!-- test -->
```bash
spot add-user demo ama@example.com --name Ama --role "User Manager"
echo "violet-lantern-staple-9-Quartz" | spot set-password demo ama@example.com --password-stdin
```

Serve the site, sign in with the Administrator's one-time secret, choose a password and create a
project with the session token that comes back. The secret is a password, not an API key: sent as a
bearer credential it is refused, and the session `POST /api/v1/auth/login` starts with it can do
nothing but change the password, sign out and read itself until then (`403
cli.password_change_required`; the login answer itself carries the warning
`auth.password_change_required`). The change
replaces that session, so the token the second call answers with is the one to use. Replace the
host name in the `Host` header with the site name or one of its aliases.

Warning: the commands below pass the secret and the new password to `curl -d`, which puts them on
the command line (visible in the process list and kept in shell history). That is acceptable only
for this throwaway local demo. For a real site, read the body from standard input or a file with
mode 600 (`curl --data @-`), never type a real password into a command line, and use a password
of your own, not the example one.

<!-- test -->
```bash
spot serve &
SERVER=$!
for _ in $(seq 120); do
  curl -fs http://127.0.0.1:8000/api/health/ready >/dev/null && break
  sleep 0.5
done
token_of() { sed -n 's/.*"token":"\([^"]*\)".*/\1/p'; }
RESTRICTED=$(curl -fsS -X POST -H "Host: demo" -H "Content-Type: application/json" \
  -d "{\"user\":\"administrator\",\"password\":\"$ADMIN_KEY\"}" \
  http://127.0.0.1:8000/api/v1/auth/login | token_of)
TOKEN=$(curl -fsS -X POST -H "Host: demo" \
  -H "Authorization: Bearer $RESTRICTED" -H "Content-Type: application/json" \
  -d "{\"current\":\"$ADMIN_KEY\",\"new\":\"violet-lantern-staple-9-Quartz\"}" \
  http://127.0.0.1:8000/api/v1/auth/password | token_of)
curl -fsS -H "Host: demo" -H "Authorization: Bearer $TOKEN" \
  http://127.0.0.1:8000/api/v1/meta/project >/dev/null && echo "signed in"
curl -fsS -X POST http://127.0.0.1:8000/api/v1/resource/project \
  -H "Host: demo" -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"code":"PRJ-README-1","title":"First project","status":"Planning","currency":"USD","budget":"10.00","owner":"01990000-0000-7000-8000-000000000001"}'
echo
kill "$SERVER"
wait "$SERVER" || true
```

If an app's DocTypes change later, `spot migrate` plans the change against the live database;
`spot migrate --apply --plan-hash <hash>` applies exactly the plan that was reviewed. A
destructive change is refused with its reason and a non-zero exit code. `spot reset-admin demo
--yes-i-am-the-operator` replaces the Administrator's credentials with a new one-time secret and
writes an audit record to `sites/demo/audit.jsonl`.
