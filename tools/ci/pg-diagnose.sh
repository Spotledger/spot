#!/usr/bin/env bash
# Prints what is needed to explain a PostgreSQL error from the throwaway cluster that
# start-postgres.sh started (SQLSTATE 58P01 "undefined file", "starting up", a vanished data
# directory). Runs in an `if: failure()` step; it never fails the job.
dir="${SPOT_PG_DIR:-}"
echo "SPOT_PG_DIR=${dir:-unset}"
if [ -z "$dir" ]; then exit 0; fi
if [ ! -d "$dir" ]; then
  echo "the cluster directory no longer exists (something removed it while the job ran)"
  ls -la "$(dirname "$dir")" 2>&1 | head -20
else
  ls -la "$dir" "$dir/data" 2>&1 | head -40
  echo "--- server log: errors and the last 40 lines"
  grep -nE "FATAL|ERROR|PANIC|could not|No such file" "$dir/log" 2>/dev/null | tail -30
  tail -n 40 "$dir/log" 2>/dev/null
fi
echo "--- postgres processes"
ps -eo pid,ppid,etime,user,args 2>/dev/null | grep "[p]ostgres" | head -20
echo "--- disk"
df -h "${RUNNER_TEMP:-/tmp}" 2>&1 | tail -2
exit 0
