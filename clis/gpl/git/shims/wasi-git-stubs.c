/* SPDX-License-Identifier: GPL-2.0-only */
/* Stubs for POSIX APIs wasi-libc lacks. See wasi-git.h. */
#include <errno.h>
#include <fcntl.h>
#include <pwd.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <termios.h>
#include <unistd.h>
#include "wasi-git.h"

#define NOSYS(ret) do { errno = ENOSYS; return ret; } while (0)

/* WASI stat reports st_uid 0; match it so the safe.directory check passes. */
uid_t getuid(void) { return 0; }
uid_t geteuid(void) { return 0; }
gid_t getgid(void) { return 1000; }
gid_t getegid(void) { return 1000; }
pid_t getppid(void) { return 1; }
pid_t setsid(void) { NOSYS(-1); }
pid_t getpgid(pid_t p) { (void)p; NOSYS(-1); }
pid_t tcgetpgrp(int fd) { (void)fd; NOSYS(-1); }
/* No process creation inside a WASI child. */
pid_t fork(void) { NOSYS(-1); }
int execv(const char *p, char *const a[]) { (void)p; (void)a; NOSYS(-1); }
int execvp(const char *p, char *const a[]) { (void)p; (void)a; NOSYS(-1); }
int execve(const char *p, char *const a[], char *const e[]) { (void)p; (void)a; (void)e; NOSYS(-1); }
int execl(const char *p, const char *a, ...) { (void)p; (void)a; NOSYS(-1); }
int execlp(const char *p, const char *a, ...) { (void)p; (void)a; NOSYS(-1); }
pid_t waitpid(pid_t p, int *s, int o) { (void)p; (void)s; (void)o; errno = ECHILD; return -1; }
pid_t wait(int *s) { (void)s; errno = ECHILD; return -1; }
int pipe(int fds[2]) { (void)fds; NOSYS(-1); }
int kill(pid_t p, int s) { (void)p; (void)s; NOSYS(-1); }
int dup(int fd) { (void)fd; NOSYS(-1); }
int dup2(int fd, int fd2) { (void)fd; (void)fd2; NOSYS(-1); }
mode_t umask(mode_t m) { (void)m; return 022; }
unsigned alarm(unsigned s) { (void)s; return 0; }
char *getpass(const char *p) { (void)p; errno = ENOTTY; return NULL; }
void flockfile(FILE *f) { (void)f; }
void funlockfile(FILE *f) { (void)f; }
int tcgetattr(int fd, struct termios *t) { (void)fd; (void)t; errno = ENOTTY; return -1; }
int tcsetattr(int fd, int a, const struct termios *t) { (void)fd; (void)a; (void)t; errno = ENOTTY; return -1; }
int sigaction(int s, const struct sigaction *a, struct sigaction *o) { (void)s; (void)a; if (o) memset(o, 0, sizeof(*o)); return 0; }
int sigprocmask(int h, const sigset_t *s, sigset_t *o) { (void)h; (void)s; if (o) *o = 0; return 0; }
int sigemptyset(sigset_t *s) { *s = 0; return 0; }
int sigfillset(sigset_t *s) { *s = ~0UL; return 0; }
int sigaddset(sigset_t *s, int sig) { *s |= 1UL << sig; return 0; }

static struct passwd pw = { "user", "x", 1000, 1000, "user", "/home/user", "/bin/sh" };
struct passwd *getpwuid(uid_t u) { (void)u; const char *h = getenv("HOME"); pw.pw_dir = (char *)(h ? h : "/"); return &pw; }
struct passwd *getpwnam(const char *n) { (void)n; return getpwuid(0); }

static int fill_x(char *t) {
	size_t n = strlen(t);
	if (n < 6 || strcmp(t + n - 6, "XXXXXX")) { errno = EINVAL; return -1; }
	static const char c[] = "abcdefghijklmnopqrstuvwxyz0123456789";
	unsigned char r[6];
	if (getentropy(r, 6)) return -1;
	for (int i = 0; i < 6; i++) t[n - 6 + i] = c[r[i] % 36];
	return 0;
}
int mkstemp(char *t) {
	for (int tries = 0; tries < 100; tries++) {
		if (fill_x(t)) return -1;
		int fd = open(t, O_RDWR | O_CREAT | O_EXCL, 0600);
		if (fd >= 0 || errno != EEXIST) return fd;
	}
	return -1;
}
char *mkdtemp(char *t) {
	for (int tries = 0; tries < 100; tries++) {
		if (fill_x(t)) return NULL;
		if (!mkdir(t, 0700)) return t;
		if (errno != EEXIST) return NULL;
	}
	return NULL;
}
/* WASI has no permission bits; git chmods lock files to match the target. */
int chmod(const char *p, mode_t m) { (void)p; (void)m; return 0; }
int fchmod(int fd, mode_t m) { (void)fd; (void)m; return 0; }
