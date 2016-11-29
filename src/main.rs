extern crate docopt;
extern crate git2;
extern crate regex;
extern crate rustc_serialize;
extern crate progress;
extern crate toml;

#[macro_use]
extern crate log;
extern crate env_logger;

use docopt::Docopt;
use std::env;

const USAGE: &'static str = "
Usage: cargo-incremental build [options]
       cargo-incremental replay [options] <revisions>
       cargo-incremental --help

This is a tool for testing incremental compilation. It offers two main
modes:

## Build mode

`cargo incremental build` will run an incremental build. In case of
problems, it will silently create a branch in your current git
repository called `cargo-incremental-build`. Each time that you build,
a commit is added to this branch with the current state of your
working directory. This way, if you encounter a problem, we can easily
replay the steps that led to the bug.

## Replay mode

This mode will walk back through a linearization of your git history.
At each step, it will compile both incrementally and normally and also
run tests. It checks that both versions of the compiler execute in the
same way, and reports an error if that is not the case.

This can be used to try and reproduce a failure that occurred with
`cargo incremental build`, but it can also be used just as a general
purpose tester.

To do this, a temporary `work` directory is needed (specified by
`--work-dir`).  Note that this directory is **completely deleted**
before execution begins so don't supply a directory with valuable
contents. =)

Options:
    --cargo CARGO           path to Cargo.toml [default: Cargo.toml]
    --work-dir DIR          directory where we can do our work [default: work]
    --just-current          track just the current projection incrementally, not all deps
    --cli-log               print all sub-process output instead of writing to files
    --skip-tests            do not run tests, just compare compilation artifacts
    --no-debuginfo          compile without debuginfo whe comparing artifacts
    --verbose               print more output
";

// dead code allowed for now
#[allow(dead_code)]
#[derive(RustcDecodable, Clone)]
pub struct Args {
    cmd_build: bool,
    cmd_replay: bool,
    arg_arguments: Vec<String>,
    flag_cargo: String,
    arg_revisions: String,
    flag_work_dir: String,
    flag_just_current: bool,
    flag_cli_log: bool,
    flag_skip_tests: bool,
    flag_no_debuginfo: bool,
    flag_verbose: bool,
}

impl Args {
    pub fn to_cli_command(&self) -> String {
        use std::fmt::Write;

        let mut cmd = String::from("cargo-incremental");

        if self.cmd_replay {
            cmd.push_str(" replay");

            if !self.flag_cargo.is_empty() {
                write!(cmd, " --cargo {}", self.flag_cargo).unwrap();
            }

            if !self.flag_work_dir.is_empty() {
                write!(cmd, " --work-dir {}", self.flag_work_dir).unwrap();
            }

            if self.flag_just_current {
                cmd.push_str(" --just-current");
            }

            if self.flag_cli_log {
                cmd.push_str(" --cli-log");
            }

            if self.flag_skip_tests {
                cmd.push_str(" --skip-tests");
            }

            if self.flag_no_debuginfo {
                cmd.push_str(" --no-debuginfo");
            }

            if self.flag_verbose {
                cmd.push_str(" --verbose");
            }

            write!(cmd, " {}", self.arg_revisions).unwrap();

            return cmd;
        }

        unimplemented!()
    }
}

macro_rules! error {
    ($($args:tt)*) => {
        {
            let stderr = ::std::io::stderr();
            let mut stderr = stderr.lock();
            write!(stderr, "error: ").unwrap();
            writeln!(stderr, $($args)*).unwrap();
            ::std::process::exit(1)
        }
    }
}

fn main() {
    env_logger::init().unwrap();
    debug!("env_logger initialized");

    let args: Args = Docopt::new(USAGE)
        .and_then(|d| d.argv(env::args().into_iter()).decode())
        .unwrap_or_else(|e| e.exit());

    if args.cmd_build {
        build::build(&args);
    } else if args.cmd_replay {
        replay::replay(&args);
    }
}

mod build;
mod dfs;
mod replay;
mod util;

#[test]
fn test_args_to_cli_command() {
    let args = Args {
        cmd_build: false,
        cmd_replay: true,
        arg_arguments: vec![],
        flag_cargo: "".to_string(),
        arg_revisions: "master~1..master".to_string(),
        flag_work_dir: "".to_string(),
        flag_just_current: false,
        flag_cli_log: false,
        flag_skip_tests: false,
        flag_no_debuginfo: false,
    };

    assert_eq!(args.to_cli_command(), "cargo-incremental replay master~1..master");

    let cargo = Args {
        flag_cargo: "test-cargo".to_string(),
        .. args.clone()
    };
    assert_eq!(cargo.to_cli_command(), "cargo-incremental replay --cargo test-cargo master~1..master");

    let work_dir = Args {
        flag_work_dir: "/tmp/ciw".to_string(),
        .. args.clone()
    };
    assert_eq!(work_dir.to_cli_command(), "cargo-incremental replay --work-dir /tmp/ciw master~1..master");

    let just_current = Args {
        flag_just_current: true,
        .. args.clone()
    };
    assert_eq!(just_current.to_cli_command(), "cargo-incremental replay --just-current master~1..master");

    let cli_log = Args {
        flag_cli_log: true,
        .. args.clone()
    };
    assert_eq!(cli_log.to_cli_command(), "cargo-incremental replay --cli-log master~1..master");

    let skip_tests = Args {
        flag_skip_tests: true,
        .. args.clone()
    };
    assert_eq!(skip_tests.to_cli_command(), "cargo-incremental replay --skip-tests master~1..master");

    let no_debuginfo = Args {
        flag_no_debuginfo: true,
        .. args.clone()
    };
    assert_eq!(no_debuginfo.to_cli_command(), "cargo-incremental replay --no-debuginfo master~1..master");

    let verbose = Args {
        flag_verbose: true,
        .. args.clone()
    };
    assert_eq!(verbose.to_cli_command(), "cargo-incremental replay --verbose master~1..master");
}
