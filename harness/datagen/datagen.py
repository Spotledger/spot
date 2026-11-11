#!/usr/bin/env python3
"""datagen: command line entry point for the datagen package.

Runs as `python harness/datagen/datagen.py ...`. Adds this script's directory
to sys.path; when both datagen/ (the package) and this file exist there, the
package wins the import, which is exactly what we want.
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from datagen.cli import main  # noqa: E402

if __name__ == "__main__":
    sys.exit(main())