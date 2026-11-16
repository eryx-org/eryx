/* SPDX-License-Identifier: GPL-2.0-only */
#pragma once
typedef unsigned int tcflag_t; typedef unsigned char cc_t; typedef unsigned int speed_t;
#define NCCS 32
struct termios { tcflag_t c_iflag, c_oflag, c_cflag, c_lflag; cc_t c_cc[NCCS]; };
#define ECHO 0000010
#define ICANON 0000002
#define VMIN 6
#define VTIME 5
#define TCSAFLUSH 2
#define TCSANOW 0
int tcgetattr(int fd, struct termios *t);
int tcsetattr(int fd, int act, const struct termios *t);
