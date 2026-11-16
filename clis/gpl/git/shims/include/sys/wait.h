/* SPDX-License-Identifier: GPL-2.0-only */
#pragma once
#include <sys/types.h>
#define WNOHANG 1
#define WUNTRACED 2
#define WEXITSTATUS(s) (((s) & 0xff00) >> 8)
#define WTERMSIG(s) ((s) & 0x7f)
#define WIFEXITED(s) (!WTERMSIG(s))
#define WIFSIGNALED(s) (((s) & 0xffff) - 1U < 0xffu)
#define WIFSTOPPED(s) 0
pid_t waitpid(pid_t pid, int *status, int options);
pid_t wait(int *status);
