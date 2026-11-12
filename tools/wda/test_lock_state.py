import hashlib
import importlib.util
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location("wda_build", Path(__file__).with_name("build.py"))
assert SPEC and SPEC.loader
build = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(build)


def fixture(work):
    sources = {
        build.LOCK_HELPERS: (
            "#include <notify.h>\nstatic bool fb_isLocked;\n"
            + build.LOCK_REGISTER + "\n" + build.LOCK_GETTER
            + "\n" + "if (fb_isLocked) {}\n" * 4
        ).encode(),
        build.LOCK_HEADER: b"- (BOOL)fb_isScreenLocked;\n",
        build.LOCK_COMMANDS: build.LOCK_ENDPOINT.encode(),
    }
    for path, data in sources.items():
        (work / path).parent.mkdir(parents=True, exist_ok=True)
        (work / path).write_bytes(data)
    return sources


class LockStateTests(unittest.TestCase):
    def test_exact_patch_seeds_after_registration_and_preserves_unknown(self):
        with tempfile.TemporaryDirectory() as directory:
            work = Path(directory)
            original = fixture(work)
            pins = {path: hashlib.sha256(data).hexdigest() for path, data in original.items()}
            with mock.patch.dict(build.LOCK_SOURCE_HASHES, pins, clear=True):
                manifest = build.patch_lock_state(work)
            helpers = (work / build.LOCK_HELPERS).read_text()
            self.assertNotIn("fb_isLocked", helpers)
            self.assertIn("if (status == NOTIFY_STATUS_OK)", helpers)
            self.assertIn("fb_refreshLockState(notify_token);", helpers)
            self.assertIn("fb_invalidateLockState();", helpers)
            self.assertIn("fb_copyLockState(&locked) ? @(locked) : nil", helpers)
            commands = (work / build.LOCK_COMMANDS).read_text()
            self.assertIn('unknownErrorWithMessage:@"lock_state_unavailable"', commands)
            self.assertIn("return FBResponseWithObject(isLocked);", commands)
            self.assertEqual(manifest["source_commit"], build.SOURCE_COMMIT)
            for item in manifest["files"]:
                self.assertEqual(hashlib.sha256((work / item["path"]).read_bytes()).hexdigest(),
                                 item["output_sha256"])
                self.assertEqual(item["source_sha256"], pins.get(Path(item["path"])))
            with mock.patch.dict(build.LOCK_SOURCE_HASHES, pins, clear=True):
                with self.assertRaisesRegex(ValueError, "lock_source_mismatch"):
                    build.patch_lock_state(work)

    def test_source_drift_refuses_before_any_file_change(self):
        with tempfile.TemporaryDirectory() as directory:
            work = Path(directory)
            original = fixture(work)
            pins = {path: hashlib.sha256(data).hexdigest() for path, data in original.items()}
            (work / build.LOCK_COMMANDS).write_bytes(b"drift")
            before = {path: (work / path).read_bytes() for path in original}
            with mock.patch.dict(build.LOCK_SOURCE_HASHES, pins, clear=True):
                with self.assertRaisesRegex(ValueError, "lock_source_mismatch"):
                    build.patch_lock_state(work)
            self.assertEqual(before, {path: (work / path).read_bytes() for path in original})

    @unittest.skipUnless(shutil.which("cc"), "native C compiler required")
    def test_native_cache_cold_states_failures_and_serialized_refresh(self):
        harness = r'''
#include <assert.h>
#include <errno.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdatomic.h>
static uint32_t fake_get_state(int token, uint64_t *state);
#define notify_get_state fake_get_state
#define NOTIFY_STATUS_OK 0
#include "FBHandkiteLockState.h"
static atomic_int current;
static pthread_mutex_t control = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t changed = PTHREAD_COND_INITIALIZER;
static bool pause_read, entered, release_read;
static uint32_t fake_get_state(int token, uint64_t *state) {
  (void)token;
  assert(pthread_mutex_trylock(&fb_lockStateMutex) == EBUSY);
  int observed = atomic_load(&current);
  pthread_mutex_lock(&control);
  if (pause_read) {
    pause_read = false;
    entered = true;
    pthread_cond_broadcast(&changed);
    while (!release_read) pthread_cond_wait(&changed, &control);
  }
  pthread_mutex_unlock(&control);
  *state = (uint64_t)observed;
  return observed < 0 ? 1 : NOTIFY_STATUS_OK;
}
static void *refresh(void *unused) {
  (void)unused;
  fb_refreshLockState(1);
  return NULL;
}
int main(void) {
  bool locked = true;
  fb_invalidateLockState();
  assert(!fb_copyLockState(&locked));
  atomic_store(&current, 1);
  fb_refreshLockState(1);
  assert(fb_copyLockState(&locked) && locked);
  atomic_store(&current, 0);
  fb_refreshLockState(1);
  assert(fb_copyLockState(&locked) && !locked);
  atomic_store(&current, -1);
  fb_refreshLockState(1);
  assert(!fb_copyLockState(&locked));
  fb_invalidateLockState();
  assert(!fb_copyLockState(&locked));
  atomic_store(&current, 1);
  pause_read = true;
  pthread_t first, second;
  assert(pthread_create(&first, NULL, refresh, NULL) == 0);
  pthread_mutex_lock(&control);
  while (!entered) pthread_cond_wait(&changed, &control);
  atomic_store(&current, 0);
  assert(pthread_create(&second, NULL, refresh, NULL) == 0);
  release_read = true;
  pthread_cond_broadcast(&changed);
  pthread_mutex_unlock(&control);
  pthread_join(first, NULL);
  pthread_join(second, NULL);
  assert(fb_copyLockState(&locked) && !locked);
  return 0;
}
'''
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "cache.c"
            binary = Path(directory) / "cache"
            source.write_text(harness)
            subprocess.run([shutil.which("cc"), "-std=c11", "-Wall", "-Wextra", "-Werror",
                            "-pthread", "-I", str(Path(__file__).parent.resolve()),
                            str(source), "-o", str(binary)], check=True, capture_output=True)
            subprocess.run([str(binary)], check=True, timeout=5, capture_output=True)


if __name__ == "__main__":
    unittest.main()
