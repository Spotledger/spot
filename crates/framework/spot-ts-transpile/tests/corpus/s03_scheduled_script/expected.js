import { jobs } from "spot:jobs";
import { files } from "spot:files";
var State = /* @__PURE__ */ function(State) {
	State[State["Pending"] = 0] = "Pending";
	State[State["Running"] = 1] = "Running";
	State[State["Done"] = 2] = "Done";
	State[State["Failed"] = 3] = "Failed";
	return State;
}(State || {});
const label = (state) => State[state] ?? "pending";
export async function sweep() {
	const stale = await jobs.stale(72);
	let seen = 0;
	for (const job of stale.slice(0, 50)) {
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
