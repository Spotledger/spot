"""Browser screenshot comparison with the same tolerance the .NET helper uses (WP-UI-16.T2).

The rules are ported, not shared through a build: a pixel is changed when any of R, G, B and A
differs by more than the channel delta (default 2); a case matches while at most 0.1% of pixels
changed. A dimension mismatch fails regardless of tolerance, and a missing baseline always fails
with the exact command that creates it — never a silent pass and never a fallback to another
platform's image.

    python tools/browser-visual/src/compare.py --captures out --baselines baselines/browser
"""
from __future__ import annotations

import argparse
import html
import json
from dataclasses import dataclass
from pathlib import Path
import shutil
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))

import png  # noqa: E402  (the module next to this file)

CHANGED_COLOUR = b'\xff\x00\x00\xff'
UNCHANGED_COLOUR = b'\x00\x00\x00\xff'
ABSENT_COLOUR = b'\xff\x00\xff\xff'
UPDATE_COMMAND = 'bash tools/browser-visual/run.sh --update'


class CompareError(ValueError):
    """Invalid tolerance or unreadable input; never a pass."""


@dataclass(frozen=True)
class Tolerance:
    """The .NET defaults: ScreenshotTolerance(ChannelDelta: 2, ChangedPixelFraction: 0.001)."""

    channel_delta: int = 2
    changed_pixel_fraction: float = 0.001

    def __post_init__(self):
        if not 0 <= self.channel_delta <= 255:
            raise CompareError(f'channel delta out of range: {self.channel_delta}')
        if not 0 <= self.changed_pixel_fraction <= 1:
            raise CompareError(f'changed pixel fraction out of range: {self.changed_pixel_fraction}')

    def describe(self):
        return f'channel delta {self.channel_delta}, changed pixel fraction {self.changed_pixel_fraction}'


@dataclass(frozen=True)
class Comparison:
    """`status` is `match`, `pixels`, `size` or `missing`; only the first is a pass."""

    name: str
    status: str
    changed: int = 0
    total: int = 0
    expected_size: tuple = ()
    actual_size: tuple = ()

    @property
    def ok(self):
        return self.status == 'match'

    @property
    def ratio(self):
        return self.changed / self.total if self.total else 0.0

    def describe(self, tolerance=None):
        if self.status == 'missing':
            return f'{self.name}: MISSING baseline ({self.actual_size[0]}x{self.actual_size[1]}); create it with {UPDATE_COMMAND}'
        if self.status == 'size':
            return (f'{self.name}: SIZE MISMATCH expected {self.expected_size[0]}x{self.expected_size[1]}, '
                    f'actual {self.actual_size[0]}x{self.actual_size[1]}')
        if self.status == 'pixels':
            limit = tolerance or Tolerance()
            return (f'{self.name}: {self.changed}/{self.total} pixels changed ({self.ratio:.4%} > '
                    f'{limit.changed_pixel_fraction:.2%}, channel delta {limit.channel_delta})')
        return f'{self.name}: match ({self.changed}/{self.total} pixels changed)'


def compare(name, expected, actual, tolerance):
    """Compare two images with the ported .NET rules."""
    if expected.width != actual.width or expected.height != actual.height:
        return Comparison(name, 'size', total=actual.width * actual.height,
                          expected_size=(expected.width, expected.height), actual_size=(actual.width, actual.height))
    total = actual.width * actual.height
    changed = 0
    expected_pixels = expected.pixels
    actual_pixels = actual.pixels
    delta = tolerance.channel_delta
    stride = actual.width * 4
    for y in range(actual.height):
        start = y * stride
        expected_row = expected_pixels[start:start + stride]
        actual_row = actual_pixels[start:start + stride]
        if expected_row == actual_row:
            continue
        for offset in range(0, stride, 4):
            if (abs(actual_row[offset] - expected_row[offset]) > delta
                    or abs(actual_row[offset + 1] - expected_row[offset + 1]) > delta
                    or abs(actual_row[offset + 2] - expected_row[offset + 2]) > delta
                    or abs(actual_row[offset + 3] - expected_row[offset + 3]) > delta):
                changed += 1
    status = 'match' if total == 0 or changed / total <= tolerance.changed_pixel_fraction else 'pixels'
    return Comparison(name, status, changed=changed, total=total,
                      expected_size=(expected.width, expected.height), actual_size=(actual.width, actual.height))


def diff(expected, actual, tolerance):
    """Red for changed pixels, black for unchanged, magenta where a pixel is absent (max-size canvas)."""
    width = max(expected.width, actual.width)
    height = max(expected.height, actual.height)
    pixels = bytearray(width * height * 4)
    for y in range(height):
        for x in range(width):
            target = (y * width + x) * 4
            if x >= expected.width or y >= expected.height or x >= actual.width or y >= actual.height:
                pixels[target:target + 4] = ABSENT_COLOUR
                continue
            left = expected.pixel(x, y)
            right = actual.pixel(x, y)
            changed = any(abs(right[channel] - left[channel]) > tolerance.channel_delta for channel in range(4))
            pixels[target:target + 4] = CHANGED_COLOUR if changed else UNCHANGED_COLOUR
    return png.Image(width, height, bytes(pixels))


def compare_files(name, capture_path, baseline_path, tolerance):
    """Load both files and compare; a missing baseline is a failure, never an empty comparison."""
    actual = png.read(capture_path)
    if baseline_path is None or not Path(baseline_path).is_file():
        return Comparison(name, 'missing', total=actual.width * actual.height,
                          actual_size=(actual.width, actual.height)), actual, None
    expected = png.read(baseline_path)
    comparison = compare(name, expected, actual, tolerance)
    return comparison, actual, expected


def write_review(directory, name, expected, actual, difference, comparison, tolerance):
    """Write the three review images and an HTML page next to them; return the page path."""
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    written = {}
    for kind, image in (('expected', expected), ('actual', actual), ('diff', difference)):
        if image is None:
            continue
        path = directory / f'{name}-{kind}.png'
        png.write(path, image)
        written[kind] = path
    title = html.escape(name)
    figures = '\n'.join(
        f'<figure><figcaption>{kind}</figcaption><img alt="{kind}" src="{title}-{kind}.png"></figure>'
        for kind in ('expected', 'actual', 'diff') if kind in written)
    page = directory / f'{name}.html'
    page.write_text(
        '<!doctype html><meta charset="utf-8">'
        f'<title>{title}</title><h1>{title}</h1>'
        f'<p>{html.escape(comparison.describe(tolerance))}</p>'
        f'<p>Tolerance: {html.escape(tolerance.describe())}. '
        'Update deliberately with <code>' + html.escape(UPDATE_COMMAND) + '</code> after reviewing all three images.</p>'
        + figures,
        encoding='utf-8')
    return page


def run(captures, baselines, out, tolerance, update=False):
    """Compare every capture; return (comparisons, failures)."""
    captures = Path(captures)
    baselines = Path(baselines)
    if not captures.is_dir():
        raise CompareError(f'{captures}: no capture directory; run the capture step first')
    names = sorted(path.stem for path in captures.glob('*.png'))
    if not names:
        raise CompareError(f'{captures}: no captures (*.png) to compare')
    comparisons = []
    for name in names:
        capture_path = captures / f'{name}.png'
        baseline_path = baselines / f'{name}.png'
        comparison, actual, expected = compare_files(name, capture_path, baseline_path, tolerance)
        comparisons.append(comparison)
        if update:
            baselines.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(capture_path, baseline_path)
            comparisons[-1] = Comparison(name, 'match', total=actual.width * actual.height,
                                         expected_size=(actual.width, actual.height),
                                         actual_size=(actual.width, actual.height))
            continue
        if not comparison.ok:
            difference = diff(expected, actual, tolerance) if expected is not None else None
            page = write_review(Path(out) / 'review', name, expected, actual, difference, comparison, tolerance)
            print(f'review: {page}', file=sys.stderr)
    return comparisons, [comparison for comparison in comparisons if not comparison.ok]


def main(argv=None):
    parser = argparse.ArgumentParser(description='Compare browser screenshots with the committed baselines.')
    parser.add_argument('--captures', default='tools/browser-visual/results/captures', type=Path)
    parser.add_argument('--baselines', default='tools/browser-visual/baselines/browser', type=Path)
    parser.add_argument('--out', default='tools/browser-visual/results', type=Path)
    parser.add_argument('--channel-delta', default=2, type=int)
    parser.add_argument('--max-changed', default=0.001, type=float, help='changed pixel fraction allowed')
    parser.add_argument('--update', action='store_true', help='write the captures as the new baselines')
    parser.add_argument('--json', action='store_true', help='also print a machine-readable summary')
    arguments = parser.parse_args(argv)
    try:
        tolerance = Tolerance(arguments.channel_delta, arguments.max_changed)
        comparisons, failures = run(arguments.captures, arguments.baselines, arguments.out, tolerance, arguments.update)
    except (CompareError, png.PngError, OSError) as error:
        print(f'browser-visual compare: {error}', file=sys.stderr)
        return 1
    mode = 'updated' if arguments.update else 'compared'
    for comparison in comparisons:
        print(comparison.describe(tolerance))
    print(f'browser-visual compare: {len(comparisons)} case(s) {mode}, {len(failures)} failing')
    if arguments.json:
        print(json.dumps({'cases': [comparison.__dict__ for comparison in comparisons],
                          'failures': len(failures), 'updated': arguments.update}, default=list))
    return 0 if not failures else 1


if __name__ == '__main__':
    raise SystemExit(main())
