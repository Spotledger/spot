# spot-powerloss (WP-PRD-05.T1)

Power-loss and crash-consistency harness. WP-PRD-05's acceptance is **1,000 forced power cuts
during posting with zero invariant violations**. A real power cut is approximated by
hard-killing a VM (or the process tree plus filesystem barrier loss where a VM is unavailable).
This harness runs a posting workload against the service in a disposable target, cuts at a
seeded random point, boots, waits for recovery, and then runs the invariant suite and the
durability check against the client-side history.

The harness is the deliverable; the 1,000-cut campaign is run on the Windows VM runner (see
[Running the 1,000-cut campaign](#running-the-1000-cut-campaign-on-the-windows-runner)).

## Run it

```bash
# The acceptance pair:
python -m unittest discover -s harness/powerloss/tests -v
python harness/powerloss/powerloss.py --selftest

# One cycle on the model target, printing the full report:
python harness/powerloss/powerloss.py cycle --seed 7 --cut-after 40

# A campaign, resumable, JSON-lines results + a summary table:
python harness/powerloss/powerloss.py campaign --cycles 1000 --seed 7 --out results.jsonl

# The negative control: fsync=off, the audit must find a violation:
python harness/powerloss/powerloss.py negative-control --cycles 1 --seed 7
```

`--selftest` runs 20 `ProcessKill` cycles against the Linux service build when one is
configured (`SPOT_POWERLOSS_WORKLOAD_CMD` and friends) and against the **model target**
otherwise, then runs the negative control and requires it to fail. It exits non-zero if either
half is wrong: a green selftest means the durable runs passed *and* the audit is not blind.

## What it proves

- **The cut.** A target is cut by a driver and booted again; the harness records the cut
  offset, the recovery time and (on a real target) the WAL replay time per cycle.
- **The durability rule.** Every commit the client was told succeeded is present in the
  recovered store; every operation that was refused or failed left nothing; an operation that
  was not acknowledged is wholly present or wholly absent, never partial; no commit outcome is
  left unresolved; sequences have no gap or duplicate; the outbox holds each event at most
  once.
- **The framework invariants.** Document-version continuity (no lost update), child-row
  integrity, link integrity, unique keys, and the store's sequence state, over the recovered
  store.
- **It can fail.** The negative control runs the same machinery with `fsync = off` and requires
  the durability audit to report `acknowledged_effect_absent`. A campaign whose green runs came
  from an audit that cannot fail would prove nothing.

## What it does not prove

- **The model target is a model.** `--target fake` (the default, and what `--selftest` uses
  when no real target is configured) runs a small store that models only `fsync` and
  `synchronous_commit` (`fakestore.py`). It is a fast, deterministic way to test the machinery
  and to run the negative control; it is **not** evidence about the real server. Only a real
  target (`--target local` / `--target vm`) can produce that.
- **`full_page_writes`.** The harness asserts it is on the real server but does not model torn
  pages; disk faults and torn pages belong to a physical rig.
- **`ProcessKill` is the weakest cut.** The OS page cache survives a process kill, so it cannot
  lose data the way a power cut does. Use `vm-reset` for the real campaign.
- **The workload's interleaving.** Replaying a seed repeats the *schedule* (who does what, in
  which order, at which cut point), not the interleaving of threads on a real machine.

## The workload and the history

The posting workload and the client-side history are the ones of
`harness/rust/l7-invariants`: the same operation mix (party, order, linked order, update,
delete, counter increment, sequence allocation, outbox emit, read), the same deterministic
schedule (the seeded xorshift of `schedule.rs`), and the same `spot-l7-history` v1 JSON-lines
format (`history.rs` documents it as public). `l7history.py` reads and writes exactly that
format, so a history from the L7 tools can be audited here and vice versa.

The one difference power loss demands: the L7 CLI writes the whole history **after** its run
ends, so a cut mid-run loses it. This harness appends each entry and `fsync`s it **before** the
operation counts as acknowledged (`HistoryWriter`), so the client-side history survives the cut.
That is what makes "every acknowledged commit is present" checkable.

The workload runs **inside the target** through `guest/run_workload.py` (Windows:
`guest/run_workload.ps1`). It delegates to a real client with `--client-cmd` (which must stream
the same history format) or runs the model client with `--store model`.

## Cut drivers

| Driver | What | Where |
| --- | --- | --- |
| `process-kill` | `kill -9` each service pid (POSIX) | anywhere |
| `process-kill-windows` | `taskkill /F /T /PID` each service pid | Windows |
| `vm-reset` | `virsh destroy` / `virsh qemu-monitor-command ... system_reset` / `Stop-VM -TurnOff` | libvirt/QEMU on Linux, Hyper-V on Windows |
| `disk-freeze` | Linux `dm-flakey` drop-writes for a window | Linux, needs root |

Every driver builds an **argv list**, never a shell string, and validates any name it embeds
(`cutdrivers.safe_token`). A driver that cannot run on the current host is refused, never
silently swapped. The harness refuses to cut a real target unless the operator passes `--confirm-disposable`
(it cannot tell a throwaway VM from a machine that matters), and `perform_cut` re-checks the
mark, so a driver never runs against a target nobody declared disposable.

## The target

A target is one disposable machine with the service and PostgreSQL on it. The harness talks to
it through commands, so it never speaks SQL itself:

- `--workload-cmd` — starts the workload inside the target (`guest/run_workload.py ...`).
- `--snapshot-cmd` — prints a `StoreSnapshot` JSON (the change feed, outbox, sequences and
  documents) after recovery.
- `--settings-cmd` — prints `{"fsync": "...", "synchronous_commit": "...",
  "full_page_writes": "..."}` for the server under test.
- `--health-cmd` — succeeds when the service is healthy (used to wait for recovery).
- `--pids-cmd` — prints the service and PostgreSQL pids, one per line.
- `--boot-cmd` — boots the target (libvirt `virsh start`, Hyper-V `Start-VM`).

Before every cycle the harness asserts `fsync`, `synchronous_commit` and `full_page_writes` are
all `on` and **fails** otherwise; only the negative control skips that assertion. The product
defaults already force them on (`crates/framework/spot-pg-supervisor`), so a failure here means
the target is misconfigured.

## Running the 1,000-cut campaign on the Windows runner

The Windows runner is a Windows Server 2022 evaluation VM on a KVM host. For a power cut that means data, run the **service and PostgreSQL
in a second, disposable Hyper-V VM** on that host and cut *that* VM, never the runner:

1. **Provision the target VM** (once). A Hyper-V VM on the Windows host, on a virtual disk with
   **write caching disabled** so a cut can lose un-flushed writes: in Hyper-V Manager, the disk's
   *Advanced Features* → clear *Enable write caching on the disk*; in PowerShell,
   `Set-VMHardDiskDrive -VMName spot-pg -ControllerType SCSI -ControllerNumber 0 -ControllerLocation 0`
   after clearing the host's write-cache setting for that disk. A disk with write caching on
   would make the cut meaningless. Install PostgreSQL 18 and the service build in the VM; leave
   `fsync`, `synchronous_commit` and `full_page_writes` at their product defaults (on).
2. **Reset between campaigns** with a Hyper-V checkpoint: take a clean checkpoint right after
   provisioning (`Checkpoint-VM -Name spot-pg -SnapshotName clean`), then before each campaign
   `Restore-VMSnapshot -Name spot-pg -SnapshotName clean -Confirm:$false`. A campaign that ends
   on a violation leaves the VM as-is, so the failing disk image is kept for forensics.
3. **Service recovery options.** Set the PostgreSQL and service recovery to *take no action* on
   failure (`sc.exe failure "postgresql-x64-18" reset= 0 actions= ""`), so a crashed process is
   not auto-restarted before the harness has booted the VM and started recovery itself. Start
   the service at boot (`Set-Service -StartupType Automatic`).
4. **Run the campaign** from the Windows host (the orchestrator survives the cut; only the
   target VM is cut). The target commands are the operator's adapters: the harness ships the
   workload agent and the history format, not a SQL client (see the PR's Open questions). Point
   `--workload-cmd` at the guest workload agent delegating to a real streaming client
   (`--client-cmd`), and `--snapshot-cmd` / `--settings-cmd` / `--health-cmd` / `--pids-cmd` at
   adapters that read the recovered store, the server settings, the service health and the
   service pids:

   ```powershell
   python harness\powerloss\powerloss.py campaign --cycles 1000 --seed 7 `
     --driver vm-reset --target vm --confirm-disposable --hypervisor hyperv --vm spot-pg `
     --workload-cmd "python C:\spot\harness\powerloss\guest\run_workload.py --client-cmd 'spot-l7 run --backend pg'" `
     --snapshot-cmd "<snapshot adapter>" `
     --settings-cmd "<settings adapter>" `
     --health-cmd  "<health adapter>" `
     --pids-cmd    "<pids adapter>" `
     --boot-cmd    "powershell -Command Start-VM -Name spot-pg" `
     --out results.jsonl
   ```

   A smoke run on the target, to prove the plumbing before a long campaign, uses
   `--workload-cmd "python C:\spot\harness\powerloss\guest\run_workload.py --store model"`
   (the model workload; it does not touch the real server).
5. **Resume** after an interruption by running the same command again: cycles already recorded
   in `results.jsonl` are skipped. The summary table and the classification totals are printed
   at the end; the exit code is non-zero if any cycle violated an invariant.

The Linux equivalent runs in CI: the same harness with `--target local`, `--driver process-kill`
and `--confirm-disposable` (the target is a throwaway PostgreSQL cluster started by `tools/ci/start-postgres.sh`), so the orchestration
is exercised on every change even though the cut is weaker.

## Honesty and limits

- The model target's negative control is deterministic; a real `fsync=off` campaign may only
  *sometimes* lose a commit, so the real negative control is run until it fails.
- A cut that lands before the workload acknowledged anything proves nothing; the harness
  records the cut offset so an operator can see it and the campaign's `cut_min_ops` keeps cuts
  inside the run.
- The outbox check is limited to duplicate event ids: the `spot-l7-history` v1 format does not
  carry event ids, so the audit cannot tie events to operations. This is an Open question in
  the PR.
