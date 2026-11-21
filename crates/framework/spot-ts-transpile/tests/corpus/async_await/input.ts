export async function retry<T>(task: () => Promise<T>, times: number): Promise<T> {
  for (let attempt = 1; attempt <= times; attempt += 1) {
    try {
      return await task();
    } catch (error) {
      if (attempt === times) {
        throw error;
      }
    }
  }
  throw new Error("unreachable");
}
