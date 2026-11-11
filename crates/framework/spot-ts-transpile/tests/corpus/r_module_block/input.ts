module Util {
  export const id = (): number => 1;
}

export const use = (): number => Util.id();
