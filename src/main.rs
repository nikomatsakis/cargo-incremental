extern crate docopt;
extern crate git2;
extern crate rustc_serialize;

use docopt::Docopt;
use git2::{Commit, Object, Repository, Status};
use git2::build::CheckoutBuilder;
use std::env;
use std::io;
use std::io::prelude::*;
use std::path::Path;

const USAGE: &'static str = "
Usage: cargo-fuzz-incr-git [options]
       cargo-fuzz-incr-git --help

Options:
    --repo REPO        path to repository [default: .]
    --cargo CARGO      path to Cargo.toml [default: Cargo.toml]
    --revisions REV    range of revisions to test [default: HEAD~5..HEAD]
    --verbose          dump information as we go
    --target-dir DIR   directory to do our checkouts in [default: fuzz]
";

#[derive(RustcDecodable)]
struct Args {
    flag_repo: String,
    flag_cargo: String,
    flag_revisions: String,
    flag_verbose: bool,
    flag_target_dir: String,
}

macro_rules! error {
    ($($args:tt)*) => {
        {
            let mut stderr = io::stderr();
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

    let ref repo = match Repository::open(&args.flag_repo) {
        Ok(repo) => repo,
        Err(e) => error!("failed to open repository `{}`: {}", args.flag_repo, e),
    };

    check_clean(repo);

    let revisions = match repo.revparse(&args.flag_revisions) {
        Ok(revspec) => revspec,
        Err(err) => error!("failed to parse revspec `{}`: {}", args.flag_revisions, err),
    };

    let from_object = match revisions.from() {
        Some(object) => object,
        None => {
            error!("revspec `{}` had no \"from\" point specified",
                   args.flag_revisions)
        }
    };

    let to_object = match revisions.to() {
        Some(object) => object,
        None => {
            error!("revspec `{}` had no \"to\" point specified; try something like `{}..HEAD`",
                   args.flag_revisions,
                   args.flag_revisions)
        }
    };

    let from_short = short_id(from_object);
    let to_short = short_id(to_object);

    if args.flag_verbose {
        println!("from SHA1: {}", from_short);
        println!("to SHA1: {}", to_short);
    }

    let from_commit = commit_or_error(from_object.clone());
    let to_commit = commit_or_error(to_object.clone());

    let commits = find_path(from_commit, to_commit);

    for commit in &commits {
        println!("checking out {:?}", short_id(commit));
        checkout(repo, commit);
    }
}

fn check_clean(repo: &Repository) {
    let statuses = match repo.statuses(None) {
        Ok(s) => s,
        Err(err) => error!("could not load git repository status: {}", err)
    };

    let mut errors = 0;
    let dirty_status = Status::all();
    for status in statuses.iter() {
        if status.status().intersects(dirty_status) {
            let mut stderr = io::stderr();
            let mut stderr = stderr.lock();
            if let Some(p) = status.path() {
                writeln!(stderr, "file `{}` is dirty", p).unwrap();
            }
            errors += 1;
        }
    }
    if errors > 0 {
        error!("cannot run with a dirty repository; clean it first");
    }
}

fn checkout(repo: &Repository, commit: &Commit) {
    let mut cb = CheckoutBuilder::new();
    match repo.checkout_tree(commit.as_object(), Some(&mut cb)) {
        Ok(()) => {}
        Err(err) => {
            error!("encountered error checking out `{}`: {}",
                   short_id(commit), err)
        }
    }

    match repo.set_head_detached(commit.id()) {
        Ok(()) => {}
        Err(err) => {
            error!("encountered error adjusting head to `{}`: {}",
                   short_id(commit), err)
        }
    }
}

trait AsObject<'repo> {
    fn as_object(&self) -> &Object<'repo>;
}

impl<'repo> AsObject<'repo> for Object<'repo> {
    fn as_object(&self) -> &Object<'repo> {
        self
    }
}

impl<'repo> AsObject<'repo> for Commit<'repo> {
    fn as_object(&self) -> &Object<'repo> {
        self.as_object()
    }
}

fn short_id<'repo, T>(obj: &T) -> String
    where T: AsObject<'repo>
{
    let obj = obj.as_object();
    match obj.short_id() {
        Ok(buf) => buf.as_str().unwrap().to_string(), // should really be utf-8
        Err(_) => obj.id().to_string(), // oh screw it use the full id
    }
}

fn commit_or_error<'obj, 'repo>(obj: Object<'repo>) -> Commit<'repo> {
    match obj.into_commit() {
        Ok(commit) => commit,
        Err(obj) => error!("object `{}` is not a commit", short_id(&obj)),
    }
}

fn find_path<'obj, 'repo>(start: Commit<'repo>, end: Commit<'repo>) -> Vec<Commit<'repo>> {
    let mut commits = vec![end];
    while commits.last().unwrap().id() != start.id() {
        match commits.last().unwrap().parent(0) {
            Ok(p) => commits.push(p),
            Err(_) => break,
        }
    }

    // if we get here, never found the parent
    if commits.last().unwrap().id() != start.id() {
        error!("could not find path from {} to {}",
               short_id(start.as_object()),
               short_id(commits.first().unwrap().as_object()));
    }

    commits.reverse();

    return commits;
}
