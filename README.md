# LSSH

Simple SSH server manager for your terminal, with a btop-style live stats bar.

## Install

```bash
cargo build --release
sudo install -Dm755 target/release/lssh /usr/local/bin/lssh
```

## Usage

```bash
lssh add web root@11.12.13.14   # save a server
lssh web                        # connect (same as: lssh connect web / lssh c web)
lssh list                       # list saved servers (alias: ls)
lssh passwd web                 # change a server's saved password
lssh remove web                 # remove a server and its password (alias: rm)
lssh config                     # show the settings file
lssh help [command]             # help, overall or for one command
lssh version                    # print the version
```

Mistyped commands print what is wrong and the expected usage, e.g.:

```
$ lssh add web
error: missing <user@host>
  usage: lssh add <name> <user@host>
```

Output has no colours when piped. Usage errors exit with status 2, other
failures with 1.

## Session UI

When you connect from a real terminal, LSSH runs `ssh` inside a pseudo-terminal
and reserves a few lines at the bottom of the screen for a live status bar:

```
 laptop lecer@192.168.1.106  ·  up 2h 52m  ·  load 0.05 0.23 0.25  ·  8 cores
CPU ▍░░░░░░░░░░░░░░░░░░░░░░░░░░   1%  MEM ███▉░░░░░░░░░░░░░░░░░░░░░░░░  14%  DSK ███▍░░░░░░░░░░░░░░░░░░░░░░░░  12%  NET ↓ 2.8K/s  ↑ 15.8K/s
```

The bar shows the saved server name, CPU, memory, disk and network throughput
(refreshed on an interval), plus uptime, load average and core count. The
metres stretch to fill the terminal width, use fractional block glyphs and a
left-to-right colour gradient, like btop.

Stats are collected over the same authenticated connection using SSH
multiplexing (`ControlMaster`), so no second password is ever requested.

With `bar_height = 3` the bar is framed like btop:

```
╭─ laptop ──────────────────────────────────────────────────────────╮
CPU █░░░░░░░░░   2%  MEM █░░░░░░░░░  13%  DSK █░░░░░░░░░  12%  NET ↓ 5.6K/s  ↑ 29.9K/s
╰─ up 6m  ·  load 0.33 0.40 0.22  ·  8 cores ──────────────────────╯
```

## Configuration

The first connection creates `~/.config/lssh/config.toml`:

```toml
# How often remote stats are refreshed, in milliseconds (200-60000).
update_interval_ms = 1000

# Lines reserved at the bottom for the stats bar.
# 0 disables the bar entirely (plain ssh, no stats, no scroll region).
bar_height = 2

# Blank lines between the shell and the stats bar (0-3).
bar_spacing = 1

# Colour theme: "btop", "neon" or "mono".
theme = "btop"

# Meter style: "smooth" (partial blocks, btop-like), "block" or "line".
bar_style = "smooth"

# Which segments appear in the bar.
show_name = true
show_os = false
show_cpu = true
show_memory = true
show_disk = true
show_network = true
show_load = true
show_uptime = true
```

Unknown keys are rejected with a warning and the defaults are used, so a typo
can never stop a session from starting.

## Notes

- Passwords are asked once and stored in your OS keyring (needs `sshpass`).
  If one changes on the server, update it with `lssh passwd <name>`.
- `root@host` and `host@root` both work.
- The stats bar is Linux-oriented (`/proc`). On other remotes the fields it
  cannot read show `n/a`; the shell itself works normally.
- The bar is redrawn immediately when the remote clears the screen or switches
  to/from a full-screen app (vim, htop, ...). Set `bar_height = 0` to disable it.
- On the first connection to a host with a saved password, ssh's own
  fingerprint prompt is shown before the session starts (sshpass can't answer it).
- Piping output, running without a TTY, or a terminal shorter than 4 rows falls
  back to a plain `ssh` session.

## License

Unlicense
