"""Streaming proof: generating small-trader in a fresh process stays small.

The subprocess measures its own peak RSS with resource.getrusage, which must
stay master-data sized (well under the 1 GB L5 budget) regardless of the
number of invoices written. The measurement is Linux-only: resource is a
Unix module and ru_maxrss is kilobytes on Linux but bytes on macOS, so the
suite is skipped elsewhere (no tools/ci/skip.toml entry needed).
"""

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

_REPO_ROOT = Path(__file__).resolve().parents[3]
_PEAK_RSS_LIMIT_KB = 250_000  # 250 MB, far below the 1 GB L5 ceiling

_CODE = """
import resource, sys
sys.path.insert(0, 'harness/datagen')
from datagen.generate import generate_dataset
from datagen.profile import load_profile
generate_dataset(load_profile('small-trader'), 42, sys.argv[1])
print(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss)
"""


@unittest.skipUnless(
    sys.platform.startswith("linux"), "ru_maxrss in KB and resource are Linux-only"
)
class StreamingMemoryTests(unittest.TestCase):
    def test_small_trader_peak_rss_is_master_sized(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "out"
            proc = subprocess.run(
                [sys.executable, "-c", _CODE, str(out)],
                capture_output=True,
                text=True,
                cwd=str(_REPO_ROOT),
                timeout=300,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            peak_kb = int(proc.stdout.strip())
            # ru_maxrss is kilobytes on Linux
            self.assertLess(peak_kb, _PEAK_RSS_LIMIT_KB, "peak RSS too high: output is not streaming")


if __name__ == "__main__":
    unittest.main()