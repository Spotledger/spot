# Verifying a Spotledger webhook signature

Every delivery carries these headers:

| Header | Meaning |
| --- | --- |
| `X-Spot-Signature` | `t=<unix seconds>,v1=<hex hmac-sha256(secret, t + "." + body)>` |
| `X-Spot-Event-Id` | The event id (the deduplication key) |
| `X-Spot-Delivery` | The delivery id |
| `User-Agent` | `Spotledger-Webhooks/1.0` |

The signed message is the timestamp text, a literal `.`, then the **exact request body**
bytes. Reject a timestamp more than five minutes from now, and compare the signature in
constant time. The examples below are text, not compiled tests.

## Node.js

```js
const crypto = require("crypto");

function verify(secret, header, body, toleranceSeconds = 300) {
  const parts = Object.fromEntries(header.split(",").map((p) => p.split("=")));
  const timestamp = Number(parts.t);
  if (Math.abs(Math.floor(Date.now() / 1000) - timestamp) > toleranceSeconds) return false;
  const expected = crypto
    .createHmac("sha256", secret)
    .update(`${timestamp}.`)
    .update(body)
    .digest("hex");
  const a = Buffer.from(expected, "hex");
  const b = Buffer.from(parts.v1, "hex");
  return a.length === b.length && crypto.timingSafeEqual(a, b);
}
```

## Python

```python
import hashlib, hmac, time

def verify(secret: bytes, header: str, body: bytes, tolerance_seconds: int = 300) -> bool:
    parts = dict(p.split("=", 1) for p in header.split(","))
    timestamp = int(parts["t"])
    if abs(int(time.time()) - timestamp) > tolerance_seconds:
        return False
    message = str(timestamp).encode() + b"." + body
    expected = hmac.new(secret, message, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, parts["v1"])
```

## Go

```go
package webhook

import (
	"crypto/hmac"
	"crypto/sha256"
	"encoding/hex"
	"strconv"
	"strings"
	"time"
)

func Verify(secret []byte, header string, body []byte, tolerance time.Duration) bool {
	parts := map[string]string{}
	for _, p := range strings.Split(header, ",") {
		kv := strings.SplitN(p, "=", 2)
		parts[kv[0]] = kv[1]
	}
	ts, err := strconv.ParseInt(parts["t"], 10, 64)
	if err != nil || time.Since(time.Unix(ts, 0)) > tolerance {
		return false
	}
	mac := hmac.New(sha256.New, secret)
	mac.Write([]byte(strconv.FormatInt(ts, 10)))
	mac.Write([]byte("."))
	mac.Write(body)
	expected := hex.EncodeToString(mac.Sum(nil))
	return hmac.Equal([]byte(expected), []byte(parts["v1"]))
}
```

## Rotating the secret

During a rotation the site keeps the previous secret valid for an overlap window. Verify
against the current secret first, then the previous one until the window ends.
