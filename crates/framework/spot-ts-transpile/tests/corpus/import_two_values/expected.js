import { audit, emit } from "spot:audit";
export const fire = (id) => {
	audit("fired", { id });
	emit(id, { at: Date.now() });
};
