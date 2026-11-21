// types.ts: one exported type per schema of the contract (WP-WEB-10.T6).
// Generated from contracts/native-api/openapi.yaml, contract version 1.2.3.
// Generated, do not edit. Regenerate with tools/contract-clients/typescript/generate.py.

export type Bag = { [key: string]: number };

export type Color = "red" | "green";

export type Combined = Item & {
  extra?: boolean;
};

export interface ErrorEnvelope {
  error: {
    code: string;
    message: string;
  };
}

/** One item. */
export interface Item {
  id: ItemId;
  name: string;
  note?: string | null;
  color?: Color;
  count?: number;
  created_at?: string;
  tags?: string[];
  kind?: "item";
}

export type ItemId = string;

export type Keyed = /* propertyNames: {"pattern": "^[a-z]+$"} */ { [key: string]: string };

export type NotEmpty = /* not: {"const": ""} */ string;

export type Opaque = string | number | boolean | null;

export type Shape = Item | Bag;

