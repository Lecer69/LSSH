# LSSH

Simple SSH server manager for your terminal.

## Install

```bash
cargo build --release
sudo install -Dm755 target/release/lssh /usr/local/bin/lssh
```

## Usage

```bash
lssh add web root@11.12.13.14   # Add a server
lssh ls                         # List servers
lssh web                        # Connect
lssh rm web                     # Remove
```

## Notes

- Passwords are asked once and stored in your OS keyring (needs `sshpass`).
- `root@host` and `host@root` both work.

## License

Unlicense
