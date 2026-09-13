#define _POSIX_C_SOURCE 200809L
#include "sqlite3.h"

#include <errno.h>
#include <signal.h>
#include <spawn.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef LAB_UPSTREAM_MAIN
#error "LAB_UPSTREAM_MAIN must name the renamed upstream main"
#endif

#define LAB_MAX_CONNECTIONS 64
#define LAB_RANDOMBLOB_MAX (16 * 1024 * 1024)
#define LAB_RANDOM_SEED UINT64_C(0x6a09e667f3bcc909)
#define LAB_SNAPSHOT_WAIT_POLLS 150
#define LAB_SNAPSHOT_TERM_POLLS 25
#define LAB_SNAPSHOT_POLL_NS 10000000L

extern int LAB_UPSTREAM_MAIN(int argc, char **argv);
extern int sqlite_lab_auto_checkpoint_failed(void);
extern char **environ;

typedef enum {
  LAB_PROFILE_INVALID = 0,
  LAB_PROFILE_DELETE_EXTRA,
  LAB_PROFILE_WAL_FULL,
  LAB_PROFILE_WAL_NORMAL
} LabProfile;

typedef struct {
  sqlite3 *db;
  unsigned int id;
} LabConnection;

typedef struct {
  uint64_t state;
} LabRandom;

static LabConnection lab_connections[LAB_MAX_CONNECTIONS];
static unsigned int lab_next_connection = 1;
static int lab_failure_latched;
static int lab_sink_failed;
static LabProfile lab_profile;
static unsigned int lab_snapshot_connection;
static const char *lab_executable;
static uint64_t lab_snapshot_overhead_ns;
static unsigned int lab_snapshot_starts;
static unsigned int lab_snapshot_ends;

static const char *profile_name(LabProfile profile) {
  switch (profile) {
    case LAB_PROFILE_DELETE_EXTRA: return "delete-extra";
    case LAB_PROFILE_WAL_FULL: return "wal-full";
    case LAB_PROFILE_WAL_NORMAL: return "wal-normal";
    case LAB_PROFILE_INVALID: break;
  }
  return "invalid";
}

static LabProfile parse_profile(void) {
  const char *value = getenv("BLOCK_STORAGE_SQLITE_PROFILE");
  if (value == NULL) return LAB_PROFILE_INVALID;
  if (strcmp(value, "delete-extra") == 0) return LAB_PROFILE_DELETE_EXTRA;
  if (strcmp(value, "wal-full") == 0) return LAB_PROFILE_WAL_FULL;
  if (strcmp(value, "wal-normal") == 0) return LAB_PROFILE_WAL_NORMAL;
  return LAB_PROFILE_INVALID;
}

static int emit_record(const char *format, ...) {
  va_list arguments;
  int written;
  if (format == NULL || lab_sink_failed) return SQLITE_IOERR_WRITE;
  va_start(arguments, format);
  written = vfprintf(stderr, format, arguments);
  va_end(arguments);
  if (written < 0 || fflush(stderr) != 0) {
    lab_sink_failed = 1;
    lab_failure_latched = 1;
    return SQLITE_IOERR_WRITE;
  }
  return SQLITE_OK;
}

static void latch_api_failure(const char *api, int rc) {
  if (lab_failure_latched) return;
  lab_failure_latched = 1;
  (void)emit_record(
      "sqlite-lab {\"record\":\"api_error\",\"api\":\"%s\",\"rc\":%d}\n",
      api, rc);
}

static int exec_checked(sqlite3 *db, const char *sql) {
  char *message = NULL;
  int rc;
  if (db == NULL || sql == NULL) return SQLITE_MISUSE;
  rc = sqlite3_exec(db, sql, NULL, NULL, &message);
  if (rc != SQLITE_OK &&
      emit_record("sqlite-lab {\"record\":\"configuration_error\",\"rc\":%d}\n",
                  rc) != SQLITE_OK) {
    rc = SQLITE_IOERR_WRITE;
  }
  sqlite3_free(message);
  return rc;
}

static int query_int(sqlite3 *db, const char *sql, int *value) {
  sqlite3_stmt *statement = NULL;
  int rc;
  if (db == NULL || sql == NULL || value == NULL) return SQLITE_MISUSE;
  rc = sqlite3_prepare_v2(db, sql, -1, &statement, NULL);
  if (rc == SQLITE_OK) rc = sqlite3_step(statement);
  if (rc == SQLITE_ROW) {
    *value = sqlite3_column_int(statement, 0);
    rc = SQLITE_OK;
  }
  if (statement != NULL) {
    int finalize_rc = sqlite3_finalize(statement);
    if (rc == SQLITE_OK && finalize_rc != SQLITE_OK) rc = finalize_rc;
  }
  return rc;
}

static int query_journal(sqlite3 *db, const char **journal) {
  sqlite3_stmt *statement = NULL;
  const unsigned char *value;
  int rc;
  if (db == NULL || journal == NULL) return SQLITE_MISUSE;
  rc = sqlite3_prepare_v2(db, "PRAGMA journal_mode", -1, &statement, NULL);
  if (rc == SQLITE_OK) rc = sqlite3_step(statement);
  if (rc == SQLITE_ROW) {
    value = sqlite3_column_text(statement, 0);
    if (value != NULL && sqlite3_stricmp((const char *)value, "delete") == 0) {
      *journal = "delete";
    } else if (value != NULL && sqlite3_stricmp((const char *)value, "wal") == 0) {
      *journal = "wal";
    } else {
      *journal = "unexpected";
    }
    rc = SQLITE_OK;
  }
  if (statement != NULL) {
    int finalize_rc = sqlite3_finalize(statement);
    if (rc == SQLITE_OK && finalize_rc != SQLITE_OK) rc = finalize_rc;
  }
  return rc;
}

#ifdef LAB_KVTEST
static uint64_t random_next(LabRandom *random) {
  uint64_t value;
  random->state += UINT64_C(0x9e3779b97f4a7c15);
  value = random->state;
  value = (value ^ (value >> 30)) * UINT64_C(0xbf58476d1ce4e5b9);
  value = (value ^ (value >> 27)) * UINT64_C(0x94d049bb133111eb);
  return value ^ (value >> 31);
}

static void deterministic_randomblob(sqlite3_context *context, int argc,
                                     sqlite3_value **argv) {
  LabRandom *random = sqlite3_user_data(context);
  unsigned char *bytes;
  sqlite3_int64 requested_size;
  int size;
  int index;
  if (argc != 1 || random == NULL) {
    sqlite3_result_error(context, "randomblob adapter misuse", -1);
    return;
  }
  requested_size = sqlite3_value_int64(argv[0]);
  if (requested_size < 1) requested_size = 1;
  if (requested_size > LAB_RANDOMBLOB_MAX) {
    sqlite3_result_error_toobig(context);
    return;
  }
  size = (int)requested_size;
  bytes = sqlite3_malloc64((sqlite3_uint64)size);
  if (bytes == NULL) {
    sqlite3_result_error_nomem(context);
    return;
  }
  for (index = 0; index < size; index++) {
    bytes[index] = (unsigned char)random_next(random);
  }
  sqlite3_result_blob(context, bytes, size, sqlite3_free);
}

static int register_randomblob(sqlite3 *db, unsigned int connection_id) {
  LabRandom *random = sqlite3_malloc64(sizeof(*random));
  int rc;
  if (random == NULL) return SQLITE_NOMEM;
  random->state = LAB_RANDOM_SEED;
  rc = sqlite3_create_function_v2(db, "randomblob", 1, SQLITE_UTF8, random,
                                  deterministic_randomblob, NULL, NULL,
                                  sqlite3_free);
  if (rc != SQLITE_OK) return rc;
  return emit_record(
      "sqlite-lab {\"record\":\"generator\",\"connection\":%u,\"function\":\"randomblob\",\"identity\":\"splitmix64-byte-v1\",\"seed\":\"0x6a09e667f3bcc909\",\"max_bytes\":%d}\n",
      connection_id, LAB_RANDOMBLOB_MAX);
}
#endif

static int record_connection(sqlite3 *db, unsigned int *connection_id) {
  size_t index;
  if (db == NULL || connection_id == NULL) return SQLITE_MISUSE;
  for (index = 0; index < LAB_MAX_CONNECTIONS; index++) {
    if (lab_connections[index].db == NULL) {
      lab_connections[index].db = db;
      lab_connections[index].id = lab_next_connection++;
      *connection_id = lab_connections[index].id;
      return SQLITE_OK;
    }
  }
  return SQLITE_FULL;
}

static LabConnection *find_connection(sqlite3 *db) {
  size_t index;
  if (db == NULL) return NULL;
  for (index = 0; index < LAB_MAX_CONNECTIONS; index++) {
    if (lab_connections[index].db == db) return &lab_connections[index];
  }
  return NULL;
}

static size_t active_connection_count(void) {
  size_t active = 0;
  size_t index;
  for (index = 0; index < LAB_MAX_CONNECTIONS; index++) {
    if (lab_connections[index].db != NULL) active++;
  }
  return active;
}

static int query_validate_settings(sqlite3 *db, unsigned int connection_id,
                                   const char *phase, const char **journal_out) {
  const char *journal = NULL;
  int page_size = 0;
  int cache_size = 0;
  int mmap_size = -1;
  int synchronous = -1;
  int auto_checkpoint = -1;
  int expected_sync;
  int rc;
  if (db == NULL || connection_id == 0 || phase == NULL) return SQLITE_MISUSE;
  expected_sync = lab_profile == LAB_PROFILE_DELETE_EXTRA
                      ? 3
                      : (lab_profile == LAB_PROFILE_WAL_FULL ? 2 : 1);
  rc = query_int(db, "PRAGMA page_size", &page_size);
  if (rc == SQLITE_OK) rc = query_int(db, "PRAGMA cache_size", &cache_size);
  if (rc == SQLITE_OK) rc = query_int(db, "PRAGMA mmap_size", &mmap_size);
  if (rc == SQLITE_OK) rc = query_int(db, "PRAGMA synchronous", &synchronous);
  if (rc == SQLITE_OK) {
    rc = query_int(db, "PRAGMA wal_autocheckpoint", &auto_checkpoint);
  }
  if (rc == SQLITE_OK) rc = query_journal(db, &journal);
  if (rc == SQLITE_OK &&
      (page_size != 4096 || cache_size != 256 || mmap_size != 0 ||
       synchronous != expected_sync || auto_checkpoint != 1000 ||
       strcmp(journal, lab_profile == LAB_PROFILE_DELETE_EXTRA ? "delete" : "wal") != 0)) {
    rc = SQLITE_MISMATCH;
  }
  if (emit_record(
          "sqlite-lab {\"record\":\"settings\",\"phase\":\"%s\",\"connection\":%u,\"profile\":\"%s\",\"page_size\":%d,\"cache_pages\":%d,\"mmap_bytes\":%d,\"journal_mode\":\"%s\",\"synchronous\":%d,\"wal_autocheckpoint\":%d,\"rc\":%d}\n",
          phase, connection_id, profile_name(lab_profile), page_size, cache_size,
          mmap_size, journal == NULL ? "unavailable" : journal, synchronous,
          auto_checkpoint, rc) != SQLITE_OK) {
    rc = SQLITE_IOERR_WRITE;
  }
  if (journal_out != NULL) *journal_out = journal;
  return rc;
}

static int apply_profile(sqlite3 *db, unsigned int connection_id) {
  const char *journal_sql;
  const char *sync_sql;
  int rc;
  if (db == NULL || connection_id == 0 || lab_profile == LAB_PROFILE_INVALID) {
    return SQLITE_MISUSE;
  }
  journal_sql = lab_profile == LAB_PROFILE_DELETE_EXTRA
                    ? "PRAGMA journal_mode=DELETE"
                    : "PRAGMA journal_mode=WAL";
  sync_sql = lab_profile == LAB_PROFILE_DELETE_EXTRA
                 ? "PRAGMA synchronous=EXTRA"
                 : (lab_profile == LAB_PROFILE_WAL_FULL
                        ? "PRAGMA synchronous=FULL"
                        : "PRAGMA synchronous=NORMAL");
  rc = exec_checked(db, "PRAGMA page_size=4096");
  if (rc == SQLITE_OK) rc = exec_checked(db, "PRAGMA cache_size=256");
  if (rc == SQLITE_OK) rc = exec_checked(db, "PRAGMA mmap_size=0");
  if (rc == SQLITE_OK) rc = exec_checked(db, journal_sql);
  if (rc == SQLITE_OK) rc = exec_checked(db, sync_sql);
  if (rc == SQLITE_OK) rc = exec_checked(db, "PRAGMA wal_autocheckpoint=1000");
#ifdef LAB_KVTEST
  if (rc == SQLITE_OK) rc = register_randomblob(db, connection_id);
#endif
  if (rc == SQLITE_OK) {
    rc = query_validate_settings(db, connection_id, "open", NULL);
  }
  return rc;
}

static int configure_opened(sqlite3 *db) {
  unsigned int connection_id = 0;
  int rc = record_connection(db, &connection_id);
  if (rc == SQLITE_OK) rc = apply_profile(db, connection_id);
  if (rc != SQLITE_OK) latch_api_failure("configure_connection", rc);
  return rc;
}

int lab_open(const char *filename, sqlite3 **database) {
  int rc;
  if (filename == NULL || database == NULL) return SQLITE_MISUSE;
  rc = sqlite3_open(filename, database);
  if (rc == SQLITE_OK) {
    rc = configure_opened(*database);
  } else {
    latch_api_failure("sqlite3_open", rc);
  }
  return rc;
}

int lab_open_v2(const char *filename, sqlite3 **database, int flags,
                const char *vfs) {
  int rc;
  if (filename == NULL || database == NULL) return SQLITE_MISUSE;
  rc = sqlite3_open_v2(filename, database, flags, vfs);
  if (rc == SQLITE_OK) {
    rc = configure_opened(*database);
  } else {
    latch_api_failure("sqlite3_open_v2", rc);
  }
  return rc;
}

int lab_exec(sqlite3 *db, const char *sql, sqlite3_callback callback,
             void *argument, char **message) {
  int rc = sqlite3_exec(db, sql, callback, argument, message);
  if (rc != SQLITE_OK) latch_api_failure("sqlite3_exec", rc);
  return rc;
}

int lab_step(sqlite3_stmt *statement) {
  int rc = sqlite3_step(statement);
  if (rc != SQLITE_ROW && rc != SQLITE_DONE) latch_api_failure("sqlite3_step", rc);
  return rc;
}

static int monotonic_ns(uint64_t *value) {
  struct timespec now;
  uint64_t seconds;
  if (value == NULL || clock_gettime(CLOCK_MONOTONIC, &now) != 0 ||
      now.tv_sec < 0 || now.tv_nsec < 0 || now.tv_nsec >= 1000000000L) {
    return -1;
  }
  seconds = (uint64_t)now.tv_sec;
  if (seconds > (UINT64_MAX - (uint64_t)now.tv_nsec) / UINT64_C(1000000000)) {
    return -1;
  }
  *value = seconds * UINT64_C(1000000000) + (uint64_t)now.tv_nsec;
  return 0;
}

static int elapsed_ns(uint64_t start, uint64_t end, uint64_t *duration) {
  if (duration == NULL || end < start) return -1;
  *duration = end - start;
  return 0;
}

static int parse_snapshot_connection(const char *value, unsigned int *result) {
  unsigned int parsed = 0;
  const unsigned char *cursor = (const unsigned char *)value;
  if (value == NULL || result == NULL || *value == '\0') return -1;
  while (*cursor != '\0') {
    if (*cursor < '0' || *cursor > '9' || parsed > 6) return -1;
    parsed = parsed * 10U + (unsigned int)(*cursor - '0');
    cursor++;
  }
  if (parsed < 1 || parsed > LAB_MAX_CONNECTIONS) return -1;
  *result = parsed;
  return 0;
}

static int configure_snapshot(void) {
  const char *selection = getenv("BLOCK_STORAGE_SQLITE_SNAPSHOT_CONNECTION");
  struct stat metadata;
  unsigned int expected;
  if (selection == NULL) return 0;
  if (parse_snapshot_connection(selection, &lab_snapshot_connection) != 0) return -1;
#ifdef LAB_KVTEST
  expected = 2;
#else
  expected = 1;
#endif
  if (lab_snapshot_connection != expected) return -1;
  lab_executable = getenv("BLOCK_STORAGE_LAB_EXECUTABLE");
  if (lab_executable == NULL || lab_executable[0] != '/' ||
      lstat(lab_executable, &metadata) != 0 || !S_ISREG(metadata.st_mode) ||
      metadata.st_uid != geteuid() || access(lab_executable, X_OK) != 0) {
    return -1;
  }
  return 0;
}

static int poll_snapshot(pid_t pid, int *status, int polls) {
  struct timespec delay = {0, LAB_SNAPSHOT_POLL_NS};
  int poll;
  for (poll = 0; poll < polls; poll++) {
    pid_t result = waitpid(pid, status, WNOHANG);
    if (result == pid) return 0;
    if (result < 0 && errno != EINTR) return -1;
    if (nanosleep(&delay, NULL) != 0 && errno != EINTR) return -1;
  }
  return 1;
}

static int reap_snapshot(pid_t pid, int *status) {
  int result = poll_snapshot(pid, status, LAB_SNAPSHOT_WAIT_POLLS);
  if (result <= 0) return result;
  if (kill(pid, SIGTERM) != 0 && errno != ESRCH) return -1;
  result = poll_snapshot(pid, status, LAB_SNAPSHOT_TERM_POLLS);
  if (result <= 0) return result == 0 ? 1 : -1;
  if (kill(pid, SIGKILL) != 0 && errno != ESRCH) return -1;
  result = poll_snapshot(pid, status, LAB_SNAPSHOT_TERM_POLLS);
  /* The owning lab's process-group cleanup reaps any child still outstanding. */
  return result == 0 ? 1 : -1;
}

static int spawn_snapshot(const char *phase, unsigned int connection_id) {
  posix_spawn_file_actions_t actions;
  char connection[4];
  char *arguments[5];
  pid_t pid;
  int action_rc;
  int rc;
  int status = 0;
  if (phase == NULL || lab_executable == NULL || connection_id > 64) return -1;
  if (snprintf(connection, sizeof(connection), "%u", connection_id) < 1) return -1;
  arguments[0] = (char *)lab_executable;
  arguments[1] = (char *)"database-snapshot";
  arguments[2] = (char *)phase;
  arguments[3] = connection;
  arguments[4] = NULL;
  rc = posix_spawn_file_actions_init(&actions);
  if (rc != 0) return -1;
  rc = posix_spawn_file_actions_adddup2(&actions, STDERR_FILENO, STDOUT_FILENO);
  if (rc == 0) rc = posix_spawn(&pid, lab_executable, &actions, NULL, arguments, environ);
  action_rc = posix_spawn_file_actions_destroy(&actions);
  if (rc != 0) return -1;
  rc = reap_snapshot(pid, &status);
  if (action_rc != 0 || rc != 0 || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    return -1;
  }
  return 0;
}

static int run_snapshot(const char *phase, unsigned int connection_id) {
  uint64_t start;
  uint64_t end;
  uint64_t duration;
  int rc;
  if (connection_id != lab_snapshot_connection) return SQLITE_OK;
  if (monotonic_ns(&start) != 0) return SQLITE_ERROR;
  rc = spawn_snapshot(phase, connection_id);
  if (monotonic_ns(&end) != 0 || elapsed_ns(start, end, &duration) != 0 ||
      UINT64_MAX - lab_snapshot_overhead_ns < duration) {
    return SQLITE_ERROR;
  }
  lab_snapshot_overhead_ns += duration;
  if (strcmp(phase, "drain-start") == 0) lab_snapshot_starts++;
  if (strcmp(phase, "drain-end") == 0) lab_snapshot_ends++;
  return rc == 0 ? SQLITE_OK : SQLITE_ERROR;
}

int lab_close(sqlite3 *db) {
  const char *journal = NULL;
  LabConnection *connection;
  uint64_t start = 0;
  uint64_t end = 0;
  int checkpoint_rc = SQLITE_OK;
  int close_rc;
  int profile_rc;
  int log_frames = -1;
  int checkpointed_frames = -1;
  uint64_t duration = 0;
  uint64_t observer_before;
  uint64_t observer_overhead;
  unsigned int connection_id;
  int clock_started = 0;
  int start_snapshot_rc;
  int end_snapshot_rc;
  if (db == NULL) return SQLITE_OK;
  connection = find_connection(db);
  if (connection == NULL) {
    latch_api_failure("unregistered_sqlite3_close", SQLITE_MISUSE);
    close_rc = sqlite3_close(db);
    (void)emit_record(
        "sqlite-lab {\"record\":\"drain_close\",\"connection\":0,\"duration_ns\":null,\"checkpoint_rc\":null,\"close_rc\":%d,\"status\":\"failed\"}\n",
        close_rc);
    return close_rc == SQLITE_OK ? SQLITE_MISUSE : close_rc;
  }
  connection_id = connection->id;
  profile_rc = query_validate_settings(db, connection_id, "close", &journal);
  if (profile_rc != SQLITE_OK) latch_api_failure("effective_profile", profile_rc);
  observer_before = lab_snapshot_overhead_ns;
  start_snapshot_rc = run_snapshot("drain-start", connection_id);
  if (start_snapshot_rc != SQLITE_OK) latch_api_failure("database_snapshot_start", start_snapshot_rc);
  if (monotonic_ns(&start) == 0) {
    clock_started = 1;
  } else {
    latch_api_failure("clock_gettime", SQLITE_ERROR);
  }
  if (profile_rc == SQLITE_OK && strcmp(journal, "wal") == 0) {
    checkpoint_rc = sqlite3_wal_checkpoint_v2(
        db, NULL, SQLITE_CHECKPOINT_TRUNCATE, &log_frames, &checkpointed_frames);
    (void)emit_record(
        "sqlite-lab {\"record\":\"checkpoint\",\"connection\":%u,\"mode\":\"truncate\",\"rc\":%d,\"wal_frames\":%d,\"checkpointed_frames\":%d,\"status\":\"%s\"}\n",
        connection_id, checkpoint_rc, log_frames, checkpointed_frames,
        checkpoint_rc == SQLITE_OK ? "completed" : "failed");
  } else if (profile_rc == SQLITE_OK) {
    (void)emit_record(
        "sqlite-lab {\"record\":\"checkpoint\",\"connection\":%u,\"mode\":\"truncate\",\"rc\":null,\"wal_frames\":null,\"checkpointed_frames\":null,\"status\":\"not_applicable\"}\n",
        connection_id);
  } else {
    checkpoint_rc = profile_rc;
  }
  close_rc = sqlite3_close(db);
  if (clock_started && monotonic_ns(&end) == 0) {
    if (elapsed_ns(start, end, &duration) != 0) {
      clock_started = 0;
      latch_api_failure("monotonic_duration", SQLITE_ERROR);
    }
  } else if (clock_started) {
    clock_started = 0;
    latch_api_failure("clock_gettime", SQLITE_ERROR);
  }
  end_snapshot_rc = run_snapshot("drain-end", connection_id);
  if (end_snapshot_rc != SQLITE_OK) {
    latch_api_failure("database_snapshot_end", end_snapshot_rc);
  }
  observer_overhead = lab_snapshot_overhead_ns - observer_before;
  if (close_rc == SQLITE_OK) {
    connection->db = NULL;
    connection->id = 0;
  }
  if (emit_record(
          "sqlite-lab {\"record\":\"drain_close\",\"connection\":%u,\"duration_ns\":%llu,\"observer_overhead_ns\":%llu,\"checkpoint_rc\":%d,\"close_rc\":%d,\"status\":\"%s\"}\n",
          connection_id, (unsigned long long)duration,
          (unsigned long long)observer_overhead, checkpoint_rc, close_rc,
          profile_rc == SQLITE_OK && checkpoint_rc == SQLITE_OK &&
                  close_rc == SQLITE_OK && clock_started && start_snapshot_rc == SQLITE_OK &&
                  end_snapshot_rc == SQLITE_OK
              ? "completed"
              : "failed") != SQLITE_OK) {
    close_rc = close_rc == SQLITE_OK ? SQLITE_IOERR_WRITE : close_rc;
  }
  if (checkpoint_rc != SQLITE_OK) {
    latch_api_failure("sqlite3_wal_checkpoint_v2", checkpoint_rc);
  }
  if (close_rc != SQLITE_OK) latch_api_failure("sqlite3_close", close_rc);
  return close_rc == SQLITE_OK ? checkpoint_rc : close_rc;
}

static int option_is(const char *argument, const char *name) {
  if (argument == NULL || name == NULL) return 0;
  while (*argument == '-') argument++;
  return strcmp(argument, name) == 0;
}

static int validate_arguments(int argc, char **argv) {
  int index;
  if (argc < 1 || argv == NULL) return -1;
  for (index = 1; index < argc; index++) {
#ifdef LAB_KVTEST
    if (option_is(argv[index], "blob-api") || option_is(argv[index], "jmode") ||
        option_is(argv[index], "nosync") || option_is(argv[index], "nocheckpoint")) {
      return -1;
    }
    if (option_is(argv[index], "cache-size")) {
      if (++index >= argc || strcmp(argv[index], "256") != 0) return -1;
    } else if (option_is(argv[index], "mmap")) {
      if (++index >= argc || strcmp(argv[index], "0") != 0) return -1;
    }
#else
    if (option_is(argv[index], "journal") || option_is(argv[index], "nosync") ||
        option_is(argv[index], "pagesize") || option_is(argv[index], "cachesize") ||
        option_is(argv[index], "mmap") || option_is(argv[index], "singlethread") ||
        option_is(argv[index], "multithread") || option_is(argv[index], "nomutex")) {
      return -1;
    }
#endif
  }
#ifdef LAB_KVTEST
  if (argc >= 2 && strcmp(argv[1], "run") == 0) {
    int cache_found = 0;
    int mmap_found = 0;
    for (index = 2; index < argc; index++) {
      cache_found |= option_is(argv[index], "cache-size");
      mmap_found |= option_is(argv[index], "mmap");
    }
    if (!cache_found || !mmap_found) return -1;
  }
#endif
  return 0;
}

int main(int argc, char **argv) {
  uint64_t process_start;
  uint64_t process_end;
  uint64_t process_duration;
  int upstream_rc;
  if (argc < 1 || argv == NULL) return 64;
  if (validate_arguments(argc, argv) != 0) {
    (void)emit_record(
        "sqlite-lab {\"record\":\"argument_error\",\"field\":\"benchmark_options\",\"reason\":\"options_must_preserve_lab_profile\"}\n");
    return 64;
  }
  if (configure_snapshot() != 0) {
    (void)emit_record(
        "sqlite-lab {\"record\":\"argument_error\",\"field\":\"BLOCK_STORAGE_SQLITE_SNAPSHOT_CONNECTION\",\"reason\":\"selection_requires_known_connection_and_private_absolute_executable\"}\n");
    return 64;
  }
  lab_profile = parse_profile();
  if (lab_profile == LAB_PROFILE_INVALID) {
    (void)emit_record(
        "sqlite-lab {\"record\":\"argument_error\",\"field\":\"BLOCK_STORAGE_SQLITE_PROFILE\",\"allowed\":[\"delete-extra\",\"wal-full\",\"wal-normal\"]}\n");
    return 64;
  }
  if (monotonic_ns(&process_start) != 0) {
    latch_api_failure("clock_gettime", SQLITE_ERROR);
    return 1;
  }
  upstream_rc = LAB_UPSTREAM_MAIN(argc, argv);
  if (monotonic_ns(&process_end) != 0 ||
      elapsed_ns(process_start, process_end, &process_duration) != 0) {
    latch_api_failure("clock_gettime", SQLITE_ERROR);
    return 1;
  }
  if (active_connection_count() != 0) {
    latch_api_failure("active_connection_leak", SQLITE_BUSY);
  }
  if (sqlite_lab_auto_checkpoint_failed()) {
    latch_api_failure("automatic_passive_checkpoint", SQLITE_ERROR);
  }
  if (lab_snapshot_connection != 0 && upstream_rc == 0 &&
      (lab_snapshot_starts != 1 || lab_snapshot_ends != 1)) {
    latch_api_failure("database_snapshot_count", SQLITE_ERROR);
  }
  if (emit_record(
          "sqlite-lab {\"record\":\"process\",\"duration_ns\":%llu,\"total_snapshot_overhead_ns\":%llu,\"status\":\"%s\"}\n",
          (unsigned long long)process_duration,
          (unsigned long long)lab_snapshot_overhead_ns,
          upstream_rc == 0 && !lab_failure_latched ? "completed" : "failed") != SQLITE_OK) {
    return 1;
  }
  if (upstream_rc != 0) return upstream_rc;
  return lab_failure_latched ? 1 : 0;
}
