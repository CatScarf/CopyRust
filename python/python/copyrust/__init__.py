"""Typed, native batch file copying and the CopyRust command-line frontend."""

import sys

from ._native import CopyJob, CopyOptions, Progress, RunOptions, Summary, copy, run_cli

__all__ = [
    "CopyJob",
    "CopyOptions",
    "Progress",
    "RunOptions",
    "Summary",
    "copy",
    "main",
]


def main() -> None:
    """Run the native CLI using the current Python process arguments."""
    raise SystemExit(run_cli(sys.argv[1:]))
