from __future__ import annotations

import errno
import json
import math
import os
import shutil
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

DEFAULT_MIN_FREE_GIB = 8.0
_RETRYABLE_STATUS_ERRNOS = {errno.ENOSPC, errno.EDQUOT}


def minimum_free_bytes() -> int:
    raw = os.environ.get("PERFORMANCE_MIN_FREE_GIB", str(DEFAULT_MIN_FREE_GIB))
    value = float(raw)
    if not math.isfinite(value) or value < 0:
        raise ValueError("PERFORMANCE_MIN_FREE_GIB must be non-negative")
    return int(value * 1024**3)


def storage_preflight(path: Path) -> int:
    try:
        free = shutil.disk_usage(path).free
        required = minimum_free_bytes()
    except (OSError, ValueError):
        return 2
    return 75 if free < required else 0


def write_status(path: Path, **fields: Any) -> bool:
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {"updated_at": datetime.now(timezone.utc).isoformat(), "pid": os.getpid(), **fields}
    tmp = path.with_suffix(path.suffix + ".tmp")
    try:
        tmp.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")
        tmp.replace(path)
    except OSError as error:
        try:
            tmp.unlink(missing_ok=True)
        except OSError:
            pass
        if fields.get("state") == "waiting-for-idle" and error.errno in _RETRYABLE_STATUS_ERRNOS:
            return False
        raise
    return True
