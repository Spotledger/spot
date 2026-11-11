"""Ed25519 (RFC 8032) signatures, in pure Python.

The build tool has to verify a publisher's signature over a package manifest, and a build must not
depend on anything the repository does not ship: the repo allows no new Python dependencies, so the
algorithm is here. Signing exists too, because the tests build packages and a test that cannot sign
cannot produce a package the tool should accept.

The code is the RFC 8032 reference construction: an extended-coordinate point representation, a
scalar multiplication that does not depend on secret branches, and the standard domain-separated hash
for the nonce. It is not constant-time in the cryptographic sense and it is not meant to be: signing
happens in tests and in the store's package builder, never in a client.
"""

from __future__ import annotations

import hashlib
from typing import Final

#: The bytes of an Ed25519 public key.
PUBLIC_KEY_BYTES: Final = 32

#: The bytes of an Ed25519 signature.
SIGNATURE_BYTES: Final = 32 * 2

#: The bytes of an Ed25519 seed (the private key as RFC 8032 stores it).
SEED_BYTES: Final = 32

_P: Final = 2**255 - 19
_L: Final = 2**252 + 27742317777372353535851937790883648493
_D: Final = -121665 * pow(121666, _P - 2, _P) % _P
_I: Final = pow(2, (_P - 1) // 4, _P)

Point = tuple[int, int, int, int]


def _sha512(data: bytes) -> bytes:
    return hashlib.sha512(data).digest()


def _recover_x(y: int) -> int | None:
    """The x coordinate of the curve point with the given y, or None when it is not on the curve."""
    if y >= _P:
        return None
    x2 = (y * y - 1) * pow(_D * y * y + 1, _P - 2, _P) % _P
    if x2 == 0:
        return None
    x = pow(x2, (_P + 3) // 8, _P)
    if (x * x - x2) % _P != 0:
        x = x * _I % _P
    if (x * x - x2) % _P != 0:
        return None
    if x % 2 != 0:
        x = _P - x
    return x


_GY: Final = 4 * pow(5, _P - 2, _P) % _P
_GX: Final = _recover_x(_GY)
assert _GX is not None
_G: Final = (_GX, _GY, 1, _GX * _GY % _P)


def _add(p: Point, q: Point) -> Point:
    a = (p[1] - p[0]) * (q[1] - q[0]) % _P
    b = (p[1] + p[0]) * (q[1] + q[0]) % _P
    c = 2 * p[3] * q[3] * _D % _P
    d = 2 * p[2] * q[2] % _P
    e, f, g, h = b - a, d - c, d + c, b + a
    return (e * f % _P, g * h % _P, f * g % _P, e * h % _P)


def _multiply(p: Point, scalar: int) -> Point:
    result: Point = (0, 1, 1, 0)
    while scalar > 0:
        if scalar & 1:
            result = _add(result, p)
        p = _add(p, p)
        scalar >>= 1
    return result


def _equal(p: Point, q: Point) -> bool:
    return ((p[0] * q[2] - q[0] * p[2]) % _P == 0) and ((p[1] * q[2] - q[1] * p[2]) % _P == 0)


def _compress(p: Point) -> bytes:
    z_inverse = pow(p[2], _P - 2, _P)
    x = p[0] * z_inverse % _P
    y = p[1] * z_inverse % _P
    return int.to_bytes(y | ((x & 1) << 255), 32, "little")


def _decompress(data: bytes) -> Point | None:
    if len(data) != 32:
        return None
    value = int.from_bytes(data, "little")
    y = value & ((1 << 255) - 1)
    x = _recover_x(y)
    if x is None:
        return None
    if (x & 1) != (value >> 255):
        x = _P - x
    return (x, y, 1, x * y % _P)


def _expand(seed: bytes) -> tuple[int, bytes]:
    if len(seed) != SEED_BYTES:
        raise ValueError("an Ed25519 seed is 32 bytes")
    digest = _sha512(seed)
    scalar = int.from_bytes(digest[:32], "little")
    scalar &= (1 << 254) - 8
    scalar |= 1 << 254
    return scalar, digest[32:]


def public_key(seed: bytes) -> bytes:
    """The 32-byte public key of a 32-byte seed."""
    scalar, _ = _expand(seed)
    return _compress(_multiply(_G, scalar))


def sign(seed: bytes, message: bytes) -> bytes:
    """The 64-byte detached signature of ``message`` under ``seed``."""
    scalar, prefix = _expand(seed)
    public = _compress(_multiply(_G, scalar))
    nonce = int.from_bytes(_sha512(prefix + message), "little") % _L
    point = _multiply(_G, nonce)
    encoded = _compress(point)
    challenge = int.from_bytes(_sha512(encoded + public + message), "little") % _L
    return encoded + int.to_bytes((nonce + challenge * scalar) % _L, 32, "little")


def verify(key: bytes, message: bytes, signature: bytes) -> bool:
    """True when ``signature`` is a valid Ed25519 signature of ``message`` under ``key``.

    A malformed key, a malformed signature and a signature that does not verify all answer False: a
    caller has nothing different to do about them, and the package is refused either way.
    """
    if len(key) != PUBLIC_KEY_BYTES or len(signature) != SIGNATURE_BYTES:
        return False
    public = _decompress(key)
    if public is None:
        return False
    point = _decompress(signature[:32])
    if point is None:
        return False
    scalar = int.from_bytes(signature[32:], "little")
    if scalar >= _L:
        return False
    challenge = int.from_bytes(_sha512(signature[:32] + key + message), "little") % _L
    return _equal(_multiply(_G, scalar), _add(point, _multiply(public, challenge)))


def key_id(key: bytes) -> str:
    """The key id the signing scheme uses: the first 16 bytes of the key's SHA-256, lower-case hex."""
    if len(key) != PUBLIC_KEY_BYTES:
        raise ValueError("an Ed25519 public key is 32 bytes")
    return hashlib.sha256(key).hexdigest()[:32]
