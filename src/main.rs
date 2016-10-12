extern crate docopt;
extern crate git2;
extern crate regex;
extern crate rustc_serialize;
extern crate progress;

use docopt::Docopt;
use std::env;

const USAGE: &'static str = "
Usage: cargo-incremental build [options]
       cargo-incremental replay [options] <branch-name>
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
    --cargo CARGO      path to Cargo.toml [default: Cargo.toml]
    --revisions REV    range of revisions to test [default: HEAD~5..HEAD]
    --work-dir DIR     directory where we can do our work [default: work]
    --just-current     track just the current projection incrementally, not all deps
    --cli-log          print all sub-process output instead of writing to files
";

// dead code allowed for now
#[allow(dead_code)]
#[derive(RustcDecodable)]
pub struct Args {
    cmd_build: bool,
    cmd_replay: bool,
    arg_arguments: Vec<String>,
    flag_cargo: String,
    arg_branch_name: String,
    flag_work_dir: String,
    flag_just_current: bool,
    flag_cli_log: bool,
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
