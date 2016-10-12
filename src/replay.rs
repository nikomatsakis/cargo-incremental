use progress::Bar;
use regex::Regex;
use std::env;
use std::io::prelude::*;
use std::path::Path;
use std::process::Command;

use super::Args;
use super::dfs;
use super::util;
use super::util::{cargo_build, CompilationStats, IncrementalOptions, TestResult,
                  TestCaseResult};

pub fn replay(args: &Args) {
    assert!(args.cmd_replay);

    let cargo_toml_path = Path::new(&args.flag_cargo);

    if !cargo_toml_path.exists() || !cargo_toml_path.is_file() {
        error!("cargo path `{}` does not lead to a `Cargo.toml` file",
               cargo_toml_path.display());
    }

    let cargo_toml_pathref = cargo_toml_path.canonicalize().unwrap();
    let cargo_toml_path = cargo_toml_pathref.as_path();

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
            Err(err) => {
                error!("failed to parse revspec `{}`: {}",
                       args.arg_branch_name,
                       err)
            }
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
            Err(err) => {
                error!("failed to parse revspec `{}`: {}",
                       args.arg_branch_name,
                       err)
            }
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

    let cargo_dir = match cargo_toml_path.parent() {
        Some(p) => p,
        None => error!("Cargo.toml path has no parent: {}", args.flag_cargo),
    };

    let mut bar = Bar::new();

    let stages =
        &["checkout", "normal build", "normal test", "incremental build", "incremental test"];
    let mut update_percent = |crate_index: usize, crate_id: &str, stage_index: usize| {
        if args.flag_cli_log {
            println!("processing {} ({})", crate_id, stages[stage_index]);
        } else {
            bar.set_job_title(&format!("processing {} ({})", crate_id, stages[stage_index]));
            let num_stages = stages.len() as f32;
            let progress = (crate_index as f32 * num_stages) + (stage_index as f32);
            let total = (commits.len() as f32) * num_stages;
            let percentage = progress / total * 100.0;
            bar.reach_percent(percentage as i32);
        }
    };
    let mut stats = vec![CompilationStats::default(), CompilationStats::default()];
    let (mut tests_total, mut tests_passed) = (0, 0);
    for (index, commit) in commits.iter().enumerate() {
        let short_id = util::short_id(commit);

        update_percent(index, &short_id, 0);
        util::checkout(repo, commit);

        update_percent(index, &short_id, 1);
        let commit_dir = commits_dir.join(format!("{:04}-{}-normal-build", index, short_id));
        util::make_dir(&commit_dir);
        let normal_messages = cargo_build(&cargo_dir,
                                          &commit_dir,
                                          &target_normal_dir,
                                          IncrementalOptions::None,
                                          &mut stats[0],
                                          !args.flag_cli_log,
                                          args.flag_cli_log);

        update_percent(index, &short_id, 2);
        let commit_dir = commits_dir.join(format!("{:04}-{}-normal-test", index, short_id));
        util::make_dir(&commit_dir);
        let normal_test = cargo_test(&cargo_dir,
                                     &commit_dir,
                                     &target_normal_dir,
                                     IncrementalOptions::None,
                                     args.flag_cli_log);

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
                                        &mut stats[1],
                                        !args.flag_cli_log,
                                        args.flag_cli_log);

        update_percent(index, &short_id, 4);
        let commit_dir = commits_dir.join(format!("{:04}-{}-incr-test", index, short_id));
        util::make_dir(&commit_dir);
        let incr_test = cargo_test(&cargo_dir,
                                   &commit_dir,
                                   &target_incr_dir,
                                   incr_options,
                                   args.flag_cli_log);

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


fn cargo_test(cargo_dir: &Path,
              commit_dir: &Path,
              target_dir: &Path,
              incremental: IncrementalOptions,
              cli_log_mode: bool)
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
            if cli_log_mode {
                util::print_output(&output);
            } else {
                util::save_output(commit_dir, &output);
            }
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
    let all_output = util::into_string(all_bytes);

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

    let summary_regex = Regex::new(r"(?m)(\d+) passed; (\d+) failed; (\d+) ignored; \d+ measured$")
        .unwrap();

    let nb_tests_summary = summary_regex.captures_iter(&all_output)
        .fold(0, |acc, captures| {
            acc +
              captures.at(1).unwrap().parse::<usize>().unwrap() + // passed
              captures.at(2).unwrap().parse::<usize>().unwrap() + // failed
              captures.at(3).unwrap().parse::<usize>().unwrap()   // ignored
        });

    if nb_tests_summary != test_results.len() {
        error!("matched a different number of tests ({}) than in the summary ({})",
               test_results.len(),
               nb_tests_summary);
    }

    TestResult {
        success: output.status.success(),
        results: test_results,
    }
}
