#define _POSIX_C_SOURCE 200809L
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int kill_and_reap(pid_t child, int *status) {
    if (kill(child, SIGKILL) < 0 && errno != ESRCH) {
        perror("native test kill");
        return 2;
    }
    pid_t waited;
    do { waited = waitpid(child, status, 0); } while (waited < 0 && errno == EINTR);
    if (waited < 0) { perror("native test reap"); return 2; }
    return 0;
}

static int wait_for_test(pid_t child, int *status) {
    struct timespec deadline;
    if (clock_gettime(CLOCK_MONOTONIC, &deadline) < 0) {
        perror("native test clock");
        kill_and_reap(child, status);
        return 2;
    }
    deadline.tv_sec += ROCK_TEST_TIMEOUT_SECONDS;
    for (;;) {
        pid_t waited = waitpid(child, status, WNOHANG);
        if (waited == child) return 0;
        if (waited < 0 && errno != EINTR) {
            perror("native test wait");
            kill_and_reap(child, status);
            return 2;
        }
        struct timespec now;
        if (clock_gettime(CLOCK_MONOTONIC, &now) < 0) {
            perror("native test clock");
            kill_and_reap(child, status);
            return 2;
        }
        if (now.tv_sec > deadline.tv_sec ||
            (now.tv_sec == deadline.tv_sec && now.tv_nsec >= deadline.tv_nsec)) {
            if (kill_and_reap(child, status)) return 2;
            *status = -1;
            return 0;
        }
        const struct timespec pause = {0, 1000000};
        if (nanosleep(&pause, NULL) < 0 && errno != EINTR) {
            perror("native test sleep");
            kill_and_reap(child, status);
            return 2;
        }
    }
}

static FILE *result_file(const char *dir, int index, const char *suffix) {
    char path[4096];
    int length = snprintf(path, sizeof(path), "%s/%d.%s", dir, index, suffix);
    if (length < 0 || (size_t)length >= sizeof(path)) {
        fprintf(stderr, "native test result path is too long\n");
        return NULL;
    }
    FILE *file = fopen(path, "w");
    if (!file) perror("native test result file");
    return file;
}

static int run_one(const char *dir, int index) {
    FILE *out = result_file(dir, index, "out");
    if (!out) return 2;
    FILE *err = result_file(dir, index, "err");
    if (!err) { fclose(out); return 2; }
    pid_t child = fork();
    if (child < 0) {
        perror("native test fork");
        fclose(out);
        fclose(err);
        return 2;
    }
    if (child == 0) {
        if (dup2(fileno(out), STDOUT_FILENO) < 0 || dup2(fileno(err), STDERR_FILENO) < 0) {
            perror("native test output redirect");
            _exit(125);
        }
        fclose(out);
        fclose(err);
        exit(rock_tests[index]());
    }
    fclose(out);
    fclose(err);
    int status;
    if (wait_for_test(child, &status)) return 2;
    FILE *result = result_file(dir, index, "status");
    if (!result) return 2;
    if (fprintf(result, "%d\n", status) < 0) {
        perror("native test status write");
        fclose(result);
        return 2;
    }
    if (fclose(result) != 0) { perror("native test status close"); return 2; }
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 2 && argc != 3) return 2;
    if (argc == 3) {
        char *end;
        long index = strtol(argv[2], &end, 10);
        if (*end || index < 0 || index >= ROCK_TEST_COUNT) return 2;
        return run_one(argv[1], (int)index);
    }
    for (int index = 0; index < ROCK_TEST_COUNT; index++) {
        int result = run_one(argv[1], index);
        if (result) return result;
    }
    return 0;
}
