#!/usr/bin/env python3
"""Phase-one benchmark for Snapcode and in-image social-promo detection.

The fast path is intentionally OpenCV-only.  OCR is optional and only runs for
images that have enough edge structure to plausibly contain overlay text.  The
script emits JSON lines so a later Redlib worker can consume the same contract
without putting OCR on the page-rendering path.
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import statistics
import subprocess
import sys
import time
import struct
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Iterable

import cv2
import numpy as np


SOCIAL_PROMO_TEXT = re.compile(
    r"(?ix)"
    r"(?:\bsnap(?:chat)?\b|\bsnp\b|\btelegram\b|\bt\.me\b|\btg\b)"
)
VOTE_BAIT_TEXT = re.compile(r"(?ix)\bupv(?:ote|otes|oted|oting|te|tes|et|ets)\b")
URGENCY_BAIT_TEXT = re.compile(
    r"(?ix)\bdelet\w*\s+(?:in|within)\s+\[?\d{1,3}\]?\s*(?:h|hr|hrs|hours?|days?)\b"
)


@dataclass(frozen=True)
class Detection:
    signal: str
    confidence: float
    reason: str
    yellow_fraction: float
    black_on_yellow_fraction: float
    dark_component_count: int
    square_marker_count: int
    text_edge_fraction: float
    ocr_text: str = ""
    fast_ms: float = 0.0
    ocr_ms: float = 0.0
    metadata_signal: str = ""

    @property
    def flagged(self) -> bool:
        return self.signal != "none"


def _normalize_image(image: np.ndarray, max_side: int = 320) -> np.ndarray:
    if image is None or image.size == 0:
        raise ValueError("empty image")
    height, width = image.shape[:2]
    scale = min(1.0, max_side / max(height, width))
    if scale == 1.0:
        return image
    return cv2.resize(image, (round(width * scale), round(height * scale)), interpolation=cv2.INTER_AREA)


def _square_markers(mask: np.ndarray) -> int:
    contours, _ = cv2.findContours(mask, cv2.RETR_LIST, cv2.CHAIN_APPROX_SIMPLE)
    height, width = mask.shape
    image_area = height * width
    markers: list[tuple[float, float]] = []
    for contour in contours:
        area = cv2.contourArea(contour)
        if not image_area * 0.0015 <= area <= image_area * 0.08:
            continue
        x, y, w, h = cv2.boundingRect(contour)
        if h == 0 or not 0.67 <= w / h <= 1.5:
            continue
        extent = area / (w * h)
        if extent < 0.35:
            continue
        center = (x + w / 2, y + h / 2)
        if all((center[0] - old[0]) ** 2 + (center[1] - old[1]) ** 2 > 25 for old in markers):
            markers.append(center)
    return len(markers)


def fast_features(image: np.ndarray) -> tuple[float, float, int, int, float]:
    image = _normalize_image(image)
    hsv = cv2.cvtColor(image, cv2.COLOR_BGR2HSV)
    gray = cv2.cvtColor(image, cv2.COLOR_BGR2GRAY)

    # Snapchat yellow is tightly clustered near 60 degrees hue.  The wider
    # range retains compressed screenshots and warm display captures.
    yellow = cv2.inRange(hsv, (20, 125, 120), (40, 255, 255))
    yellow = cv2.morphologyEx(yellow, cv2.MORPH_CLOSE, np.ones((5, 5), np.uint8))
    yellow_fraction = float(cv2.countNonZero(yellow) / yellow.size)

    dark = cv2.inRange(gray, 0, 105)
    yellow_region = cv2.dilate(yellow, np.ones((7, 7), np.uint8))
    dark_on_yellow = cv2.bitwise_and(dark, yellow_region)
    yellow_pixels = max(1, cv2.countNonZero(yellow_region))
    black_on_yellow_fraction = float(cv2.countNonZero(dark_on_yellow) / yellow_pixels)

    component_count, _, stats, _ = cv2.connectedComponentsWithStats(dark_on_yellow, 8)
    min_area = max(2, round(dark.size * 0.00003))
    max_area = round(dark.size * 0.04)
    dark_component_count = sum(min_area <= int(area) <= max_area for area in stats[1:, cv2.CC_STAT_AREA])
    square_marker_count = _square_markers(dark_on_yellow)

    gradient = cv2.morphologyEx(gray, cv2.MORPH_GRADIENT, np.ones((3, 3), np.uint8))
    _, strong_edges = cv2.threshold(gradient, 45, 255, cv2.THRESH_BINARY)
    text_edge_fraction = float(cv2.countNonZero(strong_edges) / strong_edges.size)
    return yellow_fraction, black_on_yellow_fraction, dark_component_count, square_marker_count, text_edge_fraction


def fast_classify(image: np.ndarray) -> Detection:
    started = time.perf_counter_ns()
    yellow, black, components, markers, text_edges = fast_features(image)

    # Snapcodes have a large saturated-yellow field, dense black dots and at
    # least two square corner markers.  A dense component fallback covers
    # screenshots where the corner shapes have been clipped or decorated.
    strong_geometry = yellow >= 0.22 and 0.055 <= black <= 0.48 and markers >= 2 and components >= 18
    dense_geometry = yellow >= 0.38 and 0.07 <= black <= 0.42 and components >= 42
    if strong_geometry or dense_geometry:
        marker_score = min(1.0, markers / 3)
        component_score = min(1.0, components / 70)
        confidence = min(0.99, 0.50 + yellow * 0.25 + marker_score * 0.15 + component_score * 0.10)
        signal = "snapcode"
        reason = "yellow QR-like field"
    else:
        confidence = 0.0
        signal = "none"
        reason = ""

    elapsed = (time.perf_counter_ns() - started) / 1_000_000
    return Detection(signal, confidence, reason, yellow, black, components, markers, text_edges, fast_ms=elapsed)


def should_run_ocr(detection: Detection) -> bool:
    # Skip OCR for already-resolved Snapcodes and low-detail photographs.
    return not detection.flagged and detection.text_edge_fraction >= 0.018


def text_signal(text: str) -> tuple[str, str, float] | None:
    """Return a promotional signal without treating an unlabelled handle as one."""
    if match := SOCIAL_PROMO_TEXT.search(text):
        term = match.group(0).lower()
        return "social_promo", f"text matched {term}", 0.92 if term in {"snapchat", "telegram"} else 0.84
    if match := URGENCY_BAIT_TEXT.search(text):
        return "urgency_bait", f"text matched {match.group(0)}", 0.88
    if match := VOTE_BAIT_TEXT.search(text):
        return "vote_bait", f"text matched {match.group(0)}", 0.86
    return None


def _ocr(image: np.ndarray, tesseract: str) -> tuple[str, float]:
    started = time.perf_counter_ns()
    image = _normalize_image(image, max_side=960)
    height, width = image.shape[:2]
    if max(height, width) < 700:
        scale = 700 / max(height, width)
        image = cv2.resize(image, (round(width * scale), round(height * scale)), interpolation=cv2.INTER_CUBIC)
    # Preserve the natural image for dark/coloured lettering and add a high
    # threshold panel for the thin white overlay text common in spam images.
    gray = cv2.cvtColor(image, cv2.COLOR_BGR2GRAY)
    _, bright_text = cv2.threshold(gray, 184, 255, cv2.THRESH_BINARY)
    bright_text = cv2.cvtColor(bright_text, cv2.COLOR_GRAY2BGR)
    gutter = np.full((image.shape[0], 16, 3), 255, dtype=np.uint8)
    ocr_canvas = np.concatenate((image, gutter, bright_text), axis=1)
    ok, encoded = cv2.imencode(".png", ocr_canvas)
    if not ok:
        return "", 0.0
    proc = subprocess.run(
        [tesseract, "stdin", "stdout", "--psm", "11"],
        input=encoded.tobytes(),
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        check=False,
        timeout=8,
    )
    elapsed = (time.perf_counter_ns() - started) / 1_000_000
    return proc.stdout.decode("utf-8", "replace").strip(), elapsed


def classify(
    image: np.ndarray, *, tesseract: str | None = None, metadata_text: str = ""
) -> Detection:
    metadata_started = time.perf_counter_ns()
    metadata_match = text_signal(metadata_text)
    if metadata_match:
        signal, reason, confidence = metadata_match
        return Detection(
            signal=signal,
            confidence=confidence,
            reason=f"post metadata {reason}",
            yellow_fraction=0.0,
            black_on_yellow_fraction=0.0,
            dark_component_count=0,
            square_marker_count=0,
            text_edge_fraction=0.0,
            fast_ms=(time.perf_counter_ns() - metadata_started) / 1_000_000,
            metadata_signal=metadata_text,
        )
    fast = fast_classify(image)
    if not tesseract or not should_run_ocr(fast):
        return fast
    text, ocr_ms = _ocr(image, tesseract)
    matched = text_signal(text)
    if not matched:
        return Detection(**{**asdict(fast), "ocr_text": text, "ocr_ms": ocr_ms})
    signal, reason, confidence = matched
    return Detection(
        **{
            **asdict(fast),
            "signal": signal,
            "confidence": confidence,
            "reason": f"OCR {reason}",
            "ocr_text": text,
            "ocr_ms": ocr_ms,
        }
    )


def image_paths(path: Path) -> Iterable[Path]:
    if path.is_file():
        yield path
        return
    for suffix in ("*.jpg", "*.jpeg", "*.png", "*.webp"):
        yield from sorted(path.glob(suffix))


def percentile(values: list[float], p: float) -> float:
    if not values:
        return 0.0
    return float(np.percentile(np.asarray(values), p))


def decode_image(encoded: bytes) -> np.ndarray | None:
    """Decode at source resolution before extracting Snapcode geometry.

    JPEG reduced-decode modes can alias ordinary photographic edges into small
    square contours.  The feature pass already bounds its input to 320px, so
    preserving the source geometry here costs only the full JPEG decode.
    """
    return cv2.imdecode(np.frombuffer(encoded, dtype=np.uint8), cv2.IMREAD_COLOR)


def _read_exact(stream, size: int) -> bytes | None:
    data = bytearray()
    while len(data) < size:
        chunk = stream.read(size - len(data))
        if not chunk:
            return None if not data else (_ for _ in ()).throw(EOFError("truncated worker request"))
        data.extend(chunk)
    return bytes(data)


def worker_main() -> int:
    stream = sys.stdin.buffer
    fast_classify(np.zeros((64, 64, 3), dtype=np.uint8))
    while True:
        header = _read_exact(stream, 4)
        if header is None:
            return 0
        metadata_size = struct.unpack(">I", header)[0]
        _metadata = json.loads((_read_exact(stream, metadata_size) or b"{}").decode("utf-8"))
        image_size = struct.unpack(">Q", _read_exact(stream, 8))[0]
        encoded = _read_exact(stream, image_size)
        if encoded is None:
            raise EOFError("missing worker image")
        started = time.perf_counter_ns()
        image = decode_image(encoded)
        if image is None:
            reply = {"flagged": False, "signal": "none", "fast_ms": 0.0, "error": "decode_failed"}
        else:
            detection = fast_classify(image)
            reply = {
                "flagged": detection.flagged,
                "signal": detection.signal,
                "confidence": detection.confidence,
                "fast_ms": detection.fast_ms,
            }
        reply["total_ms"] = (time.perf_counter_ns() - started) / 1_000_000
        print(json.dumps(reply, separators=(",", ":")), flush=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", type=Path, nargs="?", help="image or directory of images")
    parser.add_argument("--worker", action="store_true", help="serve the binary stdin worker protocol")
    parser.add_argument("--ocr", action="store_true", help="run conditional Tesseract OCR")
    parser.add_argument("--title", default="", help="post title/body text to score before OCR (single-image mode)")
    parser.add_argument("--jsonl", type=Path, help="write per-image results")
    parser.add_argument("--warmup", type=int, default=2, help="unreported fast-path warmup passes")
    args = parser.parse_args()

    if args.worker:
        return worker_main()
    if args.path is None:
        parser.error("path is required unless --worker is used")

    tesseract = shutil.which("tesseract") if args.ocr else None
    if args.ocr and not tesseract:
        parser.error("--ocr requested but tesseract was not found")

    paths = list(image_paths(args.path))
    if not paths:
        parser.error(f"no images found under {args.path}")

    first = cv2.imread(str(paths[0]), cv2.IMREAD_COLOR)
    for _ in range(args.warmup):
        fast_classify(first)

    rows = []
    for path in paths:
        image = cv2.imread(str(path), cv2.IMREAD_COLOR)
        if image is None:
            print(f"warning: could not decode {path}", file=sys.stderr)
            continue
        detection = classify(image, tesseract=tesseract, metadata_text=args.title if len(paths) == 1 else "")
        row = {"path": str(path), **asdict(detection), "flagged": detection.flagged}
        rows.append(row)
        if detection.flagged:
            print(json.dumps(row, ensure_ascii=False))

    if args.jsonl:
        args.jsonl.write_text("".join(json.dumps(row, ensure_ascii=False) + "\n" for row in rows), encoding="utf-8")

    fast_times = [row["fast_ms"] for row in rows]
    ocr_times = [row["ocr_ms"] for row in rows if row["ocr_ms"] > 0]
    summary = {
        "opencv": cv2.__version__,
        "images": len(rows),
        "flagged": sum(row["flagged"] for row in rows),
        "snapcodes": sum(row["signal"] == "snapcode" for row in rows),
        "social_promo": sum(row["signal"] == "social_promo" for row in rows),
        "vote_bait": sum(row["signal"] == "vote_bait" for row in rows),
        "urgency_bait": sum(row["signal"] == "urgency_bait" for row in rows),
        "fast_ms": {
            "mean": statistics.fmean(fast_times),
            "p50": percentile(fast_times, 50),
            "p95": percentile(fast_times, 95),
            "max": max(fast_times),
        },
        "ocr": {
            "enabled": bool(tesseract),
            "images": len(ocr_times),
            "mean_ms_when_run": statistics.fmean(ocr_times) if ocr_times else 0.0,
            "amortized_ms_per_image": sum(ocr_times) / max(1, len(rows)),
        },
    }
    print(json.dumps({"summary": summary}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
