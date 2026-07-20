#!/usr/bin/env python3

import unittest

import cv2
import numpy as np

from snap_promo_detector import fast_classify, text_signal


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
