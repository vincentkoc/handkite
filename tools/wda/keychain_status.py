#!/usr/bin/env python3
"""Report login-keychain status in this process's macOS security session."""

import ctypes
import os
from pathlib import Path
import pwd
import sys


def classify(flags: int) -> str:
    if (flags & 0b011) == 0b011:
        return "unlocked"
    if (flags & 0b001) == 0 and (flags & 0b010):
        return "locked"
    return "unavailable"


def main() -> int:
    if sys.platform != "darwin":
        print("keychain_status=unavailable")
        return 2

    security = ctypes.CDLL("/System/Library/Frameworks/Security.framework/Security")
    security.SecKeychainSetUserInteractionAllowed.argtypes = [ctypes.c_bool]
    security.SecKeychainOpen.argtypes = [ctypes.c_char_p, ctypes.POINTER(ctypes.c_void_p)]
    security.SecKeychainGetStatus.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_uint32)]

    if security.SecKeychainSetUserInteractionAllowed(False) != 0:
        print("keychain_status=unavailable")
        return 2

    home = Path(pwd.getpwuid(os.geteuid()).pw_dir)
    path = home / "Library/Keychains/login.keychain-db"
    keychain = ctypes.c_void_p()
    if security.SecKeychainOpen(os.fsencode(path), ctypes.byref(keychain)) != 0:
        print("keychain_status=unavailable")
        return 2

    flags = ctypes.c_uint32()
    if security.SecKeychainGetStatus(keychain, ctypes.byref(flags)) != 0:
        print("keychain_status=unavailable")
        return 2

    state = classify(flags.value)
    print(f"keychain_status={state}")
    return 0 if state != "unavailable" else 2


if __name__ == "__main__":
    raise SystemExit(main())
