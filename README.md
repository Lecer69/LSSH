# LSSH

Simple SSH server manager for your terminal, with a live stats bar
and a built-in SFTP file panel.

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
| `proxy [set <address>\|unset]` | show, save or remove the proxy used with `-x` |
| `completions <shell>` | print bash, zsh or fish completions |
| `help [command]` | help, overall or for one command |
| `version` | print the version |

`<servers>` is a name, a comma-separated list (`web,db`) or `all`.
The Remote commands and `copy-id` take `-x` (`--proxy`) to go through the
saved proxy (see [Proxy](#proxy)).
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

## Proxy

LSSH can reach servers through a SOCKS5 or HTTP proxy, such as a local xray
or v2ray client. Save it once, then add `-x` (`--proxy`) whenever a command
should go through it:

```bash
lssh proxy set 127.0.0.1:10808       # socks5:// is assumed; http://host:port works too
lssh web -x                          # connect through the proxy
lssh -x                              # pick a server, then connect through the proxy
lssh run -x all uptime               # run, cp, copy-id, status and top take -x too
lssh status -x                       # which servers answer through the proxy
lssh proxy                           # show the saved proxy
lssh proxy unset                     # forget it
```

Without `-x` nothing changes, so servers on your LAN stay direct. The proxy is
saved as `proxy` in `config.toml`.

LSSH is its own `ProxyCommand`, the same way it is its own askpass helper: it
does what `ssh -o ProxyCommand="nc -X 5 -x 127.0.0.1:10808 %h %p"` does,
without needing netcat. Host names are resolved by the proxy, and the stats bar
shares the proxied connection. Proxies that need a username and password are
not supported.

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

### File panel (SFTP)

Press **Ctrl+N** in a session to open a file browser for the server on the
left side of the terminal; the shell keeps running next to it. It talks SFTP
over the session's own connection, so there is no second login, and it goes
through the same proxy.

```
 SFTP  laptop                  ?  ✕ │[lecer@laptop lssh-test]$ ls
 /tmp/lssh-test                     │a.txt  folder  sub  with space.bin
 ..                                 │[lecer@laptop lssh-test]$
 folder/                          — │
 sub/                          293K │
 a.txt                           6B │
 with space.bin                1.4M │
                                    │
 ↑ Uploading big.iso                │
 █████████▍░░░░░░░░░░░  45% 11M/s  ✕│
 -rw-r--r--  1.4M  2026-10-31 15:23 │
 +File   +Dir   ↑Up   ↓Get   ✕Del   │
```

Everything works with the keyboard and the mouse:

| Key | Mouse | |
| --- | --- | --- |
| `↑` `↓` / `j` `k`, `PgUp` `PgDn`, `g` `G` | click, wheel | move |
| `Enter` / `→` | double-click | open a folder (on a file: download it) |
| `←` / `Backspace` | click a folder in the path | go up |
| `Space`, `a` | right-click (or Ctrl+click) | mark entries (all of them) |
| `n` / `m` | `+File` / `+Dir` | new file / new folder |
| `u` | `↑Up`, or drop files on the terminal | upload files or folders |
| `d` | `↓Get` | download (to `~/Downloads` by default) |
| `Delete` / `x` | `✕Del` | delete, after two confirmations |
| `R` / `F2` | | rename |
| `s` | | measure a folder's size |
| `.` | | show hidden files |
| `r` / `F5`, `/`, `~` | | refresh, go to a path, go home |
| `c` | `✕` on the progress bar | cancel the running transfer |
| `<` `>` | drag the border | narrower / wider |
| `Tab` | click the shell | type in the shell (Ctrl+N comes back) |
| `Esc`, `q`, `Ctrl+N` | `✕` in the corner | close the panel |
| `?` | `?` in the corner | list the keys |

- Sizes are shown for every file; `s` measures a folder (all its contents).
- Deleting always asks twice: first what would be removed (for folders, how
  many files and how much data, counted on the server), then you type the
  name (or `delete` for several items). `Cancel` is the default, and nothing
  is removed until both are confirmed. Links are removed, never followed.
- Dropping files from a file manager onto the terminal pastes their paths;
  while the panel has focus that uploads them to the folder it shows.
  Existing files are only replaced after you confirm.
- Uploads and downloads run in the background, one after another, so you
  can keep browsing (or close the panel) meanwhile. Folders are copied with
  everything in them; a cancelled transfer leaves no half-written file.
- If the remote shell reports its directory (OSC 7, e.g. fish or bash with
  `vte.sh`), the panel opens there; otherwise in your home folder.
- While the panel is open, the mouse wheel scrolls the shell's history, and
  full-screen apps get mouse events and the arrow keys as usual.

Ctrl+N is taken from the remote while the panel is enabled; set
`sftp = false` in the config to give it back (to nano or vim, for example).

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

# Lines reserved at the bottom for the stats bar (0 disables it).
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

# Ctrl+N opens an SFTP file panel on the left of the session (see
# `lssh help connect`). Set sftp = false to pass Ctrl+N to the remote
# instead. With bar_height = 0 and sftp = false, sessions are plain ssh.
sftp = true

# Width of the file panel in columns (20-200; it always leaves room for
# the shell). Drag its border or press < and > in it to change it.
sftp_width = 36

# Show hidden (dot) files in the panel from the start (toggle with .).
sftp_hidden = false

# Where the panel downloads to. Empty means ~/Downloads, or the folder
# lssh was started in when there is none.
download_dir = ""

# Proxy for commands run with -x (lssh <name> -x), e.g. a local xray or
# v2ray client: "socks5://host:port" (or just "host:port") or
# "http://host:port". Set it with `lssh proxy set <address>`.
# proxy = "socks5://127.0.0.1:10808"

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
- The file panel needs the server's SFTP subsystem (OpenSSH has it on by
  default). Ending the shell while a transfer runs lets the transfer finish
  first; Ctrl+C stops it.
- Piping output, running without a TTY, or a terminal shorter than 4 rows falls
  back to a plain `ssh` session.

## License

Unlicense
