//! herdr-slack CLI and pane entry: args, `verify`, the TUI (spec §3 "How it runs", §7, §9).

// Compiled for tests only until the UI task calls it.
#[cfg(test)]
mod ago;

// Compiled for tests only until a later task wires a caller.
#[cfg(test)]
mod collect;
#[cfg(test)]
mod config;
#[cfg(test)]
mod model;
#[cfg(test)]
mod refresh;
#[cfg(test)]
mod slack;
#[cfg(test)]
mod sources;
#[cfg(test)]
mod state;
#[cfg(test)]
mod store;
#[cfg(test)]
mod text;
#[cfg(test)]
mod token;
#[cfg(test)]
mod watch;

use std::env;
use std::process::ExitCode;

const USAGE: &str = "usage: herdr-slack [verify] [--fixtures]";

#[derive(Debug, PartialEq)]
enum Cmd {
    Tui,
    Verify,
}

#[derive(Debug, PartialEq)]
struct Args {
    cmd: Cmd,
    fixtures: bool,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut cmd = None;
    let mut fixtures = false;
    for arg in args {
        match arg.as_str() {
            "--fixtures" => fixtures = true,
            "verify" => {
                if cmd.replace(Cmd::Verify).is_some() {
                    return Err("command given more than once".into());
                }
            }
            other => return Err(format!("unrecognized argument '{other}'")),
        }
    }
    let cmd = cmd.unwrap_or(Cmd::Tui);
    if cmd == Cmd::Verify && fixtures {
        return Err("verify always talks to Slack; it cannot use --fixtures".into());
    }
    Ok(Args { cmd, fixtures })
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match parse_args(&args) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("herdr-slack: {e}\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn args() {
        assert_eq!(
            parse(&[]),
            Ok(Args {
                cmd: Cmd::Tui,
                fixtures: false
            })
        );
        assert_eq!(
            parse(&["--fixtures"]),
            Ok(Args {
                cmd: Cmd::Tui,
                fixtures: true
            })
        );
        assert_eq!(parse(&["verify"]).unwrap().cmd, Cmd::Verify);
        assert!(parse(&["bogus"]).is_err());
        assert!(parse(&["verify", "verify"]).is_err());
        assert!(parse(&["verify", "--fixtures"]).is_err()); // verify always hits Slack
    }
}
