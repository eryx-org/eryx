/* SPDX-License-Identifier: GPL-2.0-only */
/* Force-included into every git TU for the wasm32-wasip2 build.
 * Declares the POSIX process/signal/uid APIs wasi-libc lacks; the
 * implementations in wasi-git-stubs.c either fail with ENOSYS (process
 * creation) or return a fixed single-user identity. */
#ifndef WASI_GIT_SHIM_H
#define WASI_GIT_SHIM_H
#include <sys/types.h>
#include <stdio.h>
#include <signal.h>
uid_t getuid(void); uid_t geteuid(void); gid_t getgid(void); gid_t getegid(void);
pid_t getppid(void); pid_t fork(void); pid_t setsid(void); pid_t getpgid(pid_t);
pid_t tcgetpgrp(int fd);
int execv(const char *path, char *const argv[]);
int execvp(const char *file, char *const argv[]);
int execve(const char *path, char *const argv[], char *const envp[]);
int execl(const char *path, const char *arg, ...);
int execlp(const char *file, const char *arg, ...);
int pipe(int fds[2]);
int kill(pid_t pid, int sig);
int dup(int fd); int dup2(int fd, int fd2);
mode_t umask(mode_t m);
unsigned alarm(unsigned s);
char *getpass(const char *prompt);
int mkstemp(char *tmpl);
char *mkdtemp(char *tmpl);
void flockfile(FILE *f); void funlockfile(FILE *f);

#ifndef SA_RESTART
typedef unsigned long wasi_sigset_t;
#define sigset_t wasi_sigset_t
struct sigaction { void (*sa_handler)(int); sigset_t sa_mask; int sa_flags; };
#define SA_RESTART 0x10000000
#define SIG_BLOCK 0
#define SIG_UNBLOCK 1
#define SIG_SETMASK 2
int sigaction(int sig, const struct sigaction *act, struct sigaction *old);
int sigprocmask(int how, const sigset_t *set, sigset_t *old);
int sigemptyset(sigset_t *s); int sigfillset(sigset_t *s); int sigaddset(sigset_t *s, int sig);
#endif
/* WASI has no permission bits and wasi-libc's chmod fails with ENOSYS;
 * git chmods lock files to match the file they replace. Make it a no-op. */
#define chmod wasi_git_chmod
#define fchmod wasi_git_fchmod
int chmod(const char *p, mode_t m); int fchmod(int fd, mode_t m);
#ifndef ITIMER_REAL
#define ITIMER_REAL 0
#endif
#endif
