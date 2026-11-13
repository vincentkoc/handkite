"""Retained filesystem and native identity guards for fixed supervised modes."""
from __future__ import annotations
import ctypes
import errno
import hashlib
import json
import os
import platform
import stat
import sys
from contextlib import contextmanager
import fcntl
import re
import secrets

class Refused(Exception):
    pass

def require(ok, category):
    if not ok:
        raise Refused(category)


def exact_json(raw):
    def pairs(items):
        result = {}
        for key, value in items:
            require(key not in result, "duplicate_field")
            result[key] = value
        return result
    return json.loads(raw, object_pairs_hook=pairs,
                      parse_constant=lambda _: require(False, "invalid_number"))


def fields(value, expected):
    require(type(value) is dict and set(value) == set(expected), "schema")


def digest(value):
    require(type(value) is str and re.fullmatch(r"[0-9a-f]{64}", value), "digest")
    return value


def pin(value):
    require(type(value) is list and len(value) == 9 and
            all(type(n) is int and 0 <= n < 2 ** 64 for n in value), "fingerprint")
    return tuple(value)


def read_bytes(fd, cap):
    size = os.fstat(fd).st_size
    require(0 <= size <= cap, "file_bound")
    data = os.pread(fd, size + 1, 0)
    require(len(data) == size, "file_read")
    return data


class PathAnchor:
    """Retain every ancestor; fresh walks must reach the same directories."""
    def __init__(self, path, home):
        require(type(path) is str and path.startswith(home + "/"), "account_path")
        parts = path.split("/")[1:]
        require(all(part and part not in (".", "..") for part in parts), "path")
        self.parts, self.home = parts, home
        self.fds, self.pins = [], []
        try:
            fd = os.open("/", os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
            self.fds.append(fd)
            self.pins.append(ancestor(fd))
            for index, part in enumerate(parts[:-1]):
                fd = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW |
                             os.O_CLOEXEC, dir_fd=fd)
                self.fds.append(fd)
                current = ancestor(fd)
                if index + 1 >= len(home.split("/")) - 1:
                    require(os.fstat(fd).st_uid == os.geteuid(), "ancestor_owner")
                self.pins.append(current)
        except BaseException:
            self.close()
            raise

    @property
    def parent(self):
        return self.fds[-1]

    @property
    def name(self):
        return self.parts[-1]

    def check(self):
        fresh = os.open("/", os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
        try:
            for index, (held, original) in enumerate(zip(self.fds, self.pins)):
                if index:
                    next_fd = os.open(self.parts[index - 1], os.O_RDONLY |
                                      os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                                      dir_fd=fresh)
                    os.close(fresh)
                    fresh = next_fd
                require(ancestor(held) == original == ancestor(fresh), "ancestor_changed")
        finally:
            os.close(fresh)

    def close(self):
        for fd in self.fds:
            os.close(fd)
        self.fds = []


class HeldFile:
    def __init__(self, path, home, mode, sha=None, expected=None, cap=20*1024*1024):
        self.anchor = PathAnchor(path, home)
        self.fd = None
        self.cap, self.mode, self.sha = cap, mode, sha
        try:
            self.fd = os.open(self.anchor.name, os.O_RDONLY | os.O_NOFOLLOW |
                              os.O_NONBLOCK | os.O_CLOEXEC, dir_fd=self.anchor.parent)
            self.original = fp(os.fstat(self.fd))
            if expected is not None:
                require(self.original == tuple(expected), "file_pin")
            self.check()
        except BaseException:
            self.close()
            raise

    def check(self):
        self.anchor.check()
        meta = os.fstat(self.fd)
        require(stat.S_ISREG(meta.st_mode) and meta.st_uid == os.geteuid() and
                meta.st_nlink == 1 and stat.S_IMODE(meta.st_mode) == self.mode,
                "file_policy")
        no_acl(self.fd)
        require(fp(meta) == self.original == fp(os.stat(self.anchor.name,
                dir_fd=self.anchor.parent, follow_symlinks=False)), "file_changed")
        data = read_bytes(self.fd, self.cap)
        if self.sha is not None:
            require(hashlib.sha256(data).hexdigest() == self.sha, "file_hash")
        require(fp(os.fstat(self.fd)) == self.original, "file_changed")
        require(fp(os.stat(self.anchor.name, dir_fd=self.anchor.parent,
                           follow_symlinks=False)) == self.original, "file_changed")
        no_acl(self.fd)
        self.anchor.check()
        return data

    def close(self):
        if self.fd is not None:
            os.close(self.fd)
            self.fd = None
        self.anchor.close()


class StopSet:
    """The checked receipt supplies markers; every state entry is retained separately."""
    def __init__(self, home, receipt):
        fields(receipt, ("version", "directory", "markers"))
        require(type(receipt["version"]) is int and receipt["version"] == 1, "receipt_version")
        require(type(receipt["markers"]) is list and 1 <= len(receipt["markers"]) <= 256,
                "marker_bound")
        self.anchor = PathAnchor(home + "/Library/Application Support/Handkite/state/iphone/_", home)
        self.fd = self.anchor.parent
        self.held, self.moved, self.hold = {}, False, None
        self.active = "UNLOCK-DISABLED"
        self.created = None
        try:
            directory_pin = receipt["directory"]
            fields(directory_pin, ("device", "inode", "uid", "gid", "mode"))
            actual = os.fstat(self.fd)
            require(directory_pin == dict(zip(("device", "inode", "uid", "gid", "mode"),
                (actual.st_dev, actual.st_ino, actual.st_uid, actual.st_gid, actual.st_mode))),
                "state_identity")
            require(stat.S_IMODE(actual.st_mode) == 0o700, "state_private")
            no_acl(self.fd)
            self.baseline = self.inventory()
            for item in receipt["markers"]:
                fields(item, ("name", "identity", "size", "modified", "sha256"))
                name = item["name"]
                require(type(name) is str and (name == self.active or
                        re.fullmatch(r"UNLOCK-DISABLED\.[A-Za-z0-9._-]+\.hold", name))
                        and len(name) <= 240 and name not in self.held, "marker_name")
                held = HeldFile(home + "/Library/Application Support/Handkite/state/iphone/" + name,
                                home, 0o600, digest(item["sha256"]), cap=4096)
                self.held[name] = held
                meta = os.fstat(held.fd)
                require(item["identity"] == dict(zip(("device", "inode", "uid", "gid", "mode"),
                        (meta.st_dev, meta.st_ino, meta.st_uid, meta.st_gid, meta.st_mode))) and
                        item["size"] == meta.st_size and
                        item["modified"] == [meta.st_mtime_ns // 10**9, meta.st_mtime_ns % 10**9],
                        "marker_receipt")
            require(self.active in self.held and set(self.held) ==
                    {name for name in self.baseline if name.startswith(self.active)}, "marker_inventory")
            self.directory_pin = fp(actual)
            self.check()
        except BaseException:
            self.close()
            raise

    def inventory(self):
        names = os.listdir(self.fd)
        require(len(names) <= 1024, "state_bound")
        return {name: fp(os.stat(name, dir_fd=self.fd, follow_symlinks=False)) for name in names}

    def check(self):
        self.anchor.check()
        no_acl(self.fd)
        require(fp(os.fstat(self.fd)) == self.directory_pin and
                self.inventory() == self.baseline, "state_changed")
        for held in self.held.values():
            held.check()
        if self.created:
            self.created.check()
        require(fp(os.fstat(self.fd)) == self.directory_pin and
                self.inventory() == self.baseline, "state_changed")

    def _renamed(self, source, destination):
        held = self.held[source]
        current = fp(os.fstat(held.fd))
        require(current[:8] == held.original[:8], "renamed_marker_changed")
        held.original = current
        held.anchor.parts[-1] = destination
        self.held[destination] = self.held.pop(source)
        expected = dict(self.baseline)
        del expected[source]
        expected[destination] = current
        require(self.inventory() == expected, "rename_inventory")
        self.baseline = expected
        self.directory_pin = fp(os.fstat(self.fd))
        os.fsync(self.fd)
        self.check()

    def rotate(self):
        self.check()
        self.hold = "UNLOCK-DISABLED.supervised-" + secrets.token_hex(16) + ".hold"
        # The caller holds the original runtime lock across this check and syscall.
        self.check()
        if lib.renameatx_np(self.fd, self.active.encode(), self.fd, self.hold.encode(), 4):
            raise Refused("rename_unproven")
        self.moved = True
        self._renamed(self.active, self.hold)

    def restore(self):
        self.check()
        require(self.moved and self.active not in self.baseline, "restore_refused")
        if lib.renameatx_np(self.fd, self.hold.encode(), self.fd, self.active.encode(), 4):
            raise Refused("restore_unproven")
        self._renamed(self.hold, self.active)
        self.moved = False

    def accept_cleared_attempt(self):
        """Only after the owned operator's success/proven-zero cleanup contract.

        Its active marker creation/removal changes directory timestamps. Accept
        that change only with the exact post-rotation inventory and original held
        files; all ordinary checks continue to require the full timestamp pin.
        """
        require(self.moved and self.active not in self.baseline, "attempt_not_rotated")
        self.anchor.check()
        no_acl(self.fd)
        current = fp(os.fstat(self.fd))
        require(current[:6] == self.directory_pin[:6] and
                self.inventory() == self.baseline, "attempt_inventory")
        for held in self.held.values():
            held.check()
        require(fp(os.fstat(self.fd)) == current and self.inventory() == self.baseline,
                "attempt_inventory")
        self.directory_pin = current
        self.check()

    def block(self):
        """Never replace an active entry, even when it cannot be validated."""
        self.anchor.check()
        if self.active in os.listdir(self.fd):
            return
        fd = os.open(self.active, os.O_WRONLY | os.O_CREAT | os.O_EXCL |
                     os.O_NOFOLLOW | os.O_CLOEXEC, 0o600, dir_fd=self.fd)
        try:
            require(os.write(fd, b"uncertain\n") == 10, "stop_write")
            os.fsync(fd)
        finally:
            os.close(fd)
        os.fsync(self.fd)

    def close(self):
        for held in self.held.values():
            held.close()
        if self.created:
            self.created.close()
        self.anchor.close()


@contextmanager
def runtime_lock(held):
    held.check()
    fd = os.open(held.anchor.name, os.O_RDWR | os.O_NOFOLLOW | os.O_CLOEXEC |
                 os.O_NONBLOCK, dir_fd=held.anchor.parent)
    try:
        require(fp(os.fstat(fd)) == held.original, "runtime_lock_changed")
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        held.check()
        yield
    finally:
        os.close(fd)

class Timespec(ctypes.Structure):
    _fields_ = [('tv_sec', ctypes.c_long), ('tv_nsec', ctypes.c_long)]

class DarwinStat(ctypes.Structure):
    _fields_ = [('st_dev', ctypes.c_int32), ('st_mode', ctypes.c_uint16), ('st_nlink', ctypes.c_uint16), ('st_ino', ctypes.c_uint64), ('st_uid', ctypes.c_uint32), ('st_gid', ctypes.c_uint32), ('st_rdev', ctypes.c_int32), ('st_atimespec', Timespec), ('st_mtimespec', Timespec), ('st_ctimespec', Timespec), ('st_birthtimespec', Timespec), ('st_size', ctypes.c_int64), ('st_blocks', ctypes.c_int64), ('st_blksize', ctypes.c_int32), ('st_flags', ctypes.c_uint32), ('st_gen', ctypes.c_uint32), ('st_lspare', ctypes.c_int32), ('st_qspare', ctypes.c_int64 * 2)]

class Bsd(ctypes.Structure):
    _fields_ = [(n, ctypes.c_uint32) for n in ('flags', 'status', 'xstatus', 'pid', 'ppid', 'uid', 'gid', 'ruid', 'rgid', 'svuid', 'svgid', 'rfu')] + [('comm', ctypes.c_char * 16), ('name', ctypes.c_char * 32)] + [(n, ctypes.c_uint32) for n in ('nfiles', 'pgid', 'jobc', 'tdev', 'tpgid', 'nice')] + [('startsec', ctypes.c_uint64), ('startusec', ctypes.c_uint64)]

if sys.platform == "darwin":
    require(ctypes.sizeof(DarwinStat) == 144, 'native_metadata')
    lib = ctypes.CDLL('/usr/lib/libSystem.B.dylib', use_errno=True)
    lib.filesec_init.argtypes = []
    lib.filesec_init.restype = ctypes.c_void_p
    lib.filesec_free.argtypes = [ctypes.c_void_p]
    lib.filesec_free.restype = None
    lib.filesec_query_property.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.POINTER(ctypes.c_int)]
    lib.filesec_query_property.restype = ctypes.c_int
    lib.filesec_get_property.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_void_p]
    lib.filesec_get_property.restype = ctypes.c_int
    fstatx = getattr(lib, 'fstatx_np$INODE64' if platform.machine() == 'x86_64' else 'fstatx_np')
    fstatx.argtypes = [ctypes.c_int, ctypes.POINTER(DarwinStat), ctypes.c_void_p]
    fstatx.restype = ctypes.c_int
    lib.acl_valid.argtypes = [ctypes.c_void_p]
    lib.acl_valid.restype = ctypes.c_int
    lib.acl_get_entry.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.POINTER(ctypes.c_void_p)]
    lib.acl_get_entry.restype = ctypes.c_int
    lib.acl_free.argtypes = [ctypes.c_void_p]
    lib.acl_free.restype = ctypes.c_int
    lib.acl_get_tag_type.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_int)]
    lib.acl_get_tag_type.restype = ctypes.c_int
    lib.renameatx_np.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint]
    lib.renameatx_np.restype = ctypes.c_int

def fp(s):
    return (s.st_dev, s.st_ino, s.st_mode, s.st_uid, s.st_gid, s.st_nlink, s.st_size, s.st_mtime_ns, s.st_ctime_ns)

def no_acl(fd):
    before = os.fstat(fd)
    security = lib.filesec_init()
    require(security, 'native_metadata')
    acl = ctypes.c_void_p()
    owns_acl = False
    try:
        native = DarwinStat()
        ctypes.set_errno(0)
        require(fstatx(fd, ctypes.byref(native), security) == 0, 'native_metadata')
        require((native.st_dev, native.st_ino, native.st_mode, native.st_uid, native.st_gid, native.st_nlink, native.st_size) == (before.st_dev, before.st_ino, before.st_mode, before.st_uid, before.st_gid, before.st_nlink, before.st_size), 'native_metadata')
        present = ctypes.c_int(-1)
        ctypes.set_errno(0)
        require(lib.filesec_query_property(security, 5, ctypes.byref(present)) == 0, 'native_metadata')
        if present.value:
            ctypes.set_errno(0)
            require(lib.filesec_get_property(security, 5, ctypes.byref(acl)) == 0 and acl.value not in (None, 0, 1), 'native_metadata')
            owns_acl = True
    finally:
        lib.filesec_free(security)
    if owns_acl:
        try:
            ctypes.set_errno(0)
            require(lib.acl_valid(acl) == 0, 'native_metadata')
            entry = ctypes.c_void_p()
            ctypes.set_errno(0)
            result = lib.acl_get_entry(acl, 0, ctypes.byref(entry))
            number = ctypes.get_errno()
            require(result == -1 and number == errno.EINVAL, 'native_metadata')
        finally:
            require(lib.acl_free(acl) == 0, 'native_metadata')
    require(fp(os.fstat(fd)) == fp(before), 'native_metadata')

def ancestor_acl(fd, private):
    before = os.fstat(fd)
    security = lib.filesec_init()
    require(security, 'native_metadata')
    acl = ctypes.c_void_p()
    owns = False
    try:
        native = DarwinStat()
        ctypes.set_errno(0)
        require(fstatx(fd, ctypes.byref(native), security) == 0, 'native_metadata')
        require((native.st_dev, native.st_ino, native.st_mode, native.st_uid, native.st_gid, native.st_nlink, native.st_size) == (before.st_dev, before.st_ino, before.st_mode, before.st_uid, before.st_gid, before.st_nlink, before.st_size), 'native_metadata')
        present = ctypes.c_int(-1)
        ctypes.set_errno(0)
        require(lib.filesec_query_property(security, 5, ctypes.byref(present)) == 0, 'native_metadata')
        if present.value:
            ctypes.set_errno(0)
            require(lib.filesec_get_property(security, 5, ctypes.byref(acl)) == 0 and acl.value not in (None, 0, 1), 'native_metadata')
            owns = True
    finally:
        lib.filesec_free(security)
    if owns:
        try:
            ctypes.set_errno(0)
            require(lib.acl_valid(acl) == 0, 'native_metadata')
            for index in range(129):
                entry = ctypes.c_void_p()
                ctypes.set_errno(0)
                result = lib.acl_get_entry(acl, 0 if index == 0 else -1, ctypes.byref(entry))
                number = ctypes.get_errno()
                if result == -1 and number == errno.EINVAL:
                    break
                require(result == 0 and entry.value and (not private) and (index < 128), 'native_metadata')
                tag = ctypes.c_int()
                require(lib.acl_get_tag_type(entry, ctypes.byref(tag)) == 0 and tag.value == 2, 'native_metadata')
            else:
                raise AssertionError('acl_bound')
        finally:
            require(lib.acl_free(acl) == 0, 'native_metadata')
    require(fp(os.fstat(fd)) == fp(before), 'native_metadata')

def ancestor(fd, private=False):
    s = os.fstat(fd)
    require(stat.S_ISDIR(s.st_mode) and (s.st_uid == os.geteuid() or (not private and s.st_uid == 0)) and (s.st_nlink > 0) and (s.st_mode & 3602 == 0) and (not private or stat.S_IMODE(s.st_mode) == 448), 'native_metadata')
    ancestor_acl(fd, private)
    return (s.st_dev, s.st_ino, s.st_mode, s.st_uid, s.st_gid)

def proc(pid):
    lib = ctypes.CDLL('/usr/lib/libproc.dylib', use_errno=True)
    info = Bsd()
    count = lib.proc_pidinfo(pid, 3, 0, ctypes.byref(info), ctypes.sizeof(info))
    require(count == ctypes.sizeof(info) and info.pid == pid, 'process_birth_unproven')
    return (info.uid, info.ppid, info.startsec, info.startusec)

def process_path(pid):
    lib = ctypes.CDLL('/usr/lib/libproc.dylib')
    buf = ctypes.create_string_buffer(4096)
    require(lib.proc_pidpath(pid, buf, len(buf)) > 0, 'process_path_unproven')
    return os.path.realpath(buf.value.decode())
