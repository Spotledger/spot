export class Boom {
  explode(): never {
    throw new Error("method");
  }
}

export function in_method(_: unknown): never {
  return new Boom().explode();
}

class Holder {
  run = (): never => {
    throw new TypeError("field arrow");
  };
}

export function in_field_arrow(_: unknown): never {
  return new Holder().run();
}

export async function in_async(_: unknown): Promise<never> {
  await Promise.resolve(1);
  throw new RangeError("after await");
}

export function typed_error(_: unknown): never {
  throw {
    code: "app.too_small",
    message: "the quantity is too small",
    fields: [{ field: "qty", code: "app.qty_min", message: "at least 1" }],
  };
}
