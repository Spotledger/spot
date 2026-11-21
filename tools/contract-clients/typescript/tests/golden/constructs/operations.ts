// operations.ts: the operation table and the per-operation types (WP-WEB-10.T6).
// Generated from contracts/native-api/openapi.yaml, contract version 1.2.3.
// Generated, do not edit. Regenerate with tools/contract-clients/typescript/generate.py.

import type * as T from "./types";
import type { FormParts, ResponseHeaders } from "./client";

// The runtime table. `type` names the body type for orientation; the compiler
// checks calls against the `Operations` interface below.
const table = // BEGIN OPERATIONS TABLE
{
  "health": {
    "errors": {},
    "etag": false,
    "headerParams": [],
    "idempotent": false,
    "method": "GET",
    "path": "/health",
    "pathParams": [],
    "queryParams": [],
    "request": null,
    "success": [
      {
        "contentType": "application/json",
        "status": 200,
        "type": "inline"
      }
    ]
  },
  "item_attach": {
    "errors": {
      "default": "ErrorEnvelope"
    },
    "etag": false,
    "headerParams": [],
    "idempotent": false,
    "method": "POST",
    "path": "/items/{id}/attachment",
    "pathParams": [
      {
        "name": "id",
        "required": true
      }
    ],
    "queryParams": [],
    "request": {
      "contentType": "multipart/form-data",
      "type": "FormParts"
    },
    "success": [
      {
        "contentType": null,
        "status": 201,
        "type": null
      }
    ]
  },
  "item_delete": {
    "errors": {
      "default": "ErrorEnvelope"
    },
    "etag": false,
    "headerParams": [],
    "idempotent": false,
    "method": "DELETE",
    "path": "/items/{id}",
    "pathParams": [
      {
        "name": "id",
        "required": true
      }
    ],
    "queryParams": [],
    "request": null,
    "success": [
      {
        "contentType": null,
        "status": 204,
        "type": null
      }
    ]
  },
  "item_get": {
    "errors": {
      "default": "ErrorEnvelope"
    },
    "etag": true,
    "headerParams": [
      {
        "name": "If-None-Match",
        "required": false
      }
    ],
    "idempotent": false,
    "method": "GET",
    "path": "/items/{id}",
    "pathParams": [
      {
        "name": "id",
        "required": true
      }
    ],
    "queryParams": [
      {
        "array": true,
        "name": "fields",
        "required": false
      }
    ],
    "request": null,
    "success": [
      {
        "contentType": "application/json",
        "status": 200,
        "type": "Item"
      },
      {
        "contentType": null,
        "status": 304,
        "type": null
      }
    ]
  },
  "item_replace": {
    "errors": {
      "422": "ErrorEnvelope",
      "default": "ErrorEnvelope"
    },
    "etag": true,
    "headerParams": [
      {
        "name": "Idempotency-Key",
        "required": true
      },
      {
        "name": "If-Match",
        "required": true
      }
    ],
    "idempotent": true,
    "method": "PUT",
    "path": "/items/{id}",
    "pathParams": [
      {
        "name": "id",
        "required": true
      }
    ],
    "queryParams": [],
    "request": {
      "contentType": "application/json",
      "type": "Item"
    },
    "success": [
      {
        "contentType": "application/json",
        "status": 200,
        "type": "Item"
      }
    ]
  }
}
// END OPERATIONS TABLE as const;

export const operations = Object.freeze(table);

export type OperationId = keyof typeof operations;

export interface Operations {
  health: {
    args: { [key: string]: never };
    result:
      | { status: 200; unexpected?: false; headers: ResponseHeaders; body: {
        status: "live";
      } }
      | { status: number; unexpected: true; headers: ResponseHeaders; problem: unknown };
  };
  /** Attach files. */
  item_attach: {
    args: {
      path: { id: T.ItemId };
      body?: FormParts;
    };
    result:
      | { status: 201; unexpected?: false; headers: ResponseHeaders; body: null }
      | { status: number; unexpected: true; headers: ResponseHeaders; problem: T.ErrorEnvelope };
  };
  item_delete: {
    args: {
      path: { id: T.ItemId };
    };
    result:
      | { status: 204; unexpected?: false; headers: ResponseHeaders; body: null }
      | { status: number; unexpected: true; headers: ResponseHeaders; problem: T.ErrorEnvelope };
  };
  /** Fetch one item. */
  item_get: {
    args: {
      path: { id: T.ItemId };
      query?: { fields?: string[] };
      headers?: { "If-None-Match"?: string };
    };
    result:
      | { status: 200; unexpected?: false; headers: ResponseHeaders; body: T.Item }
      | { status: 304; unexpected?: false; headers: ResponseHeaders; body: null }
      | { status: number; unexpected: true; headers: ResponseHeaders; problem: T.ErrorEnvelope };
  };
  /** Replace an item. */
  item_replace: {
    args: {
      path: { id: T.ItemId };
      headers: { "Idempotency-Key": string; "If-Match": string };
      body: T.Item;
    };
    result:
      | { status: 200; unexpected?: false; headers: ResponseHeaders; body: T.Item }
      | { status: 422; unexpected?: false; headers: ResponseHeaders; problem: T.ErrorEnvelope }
      | { status: number; unexpected: true; headers: ResponseHeaders; problem: T.ErrorEnvelope };
  };
}
export type OperationArgs<K extends OperationId> = Operations[K]["args"];
export type OperationResult<K extends OperationId> = Operations[K]["result"];
