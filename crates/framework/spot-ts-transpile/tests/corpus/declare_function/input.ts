declare function emit(event: string, payload: unknown): void;

export const saved = (): void => emit("saved", { at: Date.now() });
