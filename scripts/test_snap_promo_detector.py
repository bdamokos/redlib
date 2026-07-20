#!/usr/bin/env python3

import unittest

import cv2
import numpy as np

from snap_promo_detector import classify_encoded, decode_candidate, decode_image, fast_classify, text_signal


class SnapPromoDetectorTests(unittest.TestCase):
    def test_snapcode_geometry_is_flagged(self):
        image = np.full((256, 256, 3), (0, 255, 255), dtype=np.uint8)
        rng = np.random.default_rng(42)
        for x, y in rng.integers(28, 228, size=(110, 2)):
            cv2.circle(image, (int(x), int(y)), 3, (0, 0, 0), -1)
        for x, y in ((30, 30), (190, 30), (30, 190)):
            cv2.rectangle(image, (x, y), (x + 28, y + 28), (0, 0, 0), 5)
        self.assertEqual(fast_classify(image).signal, "snapcode")

    def test_plain_yellow_image_is_not_qr_like(self):
        image = np.full((256, 256, 3), (0, 255, 255), dtype=np.uint8)
        cv2.ellipse(image, (128, 128), (70, 105), 0, 0, 360, (20, 180, 230), -1)
        self.assertEqual(fast_classify(image).signal, "none")

    def test_snapcode_panel_embedded_in_large_poster_is_flagged(self):
        image = np.full((640, 360, 3), (150, 120, 105), dtype=np.uint8)
        panel = np.full((150, 150, 3), (0, 255, 255), dtype=np.uint8)
        rng = np.random.default_rng(43)
        for x, y in rng.integers(18, 132, size=(95, 2)):
            cv2.circle(panel, (int(x), int(y)), 2, (0, 0, 0), -1)
        for x, y in ((18, 18), (105, 18), (18, 105)):
            cv2.rectangle(panel, (x, y), (x + 22, y + 22), (0, 0, 0), 4)
        image[60:210, 105:255] = panel
        self.assertLess(cv2.countNonZero(cv2.inRange(cv2.cvtColor(image, cv2.COLOR_BGR2HSV), (20, 125, 120), (40, 255, 255))) / (640 * 360), 0.20)
        self.assertEqual(fast_classify(image).signal, "snapcode")

    def test_telegram_qr_geometry_is_flagged(self):
        qr = cv2.QRCodeEncoder_create().encode("https://t.me/example")
        qr = cv2.resize(qr, (240, 240), interpolation=cv2.INTER_NEAREST)
        image = np.full((320, 320, 3), (235, 220, 200), dtype=np.uint8)
        image[40:280, 40:280] = cv2.cvtColor(qr, cv2.COLOR_GRAY2BGR)
        self.assertEqual(fast_classify(image).signal, "telegram_qr")

    def test_positive_jpeg_candidate_is_confirmed_at_source_geometry(self):
        image = np.full((416, 750, 3), (25, 170, 205), dtype=np.uint8)
        qr = cv2.QRCodeEncoder_create().encode("https://t.me/example")
        qr = cv2.resize(qr, (240, 240), interpolation=cv2.INTER_NEAREST)
        image[80:320, 255:495] = cv2.cvtColor(qr, cv2.COLOR_GRAY2BGR)
        ok, encoded = cv2.imencode(".jpeg", image)
        self.assertTrue(ok)
        candidate = decode_candidate(encoded.tobytes(), "image/jpeg")
        decoded = decode_image(encoded.tobytes())
        detection = classify_encoded(encoded.tobytes(), "image/jpeg")
        self.assertIsNotNone(candidate)
        self.assertIsNotNone(decoded)
        self.assertLess(candidate.shape[0], decoded.shape[0])
        self.assertEqual(decoded.shape, image.shape)
        self.assertEqual(detection.signal, "telegram_qr")

    def test_unlabelled_username_is_neutral(self):
        self.assertIsNone(text_signal("@ordinary_username"))
        self.assertIsNone(text_signal("ordinary_username"))

    def test_explicit_social_platform_is_flagged(self):
        self.assertEqual(text_signal("add me on snap: someone")[0], "social_promo")
        self.assertEqual(text_signal("Telegram: @someone")[0], "social_promo")
        self.assertEqual(text_signal("TG: @someone")[0], "social_promo")
        self.assertEqual(text_signal("snp _ someone")[0], "social_promo")

    def test_vote_and_urgency_bait_are_flagged(self):
        self.assertEqual(text_signal("upvtes and comment for a reward")[0], "vote_bait")
        self.assertEqual(text_signal("Deleting in [24] hours")[0], "urgency_bait")


if __name__ == "__main__":
    unittest.main()
