import { jobs } from "spot:jobs";
import { files } from "spot:files";

enum State { Pending = 0, Running = 1, Done = 2, Failed = 3 }

const enum Thresholds { MaxAgeHours = 72, Batch = 50 }

const label = (state: State): string => State[state] ?? "pending";

export async function sweep(): Promise<void> {
  const stale = await jobs.stale(Thresholds.MaxAgeHours);
  let seen = 0;
  for (const job of stale.slice(0, Thresholds.Batch)) {
    seen += 1;
    if (job.state === State.Running) {
      continue;
    }
    if (job.state === State.Failed) {
      await files.append("sweep.log", `${label(job.state)} ${job.name}`);
    }
    await jobs.cancel(job.id);
  }
  await jobs.schedule("sweep", seen === 0 ? "tomorrow" : "hourly");
}