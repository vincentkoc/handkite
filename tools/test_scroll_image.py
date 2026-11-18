import random
from pathlib import Path
import subprocess
import tempfile
import struct
import sys
import unittest
import zlib
from unittest.mock import patch

from scroll_image import (_analyze, _bmp, _encoded_dimensions, _grid,
                          verify_vertical_motion, exact_rgb_region_digest)


def content(width=64, height=320):
    rng = random.Random(42)
    rows = [bytearray([240] * width) for _ in range(height)]
    # Nonperiodic, distributed text-like blocks with blank gutters.
    for y in range(4, height - 8, 11):
        for x in range(3, width - 6, 8):
            shade = rng.randrange(15, 170)
            for yy in range(y, y + rng.randrange(2, 6)):
                for xx in range(x, x + rng.randrange(3, 7)):
                    rows[yy][xx] = shade
    return [bytes(row) for row in rows]


def translated(rows, shift):
    return [rows[y - shift] if 0 <= y - shift < len(rows) else bytes([240] * len(rows[0]))
            for y in range(len(rows))]


def png(rows):
    width, height = len(rows[0]), len(rows)
    def chunk(kind, data):
        return struct.pack('>I', len(data)) + kind + data + struct.pack(
            '>I', zlib.crc32(kind + data) & 0xffffffff)
    raw = b''.join(b'\0' + bytes(v for pixel in row for v in (pixel,) * 3) for row in rows)
    return (b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', width, height, 8, 2, 0, 0, 0))
            + chunk(b'IDAT', zlib.compress(raw)) + chunk(b'IEND', b''))


def bmp(rows, bits=24, top_down=False):
    width, height = len(rows[0]), len(rows)
    stride = (width * bits // 8 + 3) // 4 * 4
    data = b''.join(b''.join(bytes([v, v, v] + ([255] if bits == 32 else [])) for v in row)
                    + bytes(stride - width * bits // 8)
                    for row in (rows if top_down else reversed(rows)))
    size = 54 + len(data)
    return (b'BM' + struct.pack('<IHHI', size, 0, 0, 54)
            + struct.pack('<IiiHHIIiiII', 40, width, -height if top_down else height,
                          1, bits, 0, len(data), 0, 0, 0, 0) + data)


class MotionTests(unittest.TestCase):
    def test_translation_both_directions_and_scaled_pixels(self):
        before = content()
        for shift, direction in ((-40, 'up'), (35, 'down')):
            with self.subTest(direction=direction):
                result = _analyze(before, translated(before, shift), 320, direction)
                self.assertTrue(result.verified, result)
                self.assertEqual(result.shift_y, shift)
                self.assertGreater(result.overlap, .5)
        result = _analyze(before, translated(before, -40), 640, 'up')
        self.assertTrue(result.verified, result)
        self.assertEqual(result.shift_y, -80)

    def test_native_pixel_translation_at_three_times_scale(self):
        before = content(width=192, height=975)
        for shift, direction in ((304, 'down'), (-304, 'up')):
            with self.subTest(shift=shift):
                images = [_bmp(bmp(rows))
                          for rows in (before, translated(before, shift))]
                grids = [_grid(image, (64, 325), (0, 0, 64, 325))
                         for image in images]
                result = _analyze(*grids, 325, direction)
                self.assertTrue(result.verified, result)
                self.assertAlmostEqual(result.shift_y, shift / 3)

    def test_scaled_raster_keeps_negative_motion_checks(self):
        before = content(width=192, height=975)
        rng = random.Random(19)
        unrelated = [bytes(rng.randrange(256) for _ in row) for row in before]
        cases = [(translated(before, 304), 'up'),
                 (unrelated, 'down'), (translated(before, 42), 'down'),
                 (translated(before, 603), 'down')]
        old = _grid(_bmp(bmp(before)), (64, 325), (0, 0, 64, 325))
        for after, direction in cases:
            with self.subTest(direction=direction, changed=after is unrelated):
                new = _grid(_bmp(bmp(after)), (64, 325), (0, 0, 64, 325))
                self.assertFalse(_analyze(old, new, 325, direction).verified)

    def test_identical_and_caret_blink_fail(self):
        before = content()
        caret = [bytearray(row) for row in before]
        for y in range(100, 125):
            caret[y][20] = 0
        for after in (before, [bytes(row) for row in caret]):
            self.assertFalse(_analyze(before, after, 320, 'up').verified)

    def test_noise_horizontal_and_wrong_direction_fail(self):
        before = content()
        rng = random.Random(11)
        noise = [bytes(rng.randrange(256) for _ in row) for row in before]
        horizontal = [bytes([240] * 10) + row[:-10] for row in before]
        for after, direction in ((noise, 'up'), (horizontal, 'up'),
                                 (translated(before, 40), 'up')):
            with self.subTest(direction=direction):
                self.assertFalse(_analyze(before, after, 320, direction).verified)

    def test_small_motion_large_motion_and_periodic_ambiguity_fail(self):
        before = content(height=600)
        for shift in (-5, -220):
            self.assertFalse(_analyze(before, translated(before, shift), 600, 'up').verified)
        repeated = content(height=80) * 4
        self.assertFalse(_analyze(repeated, translated(repeated, -40), 320, 'up').verified)

    def test_one_narrow_moving_object_is_not_content_scroll(self):
        before = [bytes([240] * 64) for _ in range(320)]
        after = [bytearray(row) for row in before]
        for y in range(90, 240):
            for x in range(30, 33):
                after[y][x] = 0
        self.assertFalse(_analyze(before, [bytes(r) for r in after], 320, 'up').verified)

    def test_bmp_24_32_top_down_bottom_up_and_padding(self):
        rows = [bytes([0, 50, 200]), bytes([10, 40, 250])]
        for bits in (24, 32):
            for top_down in (False, True):
                image = _bmp(bmp(rows, bits, top_down))
                self.assertEqual([[image.gray(x, y) for x in range(3)] for y in range(2)],
                                 [list(row) for row in rows])

    def test_malformed_bmp_failures(self):
        valid = bmp(content(height=100))
        variants = [b'', valid[:-1], valid + b'\0']
        for offset, value in ((10, 30), (30, 3), (18, 0), (22, 0)):
            bad = bytearray(valid)
            struct.pack_into('<I', bad, offset, value)
            variants.append(bytes(bad))
        for data in variants:
            with self.assertRaises(ValueError):
                _bmp(data)

    def test_roi_excludes_stationary_chrome_and_maps_scaled_pixels(self):
        before = content(height=320)
        after = translated(before, -40)
        # Fixed bars outside the explicit ROI are deliberately not evidence.
        bars = [bytes([100] * 64) for _ in range(40)]
        old = _bmp(bmp(bars + before + bars))
        new = _bmp(bmp(bars + after + bars))
        grids = [_grid(image, (64, 400), (0, 40, 64, 320)) for image in (old, new)]
        self.assertTrue(_analyze(*grids, 320, 'up').verified)
        # Changes only outside the ROI cannot establish a scroll.
        changed_bars = [bytes([200] * 64) for _ in range(40)]
        new = _bmp(bmp(changed_bars + before + changed_bars))
        grids = [_grid(image, (64, 400), (0, 40, 64, 320)) for image in (old, new)]
        self.assertFalse(_analyze(*grids, 320, 'up').verified)
        with self.assertRaises(ValueError):
            _grid(old, (100, 400), (0, 40, 64, 320))

    def test_exact_region_uses_every_rgb_pixel_not_gray_or_sampling(self):
        rows = [bytes([100] * 128) for _ in range(240)]
        original = _bmp(bmp(rows))
        def changed(x, y):
            data = bytearray(original.data)
            # One red-channel level need not change rounded grayscale.
            at = original.offset + (original.height - 1 - y) * original.stride + x * 3 + 2
            data[at] += 1
            return _bmp(bytes(data))
        image = png(rows)
        def digest(bitmap):
            with patch('scroll_image._convert_bitmap', return_value=bitmap):
                return exact_rgb_region_digest(image, viewport=(128, 240), roi=(16, 30, 96, 150))
        reference = digest(original)
        self.assertEqual(reference, digest(changed(5, 5)))
        self.assertEqual(original.gray(81, 131), changed(81, 131).gray(81, 131))
        self.assertNotEqual(reference, digest(changed(81, 131)))
        rgba = _bmp(bmp(rows, bits=32, top_down=True))
        self.assertEqual(reference, digest(rgba))
        alpha = bytearray(rgba.data)
        alpha[rgba.offset + 131 * rgba.stride + 81 * 4 + 3] = 0
        self.assertEqual(reference, digest(_bmp(bytes(alpha))))
        for x, y in ((16, 30), (111, 179)):
            self.assertNotEqual(reference, digest(changed(x, y)))
        for x, y in ((15, 30), (112, 179), (16, 29), (111, 180)):
            self.assertEqual(reference, digest(changed(x, y)))

    def test_exact_region_invalid_geometry_or_decode_refuses(self):
        image = png(content())
        with patch('scroll_image._convert_bitmap') as convert:
            for roi in ((0, 0, 65, 320), (-1, 0, 64, 320)):
                with self.assertRaisesRegex(ValueError, 'invalid_region_image'):
                    exact_rgb_region_digest(image, viewport=(64, 320), roi=roi)
            convert.assert_not_called()
        with self.assertRaisesRegex(ValueError, 'invalid_region_image'):
            exact_rgb_region_digest(b'bad image', viewport=(64, 320), roi=(0, 0, 64, 320))
        with patch('scroll_image._convert_bitmap', side_effect=ValueError('private detail')):
            with self.assertRaisesRegex(ValueError, '^invalid_region_image$'):
                exact_rgb_region_digest(image, viewport=(64, 320), roi=(0, 0, 64, 320))

    @unittest.skipUnless(sys.platform == 'darwin', 'native sips is macOS only')
    def test_native_exact_region_ignores_only_pixels_outside_roi(self):
        rows = [bytes([100] * 128) for _ in range(240)]
        original = png(rows)
        outside = [bytearray(row) for row in rows]
        outside[5][5] += 1
        inside = [bytearray(row) for row in rows]
        inside[131][81] += 1
        kwargs = dict(viewport=(128, 240), roi=(16, 30, 96, 150))
        reference = exact_rgb_region_digest(original, **kwargs)
        self.assertEqual(reference, exact_rgb_region_digest(png(outside), **kwargs))
        self.assertNotEqual(reference, exact_rgb_region_digest(png(inside), **kwargs))

    def test_input_bounds_refuse_before_native_decoder(self):
        valid = png(content())
        cases = [dict(viewport=(64, 320), roi=(0, 0, 65, 320), direction='up'),
                 dict(viewport=(64, 320), roi=(0, -1, 64, 320), direction='up'),
                 dict(viewport=(64, 320), roi=(0, 0, 64, 320), direction='left'),
                 dict(viewport=(64, float('nan')), roi=(0, 0, 64, 320), direction='up')]
        with patch('scroll_image.subprocess.run') as run:
            for args in cases:
                self.assertEqual(verify_vertical_motion(valid, valid, **args).reason, 'invalid_input')
            self.assertEqual(verify_vertical_motion(b'junk', valid, viewport=(64, 320),
                             roi=(0, 0, 64, 320), direction='up').reason, 'invalid_input')
            run.assert_not_called()

    def test_encoded_header_dimension_bound_and_jpeg(self):
        data = bytearray(png(content()))
        struct.pack_into('>I', data, 16, 100_000)
        with self.assertRaises(ValueError):
            _encoded_dimensions(bytes(data))
        jpeg = b'\xff\xd8\xff\xc0' + struct.pack('>HBHHB', 8, 8, 320, 64, 1)
        self.assertEqual(_encoded_dimensions(jpeg), (64, 320, 'jpg'))
        for data in (jpeg[:-2], b'\xff\xd8\xff\xe0\0\1', b'\x89PNG\r\n\x1a\n'):
            with self.assertRaises(ValueError):
                _encoded_dimensions(data)

    @unittest.skipUnless(sys.platform == 'darwin', 'native sips is macOS only')
    def test_native_png_conversion_and_translation(self):
        before = content()
        result = verify_vertical_motion(png(before), png(translated(before, -40)),
                                        viewport=(64, 320), roi=(0, 0, 64, 320), direction='up')
        self.assertTrue(result.verified, result)
        self.assertEqual(result.shift_y, -40)

    @unittest.skipUnless(sys.platform == 'darwin', 'native sips is macOS only')
    def test_native_jpeg_conversion_and_translation(self):
        before = content()
        with tempfile.TemporaryDirectory() as directory:
            encoded = []
            for index, rows in enumerate((before, translated(before, 35))):
                source = Path(directory) / f'{index}.png'
                target = Path(directory) / f'{index}.jpg'
                source.write_bytes(png(rows))
                subprocess.run(['/usr/bin/sips', '-s', 'format', 'jpeg', str(source),
                                '--out', str(target)], check=True, timeout=6,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                encoded.append(target.read_bytes())
            result = verify_vertical_motion(*encoded, viewport=(64, 320),
                                            roi=(0, 0, 64, 320), direction='down')
        self.assertTrue(result.verified, result)
        self.assertEqual(result.shift_y, 35)

    def test_native_decode_failure_and_timeout_have_fixed_results(self):
        valid = png(content())
        for outcome in (subprocess.CompletedProcess([], 1),
                        subprocess.TimeoutExpired('private-command', 6)):
            with patch('scroll_image.subprocess.run') as run:
                if isinstance(outcome, Exception):
                    run.side_effect = outcome
                else:
                    run.return_value = outcome
                result = verify_vertical_motion(valid, valid, viewport=(64, 320),
                                                roi=(0, 0, 64, 320), direction='up')
                self.assertEqual(result.reason, 'decode_failed')
                self.assertFalse(result.verified)
                self.assertNotIn('private', repr(result))


if __name__ == '__main__':
    unittest.main()
