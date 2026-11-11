export const safe = (task: () => void): string => {
  try {
    task();
    return "done";
  } catch (error: unknown) {
    return error instanceof Error ? error.message : "failed";
  }
};
