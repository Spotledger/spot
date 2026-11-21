"""Minimal PNG reader/writer, standard library only (WP-UI-16.T2).

The browser visual comparator must not add a dependency, so it decodes the PNGs Playwright
writes and re-encodes the diff/review images itself. Supports 8-bit, non-interlaced greyscale
(type 0), truecolour (2) and truecolour+alpha (6) — what Chromium and .NET emit — and refuses
anything else instead of guessing. Normalised output is always RGBA8.
"""
from __future__ import annotations

from dataclasses import dataclass
import struct
import zlib

PNG_SIGNATURE = b'\x89PNG\r\n\x1a\n'
COLOUR_CHANNELS = {0: 1, 2: 3, 6: 4}
CHANNELS_PER_PIXEL = 4


class PngError(ValueError):
    """The file is not a PNG this reader understands."""


@dataclass(frozen=True)
class Image:
    """An RGBA8 image: `pixels` is row-major, four bytes per pixel, no padding."""

    width: int
    height: int
    pixels: bytes

    @property
    def byte_size(self):
        return self.width * self.height * CHANNELS_PER_PIXEL

    def pixel(self, x, y):
        offset = (y * self.width + x) * CHANNELS_PER_PIXEL
        return self.pixels[offset:offset + CHANNELS_PER_PIXEL]

    def row(self, y):
        start = y * self.width * CHANNELS_PER_PIXEL
        return self.pixels[start:start + self.width * CHANNELS_PER_PIXEL]


def _chunks(data):
    if not data.startswith(PNG_SIGNATURE):
        raise PngError('not a PNG file (bad signature)')
    offset = len(PNG_SIGNATURE)
    while offset + 8 <= len(data):
        length, kind = struct.unpack('>I4s', data[offset:offset + 8])
        body = data[offset + 8:offset + 8 + length]
        if len(body) != length:
            raise PngError(f'truncated {kind!r} chunk')
        yield kind, body
        offset += 12 + length


def decode(data):
    """Decode PNG bytes into an :class:`Image`."""
    header = None
    palette = None
    compressed = bytearray()
    for kind, body in _chunks(data):
        if kind == b'IHDR':
            header = struct.unpack('>IIBBBBB', body)
        elif kind == b'PLTE':
            palette = body
        elif kind == b'IDAT':
            compressed += body
        elif kind == b'IEND':
            break
    if header is None:
        raise PngError('no IHDR chunk')
    width, height, depth, colour, compression, filtering, interlace = header
    if depth != 8:
        raise PngError(f'only 8-bit PNGs are supported, got {depth}')
    if compression != 0 or filtering != 0:
        raise PngError('unknown PNG compression or filter method')
    if interlace != 0:
        raise PngError('interlaced PNGs are not supported')
    if colour not in COLOUR_CHANNELS:
        raise PngError(f'unsupported colour type {colour}')
    if palette is not None and colour != 6:
        raise PngError('palette PNGs are not supported')
    if width <= 0 or height <= 0:
        raise PngError('empty image')
    channels = COLOUR_CHANNELS[colour]
    stride = width * channels
    raw = zlib.decompress(bytes(compressed))
    if len(raw) != height * (stride + 1):
        raise PngError(f'expected {height * (stride + 1)} bytes of scanlines, got {len(raw)}')
    rows = []
    previous = bytearray(stride)
    for y in range(height):
        start = y * (stride + 1)
        filter_kind = raw[start]
        line = bytearray(raw[start + 1:start + 1 + stride])
        _unfilter(filter_kind, line, previous, channels)
        rows.append(line)
        previous = line
    pixels = bytearray(width * height * CHANNELS_PER_PIXEL)
    for y, line in enumerate(rows):
        if channels == 4:
            pixels[y * stride:(y + 1) * stride] = line
        elif channels == 3:
            for x in range(width):
                source = x * 3
                target = (y * width + x) * 4
                pixels[target:target + 3] = line[source:source + 3]
                pixels[target + 3] = 255
        else:
            for x in range(width):
                value = line[x]
                target = (y * width + x) * 4
                pixels[target] = pixels[target + 1] = pixels[target + 2] = value
                pixels[target + 3] = 255
    return Image(width=width, height=height, pixels=bytes(pixels))


def _unfilter(kind, line, previous, bpp):
    length = len(line)
    if kind == 0:
        return
    if kind == 1:
        for index in range(bpp, length):
            line[index] = (line[index] + line[index - bpp]) & 0xFF
    elif kind == 2:
        for index in range(length):
            line[index] = (line[index] + previous[index]) & 0xFF
    elif kind == 3:
        for index in range(length):
            left = line[index - bpp] if index >= bpp else 0
            line[index] = (line[index] + ((left + previous[index]) >> 1)) & 0xFF
    elif kind == 4:
        for index in range(length):
            left = line[index - bpp] if index >= bpp else 0
            up = previous[index]
            up_left = previous[index - bpp] if index >= bpp else 0
            line[index] = (line[index] + _paeth(left, up, up_left)) & 0xFF
    else:
        raise PngError(f'unknown scanline filter {kind}')


def _paeth(left, up, up_left):
    estimate = left + up - up_left
    distance_left = abs(estimate - left)
    distance_up = abs(estimate - up)
    distance_up_left = abs(estimate - up_left)
    if distance_left <= distance_up and distance_left <= distance_up_left:
        return left
    return up if distance_up <= distance_up_left else up_left


def read(path):
    with open(path, 'rb') as handle:
        return decode(handle.read())


def encode(image):
    """Encode an :class:`Image` as an RGBA8 PNG with filter 0."""
    if image.width <= 0 or image.height <= 0:
        raise PngError('empty image')
    if len(image.pixels) != image.byte_size:
        raise PngError('pixel buffer does not match width x height x 4')
    raw = bytearray()
    stride = image.width * CHANNELS_PER_PIXEL
    for y in range(image.height):
        raw.append(0)
        raw += image.pixels[y * stride:(y + 1) * stride]
    header = struct.pack('>IIBBBBB', image.width, image.height, 8, 6, 0, 0, 0)
    return (PNG_SIGNATURE
            + _chunk(b'IHDR', header)
            + _chunk(b'IDAT', zlib.compress(bytes(raw), 9))
            + _chunk(b'IEND', b''))


def _chunk(kind, body):
    return struct.pack('>I', len(body)) + kind + body + struct.pack('>I', zlib.crc32(kind + body) & 0xFFFFFFFF)


def write(path, image):
    with open(path, 'wb') as handle:
        handle.write(encode(image))


def solid(width, height, rgba):
    """A test helper: a single-colour image."""
    return Image(width, height, bytes(rgba) * (width * height))


def paint(image, x, y, rgba):
    """A test helper: a copy of `image` with one pixel replaced."""
    pixels = bytearray(image.pixels)
    offset = (y * image.width + x) * CHANNELS_PER_PIXEL
    pixels[offset:offset + 4] = bytes(rgba)
    return Image(image.width, image.height, bytes(pixels))
