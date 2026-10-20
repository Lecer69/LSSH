//! Reading hosts out of an OpenSSH config file (`~/.ssh/config`).

/// One concrete `Host` alias and the settings lssh cares about.
#[derive(Debug, Default, PartialEq)]
pub struct HostEntry {
    pub alias: String,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<String>,
    pub identity: Option<String>,
}

/// Collect the concrete hosts of an ssh config, in file order.
///
/// Like ssh, the first value seen for a setting wins. Wildcard patterns,
/// negations and `Match` blocks are skipped, as is anything before the
/// first `Host` line.
pub fn parse_ssh_config(text: &str) -> Vec<HostEntry> {
    let mut entries: Vec<HostEntry> = Vec::new();
    // Indices of the entries the current block applies to.
    let mut current: Vec<usize> = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (keyword, value) = split_keyword(line);
        match keyword.to_ascii_lowercase().as_str() {
            "host" => {
                current.clear();
                for alias in value.split_whitespace().map(unquote) {
                    if alias.contains(['*', '?', '!']) {
                        continue;
                    }
                    let index = match entries.iter().position(|e| e.alias == alias) {
                        Some(index) => index,
                        None => {
                            entries.push(HostEntry {
                                alias: alias.to_string(),
                                ..HostEntry::default()
                            });
                            entries.len() - 1
                        }
                    };
                    current.push(index);
                }
            }
            "match" => current.clear(),
            key @ ("hostname" | "user" | "port" | "identityfile") => {
                for &index in &current {
                    let entry = &mut entries[index];
                    let slot = match key {
                        "hostname" => &mut entry.hostname,
                        "user" => &mut entry.user,
                        "port" => &mut entry.port,
                        _ => &mut entry.identity,
                    };
                    slot.get_or_insert_with(|| unquote(value).to_string());
                }
            }
            _ => {}
        }
    }
    entries
}

/// Split `Keyword value`, `Keyword=value` or `Keyword = value`.
fn split_keyword(line: &str) -> (&str, &str) {
    let end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let (keyword, rest) = line.split_at(end);
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest).trim();
    (keyword, rest)
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
# global settings are ignored
ServerAliveInterval 30

Host web www
    HostName 11.12.13.14
    User root
    Port 2222

Host db
    hostname=db.example.com
    User = admin
    IdentityFile "~/.ssh/id db"
    User ignored-because-first-wins

Host *.internal !bastion
    User nobody

Match host foo
    User matched

Host web
    User second-block-does-not-override
    IdentityFile ~/.ssh/id_web
"#;

    #[test]
    fn parses_hosts() {
        let hosts = parse_ssh_config(CONFIG);
        assert_eq!(
            hosts,
            vec![
                HostEntry {
                    alias: "web".into(),
                    hostname: Some("11.12.13.14".into()),
                    user: Some("root".into()),
                    port: Some("2222".into()),
                    identity: Some("~/.ssh/id_web".into()),
                },
                HostEntry {
                    alias: "www".into(),
                    hostname: Some("11.12.13.14".into()),
                    user: Some("root".into()),
                    port: Some("2222".into()),
                    identity: None,
                },
                HostEntry {
                    alias: "db".into(),
                    hostname: Some("db.example.com".into()),
                    user: Some("admin".into()),
                    port: None,
                    identity: Some("~/.ssh/id db".into()),
                },
            ]
        );
    }

    #[test]
    fn empty_and_pattern_only_configs() {
        assert!(parse_ssh_config("").is_empty());
        assert!(parse_ssh_config("Host *\n  User root\n").is_empty());
    }
}
