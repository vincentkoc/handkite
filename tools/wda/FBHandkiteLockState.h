// Included only by the pinned runner's XCUIDevice+FBHelpers.m.
#include <pthread.h>
#include <stdbool.h>
#include <stdint.h>

static pthread_mutex_t fb_lockStateMutex = PTHREAD_MUTEX_INITIALIZER;
static bool fb_lockStateValid;
static bool fb_lockStateValue;

static void fb_invalidateLockState(void)
{
  pthread_mutex_lock(&fb_lockStateMutex);
  fb_lockStateValid = false;
  fb_lockStateValue = false;
  pthread_mutex_unlock(&fb_lockStateMutex);
}

static void fb_refreshLockState(int token)
{
  pthread_mutex_lock(&fb_lockStateMutex);
  uint64_t state = UINT64_MAX;
  fb_lockStateValid = notify_get_state(token, &state) == NOTIFY_STATUS_OK;
  fb_lockStateValue = fb_lockStateValid && state != 0;
  pthread_mutex_unlock(&fb_lockStateMutex);
}

static bool fb_copyLockState(bool *locked)
{
  pthread_mutex_lock(&fb_lockStateMutex);
  bool valid = fb_lockStateValid;
  *locked = fb_lockStateValue;
  pthread_mutex_unlock(&fb_lockStateMutex);
  return valid;
}
