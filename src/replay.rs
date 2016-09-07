use git2::{Commit, Repository};
use git2::build::CheckoutBuilder;
use progress::Bar;
use regex::Regex;
use std::env;
use std::fs::File;
use std::io::prelude::*;
use std::path::Path;
use std::process::{Command, Output};
use std::str::FromStr;

use super::Args;
use super::dfs;
use super::util;

pub fn replay(args: &Args) {
    assert!(args.cmd_replay);

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

    util::check_clean(repo);

    let (from_commit, to_commit);
    if args.arg_branch_name.contains("..") {
        let revisions = match repo.revparse(&args.arg_branch_name) {
            Ok(revspec) => revspec,
            Err(err) => error!("failed to parse revspec `{}`: {}", args.arg_branch_name, err),
        };


        from_commit = match revisions.from() {
            Some(object) => Some(util::commit_or_error(object.clone())),
            None => {
                error!("revspec `{}` had no \"from\" point specified",
                       args.arg_branch_name)
            }
        };

        to_commit = match revisions.to() {
            Some(object) => util::commit_or_error(object.clone()),
            None => {
                error!("revspec `{}` had no \"to\" point specified; try something like `{}..HEAD`",
                       args.arg_branch_name,
                       args.arg_branch_name)
            }
        };
    } else {
        from_commit = None;
        to_commit = match repo.revparse_single(&args.arg_branch_name) {
            Ok(revspec) => util::commit_or_error(revspec),
            Err(err) => error!("failed to parse revspec `{}`: {}", args.arg_branch_name, err),
        };
    }

    let commits = dfs::find_path(from_commit, to_commit);

    // Start out by cleaning up any existing work directory.
    let work_dir = Path::new(&args.flag_work_dir);
    util::remove_dir(work_dir);

    // We structure our work directory like:
    //
    // work/target-incr <-- cargo state when building incrementally
    // work/incr <-- compiler state
    // work/commits/1231123 <-- output from building 1231123
    let target_incr_dir = util::absolute_dir_path(&work_dir.join("target-incr"));
    let target_normal_dir = util::absolute_dir_path(&work_dir.join("target-normal"));
    let incr_dir = util::absolute_dir_path(&work_dir.join("incr"));
    let commits_dir = work_dir.join("commits");
    util::make_dir(&commits_dir);

    let cargo_dir = match Path::new(&args.flag_cargo).parent() {
        Some(p) => p,
        None => error!("Cargo.toml path has no parent: {}", args.flag_cargo),
    };

    let mut bar = Bar::new();
    let stages =
        &["checkout", "normal build", "normal test", "incremental build", "incremental test"];
    let mut update_percent = |crate_index: usize, crate_id: &str, stage_index: usize| {
        bar.set_job_title(&format!("processing {} ({})", crate_id, stages[stage_index]));
        let num_stages = stages.len() as f32;
        let progress = (crate_index as f32 * num_stages) + (stage_index as f32);
        let total = (commits.len() as f32) * num_stages;
        let percentage = progress / total * 100.0;
        bar.reach_percent(percentage as i32);
    };
    let mut stats = vec![CompilationStats::default(), CompilationStats::default()];
    let (mut tests_total, mut tests_passed) = (0, 0);
    for (index, commit) in commits.iter().enumerate() {
        let short_id = util::short_id(commit);

        update_percent(index, &short_id, 0);
        checkout(repo, commit);

        update_percent(index, &short_id, 1);
        let commit_dir = commits_dir.join(format!("{:04}-{}-normal-build", index, short_id));
        util::make_dir(&commit_dir);
        let normal_messages = cargo_build(&cargo_dir,
                                          &commit_dir,
                                          &target_normal_dir,
                                          IncrementalOptions::None,
                                          &mut stats[0]);

        update_percent(index, &short_id, 2);
        let commit_dir = commits_dir.join(format!("{:04}-{}-normal-test", index, short_id));
        util::make_dir(&commit_dir);
        let normal_test = cargo_test(&cargo_dir,
                                     &commit_dir,
                                     &target_normal_dir,
                                     IncrementalOptions::None);

        update_percent(index, &short_id, 3);
        let commit_dir = commits_dir.join(format!("{:04}-{}-incr-build", index, short_id));
        util::make_dir(&commit_dir);
        let incr_options = if args.flag_just_current {
            IncrementalOptions::CurrentProject(&incr_dir)
        } else {
            IncrementalOptions::AllDeps(&incr_dir)
        };
        let incr_messages = cargo_build(&cargo_dir,
                                        &commit_dir,
                                        &target_incr_dir,
                                        incr_options,
                                        &mut stats[1]);

        update_percent(index, &short_id, 4);
        let commit_dir = commits_dir.join(format!("{:04}-{}-incr-test", index, short_id));
        util::make_dir(&commit_dir);
        let incr_test = cargo_test(&cargo_dir, &commit_dir, &target_incr_dir, incr_options);

        if normal_messages != incr_messages {
            error!("incremental build differed from normal build")
        }

        if normal_test != incr_test {
            error!("incremental tests differed from normal tests")
        }

        tests_passed += normal_test.results.iter().filter(|t| t.status == "ok").count();
        tests_total += normal_test.results.len();
    }

    assert!(stats[0].modules_reused == 0, "normal build reused modules");
    println!("");
    println!("Fuzzing report:");
    println!("- {} commits built", commits.len());
    println!("- normal compilation took {:.2}s", stats[0].build_time);
    println!("- incremental compilation took {:.2}s", stats[1].build_time);
    println!("- {} total tests executed ({} of those passed)",
             tests_total,
             tests_passed);
    println!("- normal/incremental ratio {:.2}",
             stats[0].build_time / stats[1].build_time);
    println!("- {} of {} (or {:.0}%) modules were re-used",
             stats[1].modules_reused,
             stats[1].modules_total,
             (stats[1].modules_reused as f64 / stats[1].modules_total as f64) * 100.0);
}


#[derive(Default)]
struct CompilationStats {
    build_time: f64, // in seconds
    modules_reused: u64,
    modules_total: u64,
}

#[derive(Copy, Clone, Debug)]
enum IncrementalOptions<'p> {
    None,
    AllDeps(&'p Path),
    CurrentProject(&'p Path),
}

#[derive(PartialEq, Eq, Debug)]
struct BuildResult {
    success: bool,
    messages: Vec<Message>,
}

#[derive(PartialEq, Eq, Debug)]
struct Message {
    kind: String,
    message: String,
    location: String,
}

#[derive(PartialEq, Eq, Debug)]
struct TestResult {
    success: bool,
    results: Vec<TestCaseResult>,
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
struct TestCaseResult {
    test_name: String,
    status: String,
}

fn cargo_build(cargo_dir: &Path,
               commit_dir: &Path,
               target_dir: &Path,
               incremental: IncrementalOptions,
               stats: &mut CompilationStats)
               -> BuildResult {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(&cargo_dir);
    cmd.env("CARGO_TARGET_DIR", target_dir);
    match incremental {
        IncrementalOptions::None => {
            cmd.arg("build").arg("-v");
        }
        IncrementalOptions::AllDeps(incr_dir) => {
            let rustflags = env::var("RUSTFLAGS").unwrap_or(String::new());
            cmd.arg("build")
                .arg("-v")
                .env("RUSTFLAGS",
                     format!("-Z incremental={} -Z incremental-info {}",
                             incr_dir.display(),
                             rustflags));
        }
        IncrementalOptions::CurrentProject(incr_dir) => {
            cmd.arg("rustc")
                .arg("-v")
                .arg("--")
                .arg("-Z")
                .arg(format!("incremental={}", incr_dir.display()))
                .arg("-Z")
                .arg("incremental-info");
        }
    }
    let output = cmd.output();
    let output = match output {
        Ok(output) => {
            save_output(commit_dir, &output);
            output
        }
        Err(err) => error!("failed to execute `cargo build`: {}", err),
    };

    // compute how much re-use we are getting
    let all_bytes: Vec<u8> = output.stdout
        .iter()
        .cloned()
        .chain(output.stderr.iter().cloned())
        .collect();
    let all_output = into_string(all_bytes);

    let reusing_regex = Regex::new(r"(?m)^incremental: re-using (\d+) out of (\d+) modules$")
        .unwrap();
    for captures in reusing_regex.captures_iter(&all_output) {
        let reused = u64::from_str(captures.at(1).unwrap()).unwrap();
        let total = u64::from_str(captures.at(2).unwrap()).unwrap();
        stats.modules_reused += reused;
        stats.modules_total += total;
    }

    let build_time_regex = Regex::new(r"(?m)^\s*Finished .* target\(s\) in ([0-9.]+) secs$")
        .unwrap();
    let mut build_time = None;
    for captures in build_time_regex.captures_iter(&all_output) {
        if build_time.is_some() {
            error!("cargo reported total build time twice");
        }

        build_time = Some(f64::from_str(captures.at(1).unwrap()).unwrap());
    }
    stats.build_time += match build_time {
        Some(v) => v,
        None => {
            // if cargo errors out, it sometimes does not report a build time
            if output.status.success(){
                error!("cargo build did not fail but failed to report total build time");
            }
            0.0
        }
    };

    let message_regex = Regex::new("(?m)(warning|error): (.*)\n  --> ([^:]:\\d+:\\d+)$").unwrap();
    let messages = message_regex.captures_iter(&all_output)
        .map(|captures| {
            Message {
                kind: captures.at(1).unwrap().to_string(),
                message: captures.at(2).unwrap().to_string(),
                location: captures.at(3).unwrap().to_string(),
            }
        })
        .collect();

    BuildResult {
        success: output.status.success(),
        messages: messages,
    }
}

fn cargo_test(cargo_dir: &Path,
              commit_dir: &Path,
              target_dir: &Path,
              incremental: IncrementalOptions)
              -> TestResult {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(&cargo_dir);
    cmd.env("CARGO_TARGET_DIR", target_dir);
    cmd.arg("test");
    match incremental {
        IncrementalOptions::None => {}
        IncrementalOptions::AllDeps(incr_dir) |
        IncrementalOptions::CurrentProject(incr_dir) => {
            let rustflags = env::var("RUSTFLAGS").unwrap_or(String::new());
            cmd.env("RUSTFLAGS",
                    format!("-Z incremental={} -Z incremental-info {}",
                            incr_dir.display(),
                            rustflags));
        }
    }
    let output = cmd.output();
    let output = match output {
        Ok(output) => {
            save_output(commit_dir, &output);
            output
        }
        Err(err) => error!("failed to execute `cargo build`: {}", err),
    };

    // compute set of tests and their results
    let all_bytes: Vec<u8> = output.stdout
        .iter()
        .cloned()
        .chain(output.stderr.iter().cloned())
        .collect();
    let all_output = into_string(all_bytes);

    let test_regex = Regex::new(r"(?m)^test (.*) \.\.\. (\w+)").unwrap();
    let mut test_results: Vec<_> = test_regex.captures_iter(&all_output)
        .map(|captures| {
            TestCaseResult {
                test_name: captures.at(1).unwrap().to_string(),
                status: captures.at(2).unwrap().to_string(),
            }
        })
        .collect();

    test_results.sort();

    let summary_regex = Regex::new(r"(?m)(\d+) passed; (\d+) failed; \d+ ignored; \d+ measured$")
        .unwrap();

    let nb_tests_summary = summary_regex.captures_iter(&all_output)
        .fold(0, |acc, captures| acc + captures.at(1).unwrap().parse::<usize>().unwrap() +
            captures.at(2).unwrap().parse::<usize>().unwrap());

    if nb_tests_summary != test_results.len() {
        error!("matched a different number of tests ({}) than in the summary ({})",
            test_results.len(), nb_tests_summary);
    }

    TestResult {
        success: output.status.success(),
        results: test_results,
    }
}

fn into_string(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(v) => v,
        Err(_) => error!("unable to parse output as utf-8"),
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
                   util::short_id(commit),
                   err)
        }
    }

    match repo.set_head_detached(commit.id()) {
        Ok(()) => {}
        Err(err) => {
            error!("encountered error adjusting head to `{}`: {}",
                   util::short_id(commit),
                   err)
        }
    }
}

