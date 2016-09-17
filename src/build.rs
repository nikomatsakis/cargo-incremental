use std::io::prelude::*;
use std::path::Path;
use std::io;

use git2::{Commit, Repository, Signature, STATUS_WT_NEW};

use super::Args;
use super::util;
use super::util::{cargo_build, CompilationStats, IncrementalOptions};

fn reset_branch(repo: &Repository, branch: &str) {
    match repo.set_head(branch) {
        Ok(()) => {}
        Err(err) => error!("encountered error adjusting head: {}", err),
    }
}

fn check_untracked_rs_files(repo: &Repository) {
    let statuses = match repo.statuses(None) {
        Ok(s) => s,
        Err(err) => error!("could not load git repository status: {}", err),
    };

    let mut errors = 0;
    for status in statuses.iter() {
        if status.status().intersects(STATUS_WT_NEW) {
            if let Some(p) = status.path() {
                if p.ends_with("rs") {
                    let stderr = io::stderr();
                    let mut stderr = stderr.lock();
                    writeln!(stderr, "file `{}` is untracked", p).unwrap();
                    errors += 1;
                }
            }
        }
    }
    if errors > 0 {
        error!("there are untracked .rs files in the repository");
    }
}

fn commit_checkpoint(repo: &Repository) {
    let author = match Signature::now("cargo-incremental", "none") {
        Ok(author) => author,
        Err(e) => error!("failed to create git signature: {}", e),
    };

    let mut index = match repo.index() {
        Ok(index) => index,
        Err(e) => error!("{}", e),
    };

    let mut pathspecs = Vec::new();
    pathspecs.push("*");
    if let Err(e) = index.update_all(pathspecs, None) {
        error!("{}", e);
    }

    let updated_tree_oid = match index.write_tree() {
        Ok(oid) => oid,
        Err(e) => error!("failed to get oid for updated tree: {}", e),
    };

    let updated_tree = match repo.find_tree(updated_tree_oid) {
        Ok(tree) => tree,
        Err(e) => error!("{}", e),
    };

    let oid = match repo.refname_to_id("refs/heads/cargo-incremental-build") {
        Ok(oid) => oid,
        Err(e) => error!("failed to get oid for cargo-incremental branch: {}", e),
    };

    let last_commit_incr = match repo.find_commit(oid) {
        Ok(commit) => commit,
        Err(e) => error!("failed to get commit: {}", e),
    };

    let mut parents: Vec<&Commit> = Vec::new();
    parents.push(&last_commit_incr);
    let parents = parents;

    let result = repo.commit(Some("HEAD"),
                             &author,
                             &author,
                             "checkpoint",
                             &updated_tree,
                             parents.as_slice());

    match result {
        Ok(oid) => println!("Commit: {:?}", oid),
        Err(e) => error!("Failed to create commit: {}", e),
    };
}

pub fn build(args: &Args) {
    assert!(args.cmd_build);

    let cargo_toml_path = Path::new(&args.flag_cargo);

    if !cargo_toml_path.exists() || !cargo_toml_path.is_file() {
        error!("cargo path `{}` does not lead to a `Cargo.toml` file",
               cargo_toml_path.display());
    }

    let ref repo = match util::open_repo(cargo_toml_path) {
        Ok(repo) => repo,
        Err(e) => {
            error!("failed to find repository containing `{}`: {}",
                   cargo_toml_path.display(),
                   e)
        }
    };

    let repo_dir = cargo_toml_path.parent().unwrap();

    // Check that there are no are untracked .rs files that might affect the build.
    check_untracked_rs_files(repo);

    // Save the current head.
    let current_head = repo.head().unwrap();
    println!("head is: {:?}", current_head.shorthand().unwrap());

    // Checkout the branch "cargo-incremental-build".
    // TODO: Create the branch if it does not already exist.
    reset_branch(repo, "refs/heads/cargo-incremental-build");

    // Commit a checkpoint.
    println!("committing checkpoint");
    commit_checkpoint(repo);

    // Reset back to the initial head.
    println!("bringing head back to initial state");
    reset_branch(repo, current_head.name().unwrap());

    let incr_dir = Path::new("build-cache");

    let incr_options = if args.flag_just_current {
        IncrementalOptions::CurrentProject(&incr_dir)
    } else {
        IncrementalOptions::AllDeps(&incr_dir)
    };

    println!("Building..");
    let mut stats = CompilationStats::default();
    cargo_build(&repo_dir,
                &repo_dir,
                Path::new("target"),
                incr_options,
                &mut stats,
                false);
}
