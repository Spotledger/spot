// operations.ts: the operation table and the per-operation types (WP-WEB-10.T6).
// Generated from contracts/native-api/openapi.yaml, contract version 0.0.1.
// Generated, do not edit. Regenerate with tools/contract-clients/typescript/generate.py.

import type * as T from "./types";
import type { FormParts, ResponseHeaders } from "./client";

// The runtime table. `type` names the body type for orientation; the compiler
// checks calls against the `Operations` interface below.
const table = // BEGIN OPERATIONS TABLE
{
  "hostile_get": {
    "errors": {},
    "etag": false,
    "headerParams": [],
    "idempotent": false,
    "method": "GET",
    "path": "/x",
    "pathParams": [],
    "queryParams": [],
    "request": null,
    "success": [
      {
        "contentType": "application/json",
        "status": 200,
        "type": "Evil"
      }
    ]
  }
}
// END OPERATIONS TABLE as const;

export const operations = Object.freeze(table);

export type OperationId = keyof typeof operations;

export interface Operations {
  /** *\/ ; process.exit(1); /* */
  hostile_get: {
    args: { [key: string]: never };
    result:
      | { status: 200; unexpected?: false; headers: ResponseHeaders; body: T.Evil }
      | { status: number; unexpected: true; headers: ResponseHeaders; problem: unknown };
  };
}
export type OperationArgs<K extends OperationId> = Operations[K]["args"];
export type OperationResult<K extends OperationId> = Operations[K]["result"];
