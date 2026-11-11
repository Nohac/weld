"""Repository paths and the native Python used by optional GI fixtures."""

import os
import shutil
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def portal_python() -> str:
    explicit = os.environ.get("WELD_TEST_PORTAL_PYTHON")
    if explicit:
        return explicit
    # uv prepends its venv, which deliberately has no native GI/GStreamer bindings.
    search = os.pathsep.join(
        entry
        for entry in os.get_exec_path()
        if not Path(entry).absolute().is_relative_to(Path(sys.prefix))
    )
    native = shutil.which("python3", path=search)
    if not native:
        raise RuntimeError(
            "Set WELD_TEST_PORTAL_PYTHON to a Python with PyGObject/GStreamer"
        )
    return native
