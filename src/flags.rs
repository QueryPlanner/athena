//! `--name value` and `--switch` arguments for the subcommands that take
//! several of them (`eval`, `bench`).

use anyhow::{Result, bail};
use std::collections::HashMap;
use std::str::FromStr;

/// Parsed flags. Each may be given once.
#[derive(Debug, Default)]
pub struct Flags {
    values: HashMap<String, String>,
    switches: Vec<String>,
    /// Arguments that are not flags, in order.
    pub positional: Vec<String>,
}

impl Flags {
    /// Parse `args`. `valued` flags take the next argument; `switches` take
    /// none. Anything else starting with `--` is an error.
    pub fn parse(args: &[String], valued: &[&str], switches: &[&str]) -> Result<Self> {
        let mut flags = Self::default();
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            let Some(name) = arg.strip_prefix("--") else {
                flags.positional.push(arg.clone());
                continue;
            };
            let seen = flags.values.contains_key(name) || flags.switches.iter().any(|s| s == name);
            if seen {
                bail!("--{name} given twice");
            }
            if valued.contains(&name) {
                let Some(value) = rest.next() else {
                    bail!("--{name} needs a value");
                };
                flags.values.insert(name.to_string(), value.clone());
            } else if switches.contains(&name) {
                flags.switches.push(name.to_string());
            } else {
                bail!("unknown flag --{name}");
            }
        }
        Ok(flags)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    pub fn switch(&self, name: &str) -> bool {
        self.switches.iter().any(|s| s == name)
    }

    /// `--name` parsed as `T`, or `default` when absent.
    pub fn parsed<T: FromStr>(&self, name: &str, default: T) -> Result<T> {
        match self.get(name) {
            None => Ok(default),
            Some(v) => match v.parse() {
                Ok(parsed) => Ok(parsed),
                Err(_) => bail!("--{name} got `{v}`, which is not a valid value"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn values_switches_and_positionals_parse() {
        let f = Flags::parse(
            &args(&["a", "--k", "3", "--judge", "b"]),
            &["k", "out"],
            &["judge"],
        )
        .unwrap();
        assert_eq!(f.positional, ["a", "b"]);
        assert_eq!(f.get("k"), Some("3"));
        assert_eq!(f.get("out"), None);
        assert!(f.switch("judge") && !f.switch("k"));
        assert_eq!(f.parsed("k", 1usize).unwrap(), 3);
        assert_eq!(f.parsed("out", 7usize).unwrap(), 7);
    }

    #[test]
    fn bad_flags_are_refused() {
        for (list, why) in [
            (&["--nope"][..], "unknown flag --nope"),
            (&["--k"][..], "--k needs a value"),
            (&["--k", "1", "--k", "2"][..], "--k given twice"),
            (&["--judge", "--judge"][..], "--judge given twice"),
        ] {
            let err = Flags::parse(&args(list), &["k"], &["judge"])
                .unwrap_err()
                .to_string();
            assert_eq!(err, why);
        }
        let f = Flags::parse(&args(&["--k", "x"]), &["k"], &[]).unwrap();
        let err = f.parsed("k", 1usize).unwrap_err().to_string();
        assert_eq!(err, "--k got `x`, which is not a valid value");
    }
}
