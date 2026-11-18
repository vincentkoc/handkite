"""Bounded image evidence for vertical *content* motion, not gesture delivery.

The caller supplies a logical ROI excluding fixed bars/keyboard. This is a
benchmark check, not an input admission or a claim that scrolling has settled.
Only PNG/JPEG are decoded, by macOS sips in a private disposable directory.
"""
from dataclasses import dataclass
import math
import hashlib
import os
from pathlib import Path
import struct
import subprocess
import tempfile


MAX_ENCODED_BYTES = 16 * 1024 * 1024
MAX_PIXELS = 12_000_000
MAX_DIMENSION = 8192
MAX_BMP_BYTES = MAX_PIXELS * 4 + 4096
MIN_SHIFT = 15
MAX_SHIFT = 200


@dataclass(frozen=True)
class MotionResult:
    verified: bool
    reason: str
    shift_y: float = 0.0
    overlap: float = 0.0
    match_fraction: float = 0.0
    changed_fraction: float = 0.0
    mean_error: float = 0.0


def _dimensions(width, height):
    if not (0 < width <= MAX_DIMENSION and 0 < height <= MAX_DIMENSION
            and width * height <= MAX_PIXELS):
        raise ValueError("dimensions")
    return width, height


def _encoded_dimensions(data):
    if not isinstance(data, bytes) or not 0 < len(data) <= MAX_ENCODED_BYTES:
        raise ValueError("encoded_size")
    if data.startswith(b"\x89PNG\r\n\x1a\n"):
        if len(data) < 33 or data[8:16] != b"\0\0\0\rIHDR":
            raise ValueError("png_header")
        return (*_dimensions(*struct.unpack_from(">II", data, 16)), "png")
    if not data.startswith(b"\xff\xd8"):
        raise ValueError("format")
    at = 2
    while at + 4 <= len(data):
        if data[at] != 255:
            raise ValueError("jpeg_header")
        while at < len(data) and data[at] == 255:
            at += 1
        if at >= len(data):
            break
        marker = data[at]
        at += 1
        if marker in (0xD9, 0xDA) or marker == 0:
            break
        if marker in (0x01, *range(0xD0, 0xD9)):
            continue
        if at + 2 > len(data):
            break
        size = struct.unpack_from(">H", data, at)[0]
        if size < 2 or at + size > len(data):
            break
        if marker in (0xC0, 0xC1, 0xC2, 0xC3, 0xC5, 0xC6, 0xC7,
                      0xC9, 0xCA, 0xCB, 0xCD, 0xCE, 0xCF):
            if size < 8:
                break
            height, width = struct.unpack_from(">HH", data, at + 3)
            return (*_dimensions(width, height), "jpg")
        at += size
    raise ValueError("jpeg_header")


@dataclass(frozen=True)
class _Bitmap:
    data: bytes
    width: int
    height: int
    offset: int
    stride: int
    pixel_bytes: int
    top_down: bool

    def gray(self, x, y):
        row = y if self.top_down else self.height - y - 1
        at = self.offset + row * self.stride + x * self.pixel_bytes
        blue, green, red = self.data[at:at + 3]
        return (77 * red + 150 * green + 29 * blue) >> 8


def _bmp(data):
    if len(data) < 54 or len(data) > MAX_BMP_BYTES or data[:2] != b"BM":
        raise ValueError("bmp_header")
    declared, offset = struct.unpack_from("<I4xI", data, 2)
    dib, width, signed_height, planes, bits, compression = struct.unpack_from(
        "<IiiHHI", data, 14)
    width, height = _dimensions(width, abs(signed_height))
    if (dib not in (40, 52, 56, 108, 124) or offset < 14 + dib
            or planes != 1 or bits not in (24, 32) or compression != 0):
        raise ValueError("bmp_layout")
    stride = ((width * bits + 31) // 32) * 4
    if declared != len(data) or offset + stride * height != len(data):
        raise ValueError("bmp_size")
    return _Bitmap(data, width, height, offset, stride, bits // 8,
                   signed_height < 0)


def _geometry(viewport, roi, direction):
    if direction not in ("up", "down") or len(viewport) != 2 or len(roi) != 4:
        raise ValueError("geometry")
    if any(type(v) not in (int, float) or not math.isfinite(v)
           for v in (*viewport, *roi)):
        raise ValueError("geometry")
    vw, vh = viewport
    x, y, width, height = roi
    if (not 0 < vw <= MAX_DIMENSION or not 0 < vh <= MAX_DIMENSION
            or x < 0 or y < 0 or width < 64 or height < 90
            or x + width > vw or y + height > vh):
        raise ValueError("geometry")


def _grid(bitmap, viewport, roi):
    vw, vh = viewport
    x, y, width, height = roi
    sx, sy = bitmap.width / vw, bitmap.height / vh
    if abs(sx - sy) > max(sx, sy) * 0.02:
        raise ValueError("aspect_ratio")
    # Keep raster-row precision: a 3x image can move by a third of a point.
    rows, cols = min(1600, math.ceil(height * sy)), 64
    return [bytes(bitmap.gray(min(bitmap.width - 1, int((x + (c + .5) * width / cols) * sx)),
                              min(bitmap.height - 1, int((y + (r + .5) * height / rows) * sy)))
                  for c in range(cols)) for r in range(rows)]


def _edges(grid):
    height, width = len(grid), len(grid[0])
    return [bytes(abs(grid[y][x] - grid[y][x - 1]) > 20
                  or abs(grid[y][x] - grid[y - 1][x]) > 20
                  for x in range(width)) for y in range(height)]


def _analyze(before, after, logical_height, direction):
    """Pure bounded math on equal, at-most 1600 x 64 grayscale grids."""
    height, width = len(before), len(before[0])
    changed = sum(abs(a - b) > 20 for ra, rb in zip(before, after)
                  for a, b in zip(ra, rb)) / (height * width)
    if changed < .01:
        return MotionResult(False, "insufficient_change", changed_fraction=changed)
    be, ae = _edges(before), _edges(after)
    candidates = []
    scale = logical_height / height
    sign = -1 if direction == "up" else 1
    for magnitude in range(math.ceil(MIN_SHIFT / scale),
                           min(math.floor(MAX_SHIFT / scale), height // 2) + 1):
        shift = sign * magnitude
        start, end = max(0, shift), min(height, height + shift)
        errors = []
        baseline = 0
        bands_x, bands_y = set(), set()
        for y in range(start + 1, end):
            source = y - shift
            for x in range(1, width):
                if not (be[source][x] or ae[y][x]):
                    continue
                error = abs(before[source][x] - after[y][x])
                errors.append(error)
                baseline += abs(before[y][x] - after[y][x])
                if error <= 18:
                    bands_x.add(x * 8 // width)
                    bands_y.add((y - start) * 6 // (end - start))
        if len(errors) < max(80, height * width * .005):
            continue
        mean = sum(errors) / len(errors)
        matched = sum(e <= 18 for e in errors) / len(errors)
        # Require distributed structure and a strong gain over the same-position
        # comparison. Blank backgrounds or one changing caret are not evidence.
        if (mean <= 16 and matched >= .80 and baseline / len(errors) >= 20
                and sum(errors) <= baseline * .40
                and len(bands_x) >= 3 and len(bands_y) >= 3):
            candidates.append((mean, shift, matched, (end - start) / height))
    if not candidates:
        return MotionResult(False, "no_coherent_translation", changed_fraction=changed)
    candidates.sort()
    mean, shift, matched, overlap = candidates[0]
    if any(abs(other_shift - shift) * scale >= 5 and other_mean <= mean + 1
           and other_match >= matched - .02
           for other_mean, other_shift, other_match, _ in candidates[1:]):
        return MotionResult(False, "ambiguous_translation", changed_fraction=changed)
    return MotionResult(True, "verified", shift * scale, overlap, matched, changed, mean)


def _convert_bitmap(data, header, root, index):
    source, target = root / f"{index}.{header[2]}", root / f"{index}.bmp"
    with source.open("xb") as output:
        os.chmod(source, 0o600)
        output.write(data)
    result = subprocess.run(
        ["/usr/bin/sips", "-s", "format", "bmp", str(source), "--out", str(target)],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        timeout=6, check=False)
    if result.returncode != 0:
        raise ValueError("decode_failed")
    os.chmod(target, 0o600)
    with target.open("rb") as output:
        return _bmp(output.read(MAX_BMP_BYTES + 1))


def exact_rgb_region_digest(image, *, viewport, roi):
    """Hash every RGB channel in the logical ROI; no sampling or tolerance.

    Pixel centers select the ROI. Dimensions and pixel bounds bind the digest;
    encoded metadata and alpha do not. Invalid images/geometry always refuse.
    """
    try:
        _geometry(viewport, roi, "up")
        header = _encoded_dimensions(image)
        with tempfile.TemporaryDirectory(prefix="handkite-scroll-") as temporary:
            os.chmod(temporary, 0o700)
            bitmap = _convert_bitmap(image, header, Path(temporary), 0)
        if (bitmap.width, bitmap.height) != header[:2]:
            raise ValueError("dimensions_changed")
        sx, sy = bitmap.width / viewport[0], bitmap.height / viewport[1]
        if abs(sx - sy) > max(sx, sy) * .02:
            raise ValueError("aspect_ratio")
        x, y, width, height = roi
        left, top = max(0, math.ceil(x * sx - .5)), max(0, math.ceil(y * sy - .5))
        right = min(bitmap.width, math.ceil((x + width) * sx - .5))
        bottom = min(bitmap.height, math.ceil((y + height) * sy - .5))
        if right <= left or bottom <= top:
            raise ValueError("empty_pixel_region")
        digest = hashlib.sha256(struct.pack(">6I", bitmap.width, bitmap.height,
                                            left, top, right, bottom))
        for y in range(top, bottom):
            row = y if bitmap.top_down else bitmap.height - y - 1
            start = bitmap.offset + row * bitmap.stride + left * bitmap.pixel_bytes
            pixels = bitmap.data[start:start + (right - left) * bitmap.pixel_bytes]
            for channel in (2, 1, 0):
                digest.update(pixels[channel::bitmap.pixel_bytes])
        return digest.hexdigest()
    except (OSError, ValueError, TypeError, OverflowError, subprocess.TimeoutExpired):
        raise ValueError("invalid_region_image") from None


def verify_vertical_motion(before, after, *, viewport, roi, direction):
    """Verify content moved up/down 15..200 logical pixels inside the ROI.

    Refusals are fixed labels; image bytes and temporary paths never escape.
    An observed displacement proves neither gesture delivery nor quiescence.
    """
    try:
        _geometry(viewport, roi, direction)
        headers = [_encoded_dimensions(data) for data in (before, after)]
        if headers[0][:2] != headers[1][:2]:
            return MotionResult(False, "dimensions_changed")
    except (ValueError, TypeError, OverflowError):
        return MotionResult(False, "invalid_input")
    try:
        with tempfile.TemporaryDirectory(prefix="handkite-scroll-") as temporary:
            os.chmod(temporary, 0o700)
            root = Path(temporary)
            grids = []
            for index, (data, header) in enumerate(zip((before, after), headers)):
                bitmap = _convert_bitmap(data, header, root, index)
                if (bitmap.width, bitmap.height) != header[:2]:
                    return MotionResult(False, "dimensions_changed")
                grids.append(_grid(bitmap, viewport, roi))
            return _analyze(*grids, roi[3], direction)
    except (OSError, ValueError, subprocess.TimeoutExpired):
        return MotionResult(False, "decode_failed")
