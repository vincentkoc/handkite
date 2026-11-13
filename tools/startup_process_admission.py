"""Pure startup identity transitions; no process queries or lifecycle actions.

The caller supplies coherent, trusted job/process evidence and a canonical,
independently pinned executable path. Before each observation it must recheck
the same job, program, run count and process liveness; this helper proves none
of those. Retain every returned state, including refusal; never restart this
window for another observation or job PID.
"""
from __future__ import annotations

from dataclasses import dataclass, replace
import math


XPCPROXY = "/usr/libexec/xpcproxy"
PRE_EXEC_SECONDS = 5.0


def _path(value):
    return (type(value) is str and value.startswith("/") and
            all(part and part not in (".", "..") for part in value.split("/")[1:]) and
            all(char.isprintable() for char in value))


def _identity(pid, birth):
    # Birth is an opaque token from a validated observation, not a parsed date.
    return (type(pid) is int and 0 < pid < 2 ** 31 and
            type(birth) is str and 0 < len(birth) <= 128 and
            birth == birth.strip() and all(char.isprintable() for char in birth))


def _time(value):
    try:
        return type(value) in (int, float) and value >= 0 and math.isfinite(value)
    except OverflowError:
        return False


@dataclass(frozen=True)
class StartupAdmission:
    expected_path: str
    pid: int
    birth: str
    started_at: float
    last_at: float
    status: str

    @classmethod
    def begin(cls, expected_path, pid, birth, now):
        """Pin the initial trusted job identity and monotonic start time once."""
        if not (_path(expected_path) and expected_path != XPCPROXY and
                _identity(pid, birth) and _time(now) and
                math.isfinite(now + PRE_EXEC_SECONDS) and
                now + PRE_EXEC_SECONDS > now):
            raise ValueError("startup_identity")
        return cls(expected_path, pid, birth, now, now, "pending")

    def observe(self, pid, birth, path, now):
        """Return the next immutable state; selected is identity, not readiness."""
        if self.status == "refused":
            return self
        valid = (_identity(pid, birth) and (pid, birth) == (self.pid, self.birth) and
                 _time(now) and now >= self.last_at and _path(path))
        status = "refused"
        if valid:
            if self.status == "selected":
                if path == self.expected_path:
                    status = "selected"
            elif now < self.started_at + PRE_EXEC_SECONDS:
                if path == self.expected_path:
                    status = "selected"
                elif path == XPCPROXY:
                    status = "pending"
        return replace(self, status=status, last_at=now if valid else self.last_at)
