import json
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import control
import infer


class ResourceProfiles(unittest.TestCase):
    def test_budget_profiles_and_retry_ceiling(self):
        self.assertEqual(infer.batch_for_budget(30_000_000_000), 4)
        self.assertEqual(infer.batch_for_budget(20_000_000_000), 2)
        self.assertEqual(infer.batch_for_budget(16_000_000_000), 1)
        self.assertIsNone(infer.batch_for_budget(13_999_999_999))
        self.assertEqual(infer.batch_for_budget(30_000_000_000, 2), 2)
        self.assertEqual(infer.batch_for_budget(30_000_000_000, 1), 1)

    @patch('infer.subprocess.check_output')
    def test_capacity_reserves_headroom_and_caps_usage(self, query):
        query.return_value = '0, 45000, 46000, 0\n'
        index, fraction, batch, effective, free = infer.choose_gpu()
        self.assertEqual(index, 0)
        self.assertEqual(batch, 4)
        self.assertEqual(effective, 30_000_000_000)
        self.assertLessEqual(effective, free - 4_000_000_000)
        self.assertAlmostEqual(fraction * (46000 * 1024 ** 2), effective - 2_000_000_000)
        query.return_value = '0, 17000, 46000, 0\n'
        self.assertIsNone(infer.choose_gpu())

    def test_detector_and_postprocessor_use_same_bounded_profile(self):
        predictor = SimpleNamespace(model=SimpleNamespace(batched_grounding_batch_size=16, postprocess_batch_size=16))
        infer.configure_batches(predictor, 2)
        self.assertEqual(predictor.model.batched_grounding_batch_size, 2)
        self.assertEqual(predictor.model.postprocess_batch_size, 2)
        with self.assertRaises(RuntimeError):
            infer.configure_batches(SimpleNamespace(model=SimpleNamespace()), 1)

    def test_only_oom_retries_and_each_retry_reduces_batch(self):
        self.assertEqual(control.retry_profile({'error_kind':'cuda_oom', 'batch_size':4}), 2)
        self.assertEqual(control.retry_profile({'error_kind':'cuda_oom', 'batch_size':2}), 1)
        self.assertIsNone(control.retry_profile({'error_kind':'cuda_oom', 'batch_size':1}))
        self.assertIsNone(control.retry_profile({'error':'Invalid source'}))

    @patch('control.Path.read_bytes')
    def test_process_identity_requires_script_and_job_path(self, read):
        read.return_value = b'python\0/home/u/faris/plate-review-worker/infer.py\0/jobs/abc\0'
        self.assertTrue(control.live_process(123, Path('/jobs/abc')))
        self.assertFalse(control.live_process(123, Path('/jobs/other')))
        read.return_value = b'python\0unrelated.py\0/jobs/abc\0'
        self.assertFalse(control.live_process(123, Path('/jobs/abc')))


if __name__ == '__main__':
    unittest.main()
