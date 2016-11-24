use std::io::prelude::*;
use std::path::Path;
use std::io;

use git2::{self, BranchType, Commit, Reference, Repository, Signature};

use super::Args;
use super::util;
use super::util::{cargo_build, CompilationStats, IncrementalOptions};

pub fn build(args: &Args) {
    assert!(args.cmd_build);

    let cargo_toml_pathbuf = Path::new(&args.flag_cargo).canonicalize().unwrap();
    let cargo_toml_path = cargo_toml_pathbuf.as_path();

    let repo = &match util::open_repo(cargo_toml_path) {
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


    // Checkout the branch "cargo-incremental-build", create it if it does not already
    // exist.
    create_branch_if_new(repo, "cargo-incremental-build", &current_head);
    reset_branch(repo, "refs/heads/cargo-incremental-build");

    // Commit a checkpoint.
    maybe_commit_checkpoint(repo);

    // Reset back to the initial head.
    println!("bringing head back to initial state");
    reset_branch(repo, current_head.name().unwrap());

    let incr_dir = Path::new("build-cache");

    let incr_options = if args.flag_just_current {
        IncrementalOptions::CurrentProject(incr_dir)
    } else {
        IncrementalOptions::AllDeps(incr_dir)
    };

    println!("Building..");
    let mut stats = CompilationStats::default();
    let build_result = cargo_build(repo_dir,
                                   repo_dir,
                                   Path::new("target"),
                                   incr_options,
                                   &mut stats,
                                   false,
                                   true);

    for m in build_result.messages {
        println!("{}", m.message);
    }

    let build_reuse = match stats.modules_total as f32 {
        0.0 => 100.0,
        n => stats.modules_reused as f32 / n * 100.0,
    };

    println!("Modules reused: {} Total: {} Build reuse: {}%",
             stats.modules_reused,
             stats.modules_total,
             build_reuse);
}

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
        if status.status().intersects(git2::STATUS_WT_NEW) {
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

fn create_branch_if_new(repo: &Repository, name: &str, head: &Reference) {
    if let Ok(_) = repo.find_branch(name, BranchType::Local) {
        return;
    }

    println!("creating branch 'cargo-incremental-build'");
    let commit = repo.find_commit(head.target().unwrap()).unwrap();
    if let Err(e) = repo.branch(name, &commit, false) {
        error!("failed to create branch '{}': {}", name, e);
    }
}

fn maybe_commit_checkpoint(repo: &Repository) {
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
    let pathspecs = pathspecs;

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

    // Check if there are actually any changes
    let last_commit_tree = last_commit_incr.tree().unwrap();
    if updated_tree.len() == last_commit_tree.len() {
        let has_changed = updated_tree.iter().any(|entry| {
            last_commit_tree.get_id(entry.id()).is_none()
        });

        if !has_changed {
            println!("not creating new checkpoint since there are no changes");
            return
        }
    }

    let mut parents: Vec<&Commit> = Vec::new();
    parents.push(&last_commit_incr);
    let parents = parents;

    println!("committing checkpoint");
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
