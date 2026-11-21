"""The PNG reader/writer and the scenario catalogue (WP-UI-16.T2)."""
from __future__ import annotations

import json
import unittest
import zlib

import _support
from _support import png

VIEWPORTS = {(1280, 800), (768, 1024), (390, 844)}


class PngTest(unittest.TestCase):
    def test_round_trip_keeps_every_pixel(self):
        image = png.solid(3, 2, (1, 2, 3, 4))
        decoded = png.decode(png.encode(image))
        self.assertEqual((decoded.width, decoded.height), (3, 2))
        self.assertEqual(decoded.pixels, image.pixels)

    def test_a_foreign_signature_is_rejected(self):
        with self.assertRaises(png.PngError):
            png.decode(b'\x89GIF\r\n\x1a\n')

    def test_a_truncated_file_is_rejected(self):
        data = png.encode(png.solid(2, 2, (0, 0, 0, 255)))
        with self.assertRaises(png.PngError):
            png.decode(data[:20])

    def test_interlaced_and_paletted_files_are_refused_not_guessed(self):
        def chunk(kind, body):
            return (len(body).to_bytes(4, 'big') + kind + body
                    + (zlib.crc32(kind + body) & 0xFFFFFFFF).to_bytes(4, 'big'))

        def build(depth, colour, interlace):
            header = (2).to_bytes(4, 'big') + (2).to_bytes(4, 'big') + bytes([depth, colour, 0, 0, interlace])
            raw = b''.join(b'\x00' + b'\x00\x00\x00\xff' * 2 for _ in range(2))
            return png.PNG_SIGNATURE + chunk(b'IHDR', header) + chunk(b'IDAT', zlib.compress(raw)) + chunk(b'IEND', b'')

        with self.assertRaises(png.PngError):
            png.decode(build(8, 6, 1))
        with self.assertRaises(png.PngError):
            png.decode(build(16, 6, 0))
        with self.assertRaises(png.PngError):
            png.decode(build(8, 3, 0))
        self.assertEqual(png.decode(build(8, 6, 0)).pixel(0, 0), bytes((0, 0, 0, 255)))

    def test_each_filter_kind_is_undone(self):
        # A 2x1 image whose single scanline uses Sub, Up, Average and Paeth folding.
        def chunk(kind, body):
            return (len(body).to_bytes(4, 'big') + kind + body
                    + (zlib.crc32(kind + body) & 0xFFFFFFFF).to_bytes(4, 'big'))

        header = (2).to_bytes(4, 'big') + (1).to_bytes(4, 'big') + bytes([8, 0, 0, 0, 0])
        grey = bytes([10, 20])
        for filter_kind in (0, 1, 2, 3, 4):
            encoded = bytearray(grey)
            if filter_kind == 1:
                encoded = bytearray([10, 10])
            elif filter_kind == 2:
                encoded = bytearray([10, 20])
            elif filter_kind == 3:
                encoded = bytearray([10, 15])
            elif filter_kind == 4:
                encoded = bytearray([10, 10])
            raw = bytes([filter_kind]) + bytes(encoded)
            data = png.PNG_SIGNATURE + chunk(b'IHDR', header) + chunk(b'IDAT', zlib.compress(raw)) + chunk(b'IEND', b'')
            image = png.decode(data)
            self.assertEqual([image.pixel(0, 0)[0], image.pixel(1, 0)[0]], [10, 20], f'filter {filter_kind}')


class ScenarioCatalogueTest(unittest.TestCase):
    def setUp(self):
        self.document = _support.scenario_document()

    def test_the_three_required_viewports_are_present(self):
        sizes = {(viewport['width'], viewport['height']) for viewport in self.document['viewports']}
        self.assertEqual(sizes, VIEWPORTS)

    def test_every_scenario_has_a_unique_id_and_a_driveable_shape(self):
        identifiers = [scenario['id'] for scenario in self.document['scenarios']]
        self.assertEqual(len(identifiers), len(set(identifiers)))
        for scenario in self.document['scenarios']:
            self.assertTrue(scenario['description'])
            for step in scenario['steps']:
                self.assertEqual(len(step), 1, step)
                kind = next(iter(step))
                self.assertIn(kind, {'openList', 'openForm'})
                if kind == 'openForm':
                    self.assertEqual(len(step[kind]), 2, step)

    def test_the_states_the_brief_names_are_covered(self):
        identifiers = {scenario['id'] for scenario in self.document['scenarios']}
        self.assertLessEqual({'login', 'home-with-tabs', 'list', 'form', 'gallery-catalogues', 'gallery-grid'}, identifiers)

    def test_a_scenario_the_build_cannot_reach_says_so(self):
        for scenario in self.document['scenarios']:
            if 'requires' in scenario:
                self.assertTrue(scenario.get('unavailable'), f'{scenario["id"]} needs a written reason')

    def test_capture_names_are_unique_per_scenario_and_viewport(self):
        names = [_support.capture_name(scenario['id'], viewport['width'], viewport['height'])
                 for viewport in self.document['viewports'] for scenario in self.document['scenarios']]
        self.assertEqual(len(names), len(set(names)))

    def test_the_manifest_contract_is_documented(self):
        readme = (_support.ROOT / 'README.md').read_text(encoding='utf-8')
        for word in ('manifest.json', '--update', '1280x800', 'unavailable'):
            self.assertIn(word, readme)


if __name__ == '__main__':
    unittest.main()
