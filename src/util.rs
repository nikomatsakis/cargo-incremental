use git2::{Commit, Error as Git2Error, ErrorCode, Object, Repository, Status, STATUS_IGNORED};
use std::fs;
use std::io;
use std::io::prelude::*;
use std::path::{Path, PathBuf};

pub fn absolute_dir_path(path: &Path) -> PathBuf {
    assert!(!path.exists(),
            "absolute_dir_path: path {} already exists",
            path.display());
    make_dir(&path);
    match fs::canonicalize(&path) {
        Ok(i) => i,
        Err(err) => error!("failed to canonicalize `{}`: {}", path.display(), err),
    }
}

pub fn remove_dir(path: &Path) {
    if path.exists() {
        if !path.is_dir() {
            error!("`{}` is not a directory", path.display());
        }

        match fs::remove_dir_all(path) {
            Ok(()) => {}
            Err(err) => error!("error removing directory `{}`: {}", path.display(), err),
        }
    }
}

pub fn make_dir(path: &Path) {
    match fs::create_dir_all(path) {
        Ok(()) => {}
        Err(err) => error!("cannot create work-directory `{}`: {}", path.display(), err),
    }
}

pub fn open_repo(cargo_path: &Path) -> Result<Repository, Git2Error> {
    let mut git_path = cargo_path;

    loop {
        if git_path.is_dir() {
            match Repository::open(git_path) {
                Ok(r) => {println!("repo at {}", git_path.display()); return Ok(r) }
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

pub fn check_clean(repo: &Repository) {
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

pub trait AsObject<'repo> {
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

pub fn short_id<'repo, T>(obj: &T) -> String
    where T: AsObject<'repo>
{
    let obj = obj.as_object();
    match obj.short_id() {
        Ok(buf) => buf.as_str().unwrap().to_string(), // should really be utf-8
        Err(_) => obj.id().to_string(), // oh screw it use the full id
    }
}

pub fn commit_or_error<'obj, 'repo>(obj: Object<'repo>) -> Commit<'repo> {
    match obj.into_commit() {
        Ok(commit) => commit,
        Err(obj) => error!("object `{}` is not a commit", short_id(&obj)),
    }
}
