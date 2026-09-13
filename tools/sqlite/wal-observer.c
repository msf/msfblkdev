#include <stdatomic.h>
#include <stdio.h>
#include <string.h>

static atomic_int checkpoint_failure;
static atomic_int telemetry_failure;

void sqlite_lab_auto_checkpoint(const char *database, int trigger_frames,
                                int rc, int wal_frames,
                                int checkpointed_frames) {
  const char *name = database == NULL
                         ? "unavailable"
                         : (strcmp(database, "main") == 0 ? "main" : "other");
  int written;
  if (rc != 0) atomic_store(&checkpoint_failure, 1);
  if (atomic_load(&telemetry_failure)) return;
  written = fprintf(
      stderr,
      "sqlite-lab {\"record\":\"auto_checkpoint\",\"database\":\"%s\",\"trigger_frames\":%d,\"rc\":%d,\"wal_frames\":%d,\"checkpointed_frames\":%d,\"policy\":\"passive\"}\n",
      name, trigger_frames, rc, wal_frames, checkpointed_frames);
  if (written < 0 || fflush(stderr) != 0) {
    atomic_store(&telemetry_failure, 1);
  }
}

int sqlite_lab_auto_checkpoint_failed(void) {
  return atomic_load(&checkpoint_failure) || atomic_load(&telemetry_failure);
}
