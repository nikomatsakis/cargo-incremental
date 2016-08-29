extern crate docopt;
extern crate git2;
extern crate regex;
extern crate rustc_serialize;

use docopt::Docopt;
use git2::{Commit, Error as Git2Error, ErrorCode, Object, Repository, Status, STATUS_IGNORED};
use git2::build::CheckoutBuilder;
use regex::Regex;
use std::env;
use std::fs::{self, File};
use std::io;
use std::io::prelude::*;
use std::path::Path;
use std::process::{Command, Output};
use std::str::FromStr;

const USAGE: &'static str = "
Usage: cargo-fuzz-incr-git [options]
       cargo-fuzz-incr-git --help

Options:
    --cargo CARGO      path to Cargo.toml [default: Cargo.toml]
    --revisions REV    range of revisions to test [default: HEAD~5..HEAD]
    --verbose          dump information as we go
    --work-dir DIR     directory where we can do our work [default: incr]
";

#[derive(RustcDecodable)]
struct Args {
    flag_cargo: String,
    flag_revisions: String,
    flag_verbose: bool,
    flag_work_dir: String,
}

macro_rules! error {
    ($($args:tt)*) => {
        {
            let stderr = io::stderr();
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

    let cargo_toml_path = Path::new(&args.flag_cargo);

    if !cargo_toml_path.exists() || !cargo_toml_path.is_file() {
        error!("cargo path `{}` does not lead to a `Cargo.toml` file",
               cargo_toml_path.display());
    }

    let ref repo = match open_repo(cargo_toml_path) {
        Ok(repo) => repo,
        Err(e) => {
            error!("failed to find repository containing `{}`: {}",
                   cargo_toml_path.display(),
                   e)
        }
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

    // We structure our work directory like:
    //
    // work/incr <-- compiler state
    // work/commits/1231123 <-- output from building 1231123
    let incr_dir = Path::new(&args.flag_work_dir).join("incr");
    remove_dir(&incr_dir);
    make_dir(&incr_dir);
    let incr_dir = match fs::canonicalize(&incr_dir) {
        Ok(i) => i,
        Err(err) => error!("failed to canonicalize `{}`: {}", incr_dir.display(), err),
    };
    let commits_dir = Path::new(&args.flag_work_dir).join("commits");
    make_dir(&commits_dir);

    println!("incr_dir: {}", incr_dir.display());

    let cargo_dir = match Path::new(&args.flag_cargo).parent() {
        Some(p) => p,
        None => error!("Cargo.toml path has no parent: {}", args.flag_cargo),
    };

    for (index,commit) in commits.iter().enumerate() {
        let short_id = short_id(commit);

        println!("processing {:?}", short_id);

        checkout(repo, commit);

        if index == 0 {
            let commit_dir = commits_dir.join(format!("{:05}-{}-clean", index, short_id));
            make_dir(&commit_dir);
            cargo_clean(&cargo_dir, &commit_dir);
        }

        let commit_dir = commits_dir.join(format!("{:05}-{}-build", index, short_id));
        make_dir(&commit_dir);
        cargo_build(&cargo_dir, &incr_dir, &commit_dir);
    }
}

fn remove_dir(path: &Path) {
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(err) => error!("error removing directory `{}`: {}", path.display(), err),
    }
}

fn make_dir(path: &Path) {
    match fs::create_dir_all(path) {
        Ok(()) => {}
        Err(err) => error!("cannot create work-directory `{}`: {}", path.display(), err),
    }
}

fn open_repo(cargo_path: &Path) -> Result<Repository, Git2Error> {
    let mut git_path = cargo_path;

    loop {
        if git_path.is_dir() {
            match Repository::open(git_path) {
                Ok(r) => return Ok(r),
                Err(err) => {
                    match err.code() {
                        ErrorCode::NotFound => {}
                        _ => {
                            return Err(err);
                        }
                    }
                }
            }
        }

        git_path = match git_path.parent() {
            Some(p) => p,
            None => return Repository::open(cargo_path),
        }
    }
}

fn check_clean(repo: &Repository) {
    let statuses = match repo.statuses(None) {
        Ok(s) => s,
        Err(err) => error!("could not load git repository status: {}", err),
    };

    let mut errors = 0;
    let dirty_status = Status::all() - STATUS_IGNORED;
    for status in statuses.iter() {
        if status.status().intersects(dirty_status) {
            let stderr = io::stderr();
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

fn cargo_clean(cargo_dir: &Path, commit_dir: &Path) {
    let output = Command::new("cargo")
        .arg("clean")
        .current_dir(&cargo_dir)
        .output();
    match output {
        Ok(output) => save_output(commit_dir, &output),
        Err(err) => error!("failed to execute `cargo clean`: {}", err),
    }
}

fn cargo_build(cargo_dir: &Path, incr_dir: &Path, commit_dir: &Path) {
    let rustflags = env::var("RUSTFLAGS").unwrap_or(String::new());
    let output = Command::new("cargo")
        .arg("build")
        .arg("-v")
        .current_dir(&cargo_dir)
        .env("RUSTFLAGS",
             format!("-Z incremental={} -Z incremental-info {}",
                     incr_dir.display(),
                     rustflags))
        .output();
    let output = match output {
        Ok(output) => { save_output(commit_dir, &output); output }
        Err(err) => error!("failed to execute `cargo build`: {}", err),
    };

    // compute how much re-use we are getting
    let string = match String::from_utf8(output.stdout) {
        Ok(v) => v,
        Err(_) => error!("unable to parse output as utf-8"),
    };
    let reusing_regex = Regex::new(r"incremental: re-using (\d+) out of (\d+) modules").unwrap();
    for line in string.lines() {
        if let Some(captures) = reusing_regex.captures(line) {
            let reused = u64::from_str(captures.at(1).unwrap()).unwrap();
            let total = u64::from_str(captures.at(2).unwrap()).unwrap();
            let percent = (reused as f64) / (total as f64) * 100.0;
            println!("re-use: {}/{} ({:.0}%)", reused, total, percent);
        }
    }
}

fn save_output(output_dir: &Path, output: &Output) {
    write_file(&output_dir.join("status"),
               format!("{}", output.status).as_bytes());
    write_file(&output_dir.join("stdout"), &output.stdout);
    write_file(&output_dir.join("stderr"), &output.stderr);
}

fn create_file(path: &Path) -> File {
    match File::create(path) {
        Ok(f) => f,
        Err(err) => error!("failed to create `{}`: {}", path.display(), err),
    }
}

fn write_file(path: &Path, content: &[u8]) {
    let mut file = create_file(path);
    match file.write_all(content) {
        Ok(()) => (),
        Err(err) => error!("failed to write to `{}`: {}", path.display(), err),
    }
}

fn checkout(repo: &Repository, commit: &Commit) {
    let mut cb = CheckoutBuilder::new();
    match repo.checkout_tree(commit.as_object(), Some(&mut cb)) {
        Ok(()) => {}
        Err(err) => {
            error!("encountered error checking out `{}`: {}",
                   short_id(commit),
                   err)
        }
    }

    match repo.set_head_detached(commit.id()) {
        Ok(()) => {}
        Err(err) => {
            error!("encountered error adjusting head to `{}`: {}",
                   short_id(commit),
                   err)
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
