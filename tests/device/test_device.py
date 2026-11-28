"""真机测试辅助逻辑的离线单测，无需 MCP 包或手机。

    python3 -m unittest discover -s tests/device -p 'test_*.py'
"""

import struct
import unittest

import device as dev


class DeviceTests(unittest.TestCase):
    def test_a_run_stops_on_another_device_than_expected(self):
        import os
        d = dev.Device()
        d.udid = "00008020-000925043499002E"
        saved = os.environ.pop("EXPECT_UDID", None)
        try:
            self.assertTrue(dev.is_expected(d), "nothing expected")
            os.environ["EXPECT_UDID"] = "00008020-000925043499002e"
            self.assertTrue(dev.is_expected(d), "the same in either case")
            os.environ["EXPECT_UDID"] = "00008130-AAAA"
            self.assertFalse(dev.is_expected(d))
        finally:
            os.environ.pop("EXPECT_UDID", None)
            if saved is not None:
                os.environ["EXPECT_UDID"] = saved

    def test_selected_device_is_found_in_both_languages(self):
        udid = "00008020-1111111111111111"
        for separator in (",", ", "):
            text = (
                "  Phone (iPhone16,2 iOS 27.2.1, 00008130-2222222222222222)\n"
                f"→ iPad (iPad11,1 iOS 26.7.3{separator}{udid})"
            )
            with self.subTest(separator=separator):
                self.assertEqual(dev.controlled(text), ("iPad11,1", "26.7.3", udid))

    def test_locked_does_not_match_unlocked(self):
        for text in ("锁屏: 已锁屏(操作时会自动解锁)", "Locked: locked (actions unlock automatically)"):
            self.assertTrue(dev.is_locked(text), text)
        for text in ("锁屏: 未锁屏", "Locked: unlocked", "Locked: unknown", ""):
            self.assertFalse(dev.is_locked(text), text)

    def test_tab_bar_can_be_at_top_or_bottom(self):
        for y, off in ((54, ""), (880, ' value="0"')):
            text = (
                f'Button "世界时钟"{off} @(404,{y}) 101x36\n'
                f'Button "闹钟"{off} @(488,{y}) 67x36\n'
                f'Button "秒表" value="1" @(554,{y}) 67x36\n'
                f'Button "计时器"{off} @(628,{y}) 84x36'
            )
            with self.subTest(y=y):
                self.assertEqual([e.label for e in dev.tabs(text)], ["世界时钟", "闹钟", "秒表", "计时器"])

    def test_tab_bar_requires_one_selected_button(self):
        text = 'Button "甲" value="1" @(10,54) 20x36\nButton "乙" value="1" @(40,54) 20x36\nButton "丙" @(70,54) 20x36'
        self.assertEqual(dev.tabs(text), [])

    def test_tab_bar_excludes_separate_toolbar_buttons_on_same_row(self):
        text = (
            'Button "编辑" @(42,54) 59x36\n'
            'Button "世界时钟" value="1" @(404,54) 101x36\n'
            'Button "闹钟" @(488,54) 67x36\n'
            'Button "秒表" @(554,54) 67x36\n'
            'Button "计时器" @(628,54) 84x36\n'
            'Button "添加" @(992,54) 36x36'
        )
        self.assertEqual([e.label for e in dev.tabs(text)], ["世界时钟", "闹钟", "秒表", "计时器"])

    def test_selected_done_button_is_not_mistaken_for_a_tab(self):
        text = (
            'Button "完成" value="1" @(42,54) 59x36\n'
            'Button "世界时钟" @(404,54) 101x36\n'
            'Button "闹钟" @(488,54) 67x36\n'
            'Button "秒表" @(554,54) 67x36\n'
            'Button "计时器" @(628,54) 84x36\n'
            'Button "添加" @(992,54) 36x36'
        )
        self.assertEqual(dev.tabs(text), [])

    def test_ipad_twofold_region_is_valid(self):
        self.assertTrue(dev.enlarged_region((682, 80), 341, 40))
        self.assertTrue(dev.enlarged_region((600, 168), 143, 40))

    def test_region_requires_enlargement_and_correct_shape(self):
        for size in (None, (341, 40), (682, 40), (682, 0)):
            with self.subTest(size=size):
                self.assertFalse(dev.enlarged_region(size, 341, 40))
        self.assertFalse(dev.enlarged_region((682, 80), 0, 40))

    def test_jpeg_and_png_picture_sizes(self):
        # 只需格式头，辅助函数不应解码整张图片。
        png = b"\x89PNG\r\n\x1a\n" + b"\x00\x00\x00\x0dIHDR" + struct.pack(">II", 1024, 768)
        jpeg = b"\xff\xd8\xff\xc0\x00\x11\x08" + struct.pack(">HH", 80, 682) + b"\x03\x00"
        self.assertEqual(dev.jpeg_size(png), (1024, 768))
        self.assertEqual(dev.jpeg_size(jpeg), (682, 80))
        self.assertIsNone(dev.jpeg_size(b"not an image"))

    def test_expected_error_and_bilingual_text_are_checked(self):
        expect = {"error": True, "has": [dev.NO_KEYBOARD]}
        for text in dev.NO_KEYBOARD:
            self.assertEqual(dev.judge(expect, text, True, 0.2, []), [])
        self.assertTrue(dev.judge(expect, "WDA unavailable", True, 0.2, []))
        self.assertTrue(dev.judge(expect, dev.NO_KEYBOARD[0], False, 0.2, []))

    def test_screenshot_expectation_rejects_missing_or_wrong_picture(self):
        expect = {"image": [1024, 768]}
        self.assertEqual(dev.judge(expect, "", False, 0.2, [(1024, 768)]), [])
        self.assertTrue(dev.judge(expect, "", False, 0.2, []))
        self.assertTrue(dev.judge(expect, "", False, 0.2, [(768, 1024)]))

    def test_unlock_time_is_deducted_in_both_languages(self):
        for text in (
            "已按 home(手机锁着,先自动解锁了,用时 8.5 秒)",
            "Pressed home (the phone was locked and was unlocked first, which took 8.5 s)",
        ):
            self.assertEqual(dev.unlock_seconds(text), 8.5)
            self.assertEqual(dev.judge({"max_s": 1}, text, False, 9.2, []), [])
            self.assertTrue(dev.judge({"max_s": 1}, text, False, 10, []))

    def test_manual_unlock_stops_only_on_an_explicit_error(self):
        for text in dev.MANUAL_UNLOCK:
            self.assertTrue(dev.manual_unlock_required(text, True))
            self.assertFalse(dev.manual_unlock_required(text, False))
        for text in ("", "WDA unavailable", "Not found", "调用方已取消"):
            self.assertFalse(dev.manual_unlock_required(text, True))

    def test_unavailable_status_is_not_ready_even_with_screen_size(self):
        self.assertTrue(dev.ready("Screen: 1024x768 points"))
        self.assertFalse(dev.ready("WDA unavailable\nScreen: 1024x768 points"))
        self.assertFalse(dev.ready("WDA 不可用\n屏幕: 1024x768 点"))


if __name__ == "__main__":
    unittest.main()
