import { audit, emit } from "spot:audit";

export const fire = (id: string): void => {
  audit("fired", { id });
  emit(id, { at: Date.now() });
};
