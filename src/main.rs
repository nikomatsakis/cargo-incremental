extern crate docopt;
extern crate git2;
extern crate rustc_serialize;

use docopt::Docopt;
use git2::Repository;
use std::env;

const USAGE: &'static str = "
Usage: cargo-fuzz-incr-git [options]
       cargo-fuzz-incr-git --help

Options:
    --repo REPO      path to repository [default: .]
    --cargo CARGO    path to Cargo.toml [default: Cargo.toml]
    --rev-parse REV  range of revisions to test [default: HEAD~5..HEAD]
    --verbose        dump information as we go
";

#[derive(RustcDecodable)]
struct Args {
    flag_repo: String,
    flag_cargo: String,
    flag_rev_parse: String,
    flag_verbose: bool,
}

macro_rules! error {
    ($($args:tt)*) => {
        {
            println!($($args)*);
            ::std::process::exit(1)
        }
    }
}

fn main() {
    let args: Args = Docopt::new(USAGE)
        .and_then(|d| d.argv(env::args().into_iter()).decode())
        .unwrap_or_else(|e| e.exit());

    let repo = match Repository::open(&args.flag_repo) {
        Ok(repo) => repo,
        Err(e) => error!("failed to open repository `{}`: {}", args.flag_repo, e),
    };

    let revisions = match repo.revparse(&args.flag_rev_parse) {
        Ok(revspec) => revspec,
        Err(err) => error!("failed to parse revspec `{}`: {}", args.flag_rev_parse, err),
    };

    let from_object = match revisions.from() {
        Some(object) => object,
        None => error!("revspec `{}` had no \"from\" point specified", args.flag_rev_parse),
    };

    let to_object = match revisions.to() {
        Some(object) => object,
        None => error!("revspec `{}` had no \"to\" point specified", args.flag_rev_parse),
    };

    if args.flag_verbose {
        println!("from SHA1: {}", from_object.id());
        println!("to SHA1: {}", to_object.id());
    }
}
