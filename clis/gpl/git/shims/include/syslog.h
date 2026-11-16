/* SPDX-License-Identifier: GPL-2.0-only */
#pragma once
#define LOG_ERR 3
#define LOG_INFO 6
#define LOG_PID 1
#define LOG_DAEMON (3<<3)
static inline void openlog(const char *i, int o, int f) { (void)i; (void)o; (void)f; }
static inline void syslog(int p, const char *fmt, ...) { (void)p; (void)fmt; }
