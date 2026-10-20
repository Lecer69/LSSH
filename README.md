# LSSH

Simple SSH server manager for your terminal, with a btop-style live stats bar.

## Install

From the AUR:

```bash
yay -S lssh
```

From source (needs `openssh`, and `libdbus` for the OS keyring):

```bash
cargo build --release
sudo install -Dm755 target/release/lssh /usr/local/bin/lssh
```

Prebuilt Linux binaries are attached to every
[GitHub release](https://github.com/Lecer69/LSSH/releases).

## Usage

```bash
lssh add web root@11.12.13.14        # save a server (user@host[:port])
lssh import                          # or import hosts from ~/.ssh/config
lssh                                 # pick a server from a searchable list
lssh web                             # connect (same as: lssh connect web)
lssh -                               # reconnect to the last server
lssh list                            # saved servers, most recent first (ls)
```

### Commands

| Servers | |
|---|---|
| `add <name> <user@host[:port]> [--key <file>]` | save a new server |
| `import [file]` | import hosts from `~/.ssh/config` |
| `list`, `ls` `[--names]` | list saved servers, most recent first |
| `rename`, `mv <old> <new>` | rename a server (its password moves along) |
| `remove`, `rm <name>` | remove a server and its saved password |
| `passwd <name>` | change a server's saved password |
| `copy-id <name> [--key <file>] [--keep-password]` | switch a server from password to key login |

| Remote | |
|---|---|
| `connect`, `c <name> [--record]` | connect; `<name>` can be `-` for the last server |
| `run <servers> <command>...` | run a command on one or more servers |
| `cp [-r] <from> <to>` | copy files; one side is `<server>:<path>` |
| `status [servers]` | check which servers are reachable |
| `top [servers]` | live stats of all servers on one screen |

| Setup | |
|---|---|
| `config [edit\|path]` | show, edit or locate the settings file |
| `completions <shell>` | print bash, zsh or fish completions |
| `help [command]` | help, overall or for one command |
| `version` | print the version |

`<servers>` is a name, a comma-separated list (`web,db`) or `all`.
`lssh help <command>` shows the details and examples of each command.

```bash
lssh run all uptime                  # every output line is prefixed with the server
lssh run web -- df -h /              # one server: its exit status becomes lssh's
lssh cp web:/var/log/syslog .        # download
lssh cp -r ./site web:/srv/www       # upload a directory
lssh status                          # ● up 43 ms  OpenSSH_9.6 ...  (exit 1 if any is down)
```

Mistyped commands say what is wrong and show the expected usage:

```
$ lssh add web
error: missing <user@host[:port]>
  usage: lssh add [options] <name> <user@host[:port]>
```

Output has no colours when piped. Usage errors exit with status 2, other
failures with 1.

## Passwords and keys

LSSH asks for a server's password the first time ssh needs it and keeps it in
your OS keyring (never on disk). If the server later rejects it, LSSH asks
again and remembers the new one; `lssh passwd <name>` changes it by hand.

To stop using passwords for a server, run `lssh copy-id <name>`: it installs
your public key with `ssh-copy-id`, checks that the key login works, and only
then deletes the saved password. Servers can also use a specific key from the
start with `lssh add <name> <target> --key ~/.ssh/id_web`.

Under the hood LSSH is ssh's own `SSH_ASKPASS` helper (OpenSSH 8.4+): ssh asks
LSSH whenever it needs input. Passwords are answered from the keyring, and
everything else (new host keys, key passphrases, one-time codes) is shown on
the terminal just like plain ssh would. No `sshpass` is needed.

## Session UI

When you connect from a real terminal, LSSH runs `ssh` inside a pseudo-terminal
and reserves a few lines at the bottom of the screen for a live status bar:

```
 1GBServer root@87.107.83.119  ·  up 3d 16h  ·  load 0.61 0.61 0.43  ·  1 cores
CPU   ▂▂▂▁▂▁ ▉░░░░░░░░░░░   7%  MEM ███▉░░░░░░░░  33%  DSK █▌░░░░░░░░░░  13%  NET   ▇█▇▇█▅ ↓ 4.9K/s  ↑ 6.1K/s
```

The bar shows the saved server name, CPU, memory, disk and network throughput
(refreshed on an interval), plus uptime, load average and core count. The
metres stretch to fill the terminal width, use fractional block glyphs and a
left-to-right colour gradient, like btop. Sparklines next to CPU and NET show
the recent history, and a metre turns red once it passes its alert threshold.

Stats are collected over the same authenticated connection using SSH
multiplexing (`ControlMaster`), so no second password is ever requested.

With `bar_height = 3` the bar is framed like btop:

```
╭─ laptop ──────────────────────────────────────────────────────────╮
CPU █░░░░░░░░░   2%  MEM █░░░░░░░░░  13%  DSK █░░░░░░░░░  12%  NET ↓ 5.6K/s  ↑ 29.9K/s
╰─ up 6m  ·  load 0.33 0.40 0.22  ·  8 cores ──────────────────────╯
```

`lssh top` puts the bars of all servers on one screen. It only uses saved
passwords and keys, so connect to a server once before watching it there.

### Recording

`lssh connect <name> --record` (or `record = true` in the config) saves the
session to `~/.local/share/lssh/recordings/<name>-<date>-<time>.cast`, in the
asciicast format. Play it back with `asciinema play <file>`. Recordings contain
everything the remote printed, so they are only readable by you.

## Configuration

The first connection creates `~/.config/lssh/config.toml`; `lssh config edit`
opens it in `$EDITOR` and checks it afterwards.

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

# Small history graphs next to the CPU and network meters.
show_sparklines = true

# A meter turns red once it reaches this percentage (0 = never).
alert_cpu = 85
alert_memory = 85
alert_disk = 90

# Record every session (see Recording above).
record = false

# Override theme colours with "#rrggbb" values. Keys: label, name, cpu,
# mem, disk, net_down, net_up, warn, crit, frame.
[colors]
# cpu = "#5eebc4"
# crit = "#ff5555"
```

Unknown keys and invalid colours are rejected with a warning and the defaults
are used, so a typo can never stop a session from starting.

## Shell completions

Completions cover commands, options and your saved server names. The AUR
package installs them; otherwise:

```bash
lssh completions bash > ~/.local/share/bash-completion/completions/lssh
lssh completions zsh > ~/.zfunc/_lssh        # with ~/.zfunc in $fpath
lssh completions fish > ~/.config/fish/completions/lssh.fish
```

## Notes

- Targets can be `user@host`, `user@host:2222`, `user@[2001:db8::1]:2222`,
  or `host@user` when the host is an IP address.
- `lssh import` reads `HostName`, `User`, `Port` and `IdentityFile`; other
  options such as `ProxyJump` are not imported.
- The stats bar is Linux-oriented (`/proc`). On other remotes the fields it
  cannot read show `n/a`; the shell itself works normally.
- The bar is redrawn immediately when the remote clears the screen or switches
  to/from a full-screen app (vim, htop, ...). Set `bar_height = 0` to disable it.
- Piping output, running without a TTY, or a terminal shorter than 4 rows falls
  back to a plain `ssh` session.

## License

Unlicense
