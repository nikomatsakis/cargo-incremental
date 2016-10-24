use progress::Bar;
use regex::Regex;
use std::collections::BTreeSet;
use std::env;
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::fs::File;

use super::Args;
use super::dfs;
use super::util;
use super::util::{cargo_build, CompilationStats, IncrementalOptions, TestResult,
                  TestCaseResult};

pub fn replay(args: &Args) {
    assert!(args.cmd_replay);
    debug!("replay(): revisions = {}", args.arg_revisions);

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

    // Filter down to the range of revisions specified by the user
    let (from_commit, to_commit);
    if args.arg_revisions.contains("..") {
        let revisions = match repo.revparse(&args.arg_revisions) {
            Ok(revspec) => revspec,
            Err(err) => {
                error!("failed to parse revspec `{}`: {}",
                       args.arg_revisions,
                       err)
            }
        };


        from_commit = match revisions.from() {
            Some(object) => Some(util::commit_or_error(object.clone())),
            None => {
                error!("revspec `{}` had no \"from\" point specified",
                       args.arg_revisions)
            }
        };

        to_commit = match revisions.to() {
            Some(object) => util::commit_or_error(object.clone()),
            None => {
                error!("revspec `{}` had no \"to\" point specified; try something like `{}..HEAD`",
                       args.arg_revisions,
                       args.arg_revisions)
            }
        };
    } else {
        from_commit = None;
        to_commit = match repo.revparse_single(&args.arg_revisions) {
            Ok(revspec) => util::commit_or_error(revspec),
            Err(err) => {
                error!("failed to parse revspec `{}`: {}",
                       args.arg_revisions,
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
    // work/incr <-- incremental compilation cache
    // work/from_scratch <-- incremental compilation cache for from-scratch builds
    // work/commits/1231123 <-- output from building 1231123
    let target_incr_dir = util::absolute_dir_path(&work_dir.join("target-incr"));
    let target_normal_dir = util::absolute_dir_path(&work_dir.join("target-normal"));
    let target_incr_from_scratch_dir = util::absolute_dir_path(&work_dir.join("target-incr-from-scratch"));
    let incr_dir = util::absolute_dir_path(&work_dir.join("incr"));
    let incr_from_scratch_dir = work_dir.join("incr-from-scratch");

    let commits_dir = work_dir.join("commits");
    util::make_dir(&commits_dir);

    let cargo_dir = match cargo_toml_path.parent() {
        Some(p) => p,
        None => error!("Cargo.toml path has no parent: {}", args.flag_cargo),
    };

    let mut bar = Bar::new();

    const CHECKOUT: &'static str = "checkout";
    const NORMAL_BUILD: &'static str = "normal build";
    const NORMAL_TEST: &'static str = "normal test";
    const INCREMENTAL_BUILD: &'static str = "incremental build";
    const INCREMENTAL_TEST: &'static str = "incremental test";
    const INCREMENTAL_BUILD_NO_CHANGE: &'static str = "incremental build (no change)";
    const INCREMENTAL_BUILD_NO_CACHE: &'static str = "incremental build (no cache)";

    const STAGES: &'static [&'static str] = &[CHECKOUT,
                                              NORMAL_BUILD,
                                              NORMAL_TEST,
                                              INCREMENTAL_BUILD,
                                              INCREMENTAL_TEST,
                                              INCREMENTAL_BUILD_NO_CHANGE,
                                              INCREMENTAL_BUILD_NO_CACHE];

    let mut update_percent = |crate_index: usize, crate_id: &str, stage_label: &str| {
        let stage_index = STAGES.iter().position(|&x| x == stage_label).unwrap();
        if args.flag_cli_log {
            println!("processing {} ({})", crate_id, STAGES[stage_index]);
        } else {
            bar.set_job_title(&format!("processing {} ({})", crate_id, STAGES[stage_index]));
            let num_stages = STAGES.len() as f32;
            let progress = (crate_index as f32 * num_stages) + (stage_index as f32);
            let total = (commits.len() as f32) * num_stages;
            let percentage = progress / total * 100.0;
            bar.reach_percent(percentage as i32);
        }
    };
    let mut stats_normal = CompilationStats::default();
    let mut stats_incr = CompilationStats::default();
    let mut stats_incr_from_scratch = CompilationStats::default();

    let (mut tests_total, mut tests_passed) = (0, 0);

    for (index, commit) in commits.iter().enumerate() {
        let short_id = util::short_id(commit);

        update_percent(index, &short_id, CHECKOUT);
        util::checkout_commit(repo, commit);

        // NORMAL BUILD --------------------------------------------------------
        update_percent(index, &short_id, NORMAL_BUILD);
        let commit_dir = commits_dir.join(format!("{:04}-{}-normal-build", index, short_id));
        util::make_dir(&commit_dir);
        let normal_build_result = cargo_build(&cargo_dir,
                                              &commit_dir,
                                              &target_normal_dir,
                                              IncrementalOptions::None,
                                              &mut stats_normal,
                                              !args.flag_cli_log,
                                              args.flag_cli_log);

        // NORMAL TESTING ------------------------------------------------------
        update_percent(index, &short_id, NORMAL_TEST);
        let commit_dir = commits_dir.join(format!("{:04}-{}-normal-test", index, short_id));
        util::make_dir(&commit_dir);
        let normal_test = cargo_test(&cargo_dir,
                                     &commit_dir,
                                     &target_normal_dir,
                                     IncrementalOptions::None,
                                     args.flag_cli_log);


        // INCREMENTAL BUILD ---------------------------------------------------
        update_percent(index, &short_id, INCREMENTAL_BUILD);
        let commit_dir = commits_dir.join(format!("{:04}-{}-incr-build", index, short_id));
        util::make_dir(&commit_dir);
        let incr_options = if args.flag_just_current {
            IncrementalOptions::CurrentProject(&incr_dir)
        } else {
            IncrementalOptions::AllDeps(&incr_dir)
        };
        let incr_build_result = cargo_build(&cargo_dir,
                                            &commit_dir,
                                            &target_incr_dir,
                                            incr_options,
                                            &mut stats_incr,
                                            !args.flag_cli_log,
                                            args.flag_cli_log);


        // COMPARE BUILD CLI OUTPUT --------------------------------------------
        if normal_build_result != incr_build_result {
            error!("incremental build differed from normal build")
        }


        // INCREMENTAL TESTING -------------------------------------------------
        update_percent(index, &short_id, INCREMENTAL_TEST);
        let commit_dir = commits_dir.join(format!("{:04}-{}-incr-test", index, short_id));
        util::make_dir(&commit_dir);
        let incr_test = cargo_test(&cargo_dir,
                                   &commit_dir,
                                   &target_incr_dir,
                                   incr_options,
                                   args.flag_cli_log);


        // COMPARE TEST RESULTS ------------------------------------------------
        if normal_test != incr_test {
            error!("incremental tests differed from normal tests")
        }


        // INCREMENTAL BUILD (FULL RE-USE) -------------------------------------
        update_percent(index, &short_id, INCREMENTAL_BUILD_NO_CHANGE);
        if incr_build_result.success {
            let commit_dir = commits_dir.join(format!("{:04}-{}-incr-build-full-re-use", index, short_id));
            util::make_dir(&commit_dir);
            let mut full_reuse_stats = CompilationStats::default();
            assert_eq!(full_reuse_stats.modules_reused, 0);
            assert_eq!(full_reuse_stats.modules_total, 0);
            cargo_build(&cargo_dir,
                        &commit_dir,
                        &target_incr_dir,
                        incr_options, // NOTE: we are using the same cache dir
                        &mut full_reuse_stats,
                        !args.flag_cli_log,
                        args.flag_cli_log);


            // CHECK FULL RE-USE ---------------------------------------------------
            if full_reuse_stats.modules_reused != full_reuse_stats.modules_total {
                error!("only {} modules out of {} re-used in full re-use test",
                        full_reuse_stats.modules_reused,
                        full_reuse_stats.modules_total)
            }
        }


        // INCREMENTAL BUILD (FROM SCRATCH) ------------------------------------
        update_percent(index, &short_id, INCREMENTAL_BUILD_NO_CACHE);
        if incr_build_result.success {
            let commit_dir = commits_dir.join(format!("{:04}-{}-incr-build-from-scratch", index, short_id));
            util::make_dir(&commit_dir);
            // We want to do a clean rebuild in incremental mode, so clear the
            // incremental compilation cache
            util::remove_dir(&incr_from_scratch_dir);
            util::make_dir(&incr_from_scratch_dir);
            util::remove_dir(&target_incr_from_scratch_dir);
            util::make_dir(&target_incr_from_scratch_dir);
            let incr_from_scratch_options = if args.flag_just_current {
                IncrementalOptions::CurrentProject(&incr_from_scratch_dir)
            } else {
                IncrementalOptions::AllDeps(&incr_from_scratch_dir)
            };
            let _ = cargo_build(&cargo_dir,
                                &commit_dir,
                                &target_incr_from_scratch_dir,
                                incr_from_scratch_options,
                                &mut stats_incr_from_scratch,
                                !args.flag_cli_log,
                                args.flag_cli_log);


            // CHECK THAT REGULAR AND FROM-SCRATCH INCREMENTAL COMPILATION YIELD THE
            // SAME RESULTS
            compare_incr_comp_dirs(&incr_from_scratch_dir, &incr_dir);
        }


        // UPDATE STATISTICS
        tests_passed += normal_test.results.iter().filter(|t| t.status == "ok").count();
        tests_total += normal_test.results.len();
    }

    assert!(stats_normal.modules_reused == 0, "normal build reused modules");
    println!("");
    println!("Fuzzing report:");
    println!("- {} commits built", commits.len());
    println!("- normal compilation took {:.2}s", stats_normal.build_time);
    println!("- incremental compilation took {:.2}s", stats_incr.build_time);
    println!("- {} total tests executed ({} of those passed)",
             tests_total,
             tests_passed);
    println!("- normal/incremental ratio {:.2}",
             stats_normal.build_time / stats_incr.build_time);
    println!("- {} of {} (or {:.0}%) modules were re-used",
             stats_incr.modules_reused,
             stats_incr.modules_total,
             (stats_incr.modules_reused as f64 / stats_incr.modules_total as f64) * 100.0);
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

// Compare two incremental compilation cache directories:
//
// - For each crate directory in the reference directory, make sure that there
//   is a corresponding crate directory in the test directory
// - For each pair of crate directories, make sure they are equivalent
//
// The function aborts if it finds a difference.
fn compare_incr_comp_dirs(reference_dir: &Path, tested_dir: &Path) {

    // The cache directory contains a sub-directory for each crate

    let reference_crate_dirs = util::dir_entries(reference_dir);
    let tested_crate_dirs = util::dir_entries(tested_dir);

    for reference_crate_dir in reference_crate_dirs {
        let reference_crate_id = reference_crate_dir.file_name().unwrap();

        let crate_dir_to_test = tested_crate_dirs.iter().find(|dir| {
            let crate_id = dir.file_name().unwrap();
            crate_id == reference_crate_id
        }).unwrap_or_else(|| {
            error!("no cache directory found for crate `{}`",
                   reference_crate_id.to_string_lossy())
        });

        let reference_session_dir = get_only_session_dir(&reference_crate_dir);
        let test_session_dir = get_only_session_dir(&crate_dir_to_test);

        compare_incr_comp_session_dirs(&reference_session_dir, &test_session_dir);
    }
}

// Compare two incr. comp. session directories:
//
// - Make sure that the two session directories contain exactly the same object
//   and bitcode files and that they have the same content.
// - Dep-graph and metadata files are not compared yet.
//
// The function aborts if it finds a difference.
fn compare_incr_comp_session_dirs(reference_crate_dir: &Path,
                                  crate_dir_to_test: &Path) {

    let ref_dir_entries = util::dir_entries(reference_crate_dir);
    let test_dir_entries = util::dir_entries(crate_dir_to_test);

    let ref_dir_file_names: BTreeSet<String> = ref_dir_entries
        .iter()
        .map(|p| p.file_name().unwrap())
        .map(|s| s.to_string_lossy().into_owned())
        .collect();

    let test_dir_file_names: BTreeSet<String> = test_dir_entries
        .iter()
        .map(|p| p.file_name().unwrap())
        .map(|s| s.to_string_lossy().into_owned())
        .collect();

    if ref_dir_file_names != test_dir_file_names {
        let mut message = String::new();
        message.push_str("The following files are missing in test dir:\n");

        for name in ref_dir_file_names.difference(&test_dir_file_names) {
            message.push_str(&format!(" - {}\n", name));
        }

        message.push_str("\nThe following files in test dir should not be there:\n");

        for name in test_dir_file_names.difference(&ref_dir_file_names) {
            message.push_str(&format!(" - {}\n", name));
        }

        error!("{}", message)
    }

    for file_name in ref_dir_file_names.iter() {
        // For now only compare compilation units (object files + bitcode).
        // Metadata, dep-graph, and exported hashes don't have a stable encoding
        // yet.
        if file_name.starts_with("cgu-") {
            let ref_file = reference_crate_dir.join(file_name);
            let test_file = crate_dir_to_test.join(file_name);

            compare_files(&ref_file, &test_file);
        }
    }
}

// From a crate-directory within the incremental compilation directory, get the
// sole session directory in there. If there is more than one directory,
// something is wrong and the function will abort.
fn get_only_session_dir(crate_dir: &Path) -> PathBuf {
    let dir_entries = util::dir_entries(crate_dir);

    let mut dirs_found = 0;
    let mut first_dir = None;

    for entry in dir_entries {
        if entry.is_dir() {
            dirs_found += 1;
            if first_dir.is_none() {
                first_dir = Some(entry);
            }
        }
    }

    if dirs_found != 1 {
        error!("Expected to find exactly one incr. comp. session directory in \
                `{}` but found {}",
               crate_dir.display(), dirs_found)
    }

    let first_dir = first_dir.unwrap();
    let dir_name = first_dir.file_name().unwrap().to_string_lossy().into_owned();

    if !dir_name.starts_with("s-") {
        error!("incr. comp. session directory has unexpected name `{}`",
               dir_name)
    }

    first_dir
}

// Compare two files byte-by-byte. The function aborts if it finds a difference.
fn compare_files(file1_path: &Path, file2_path: &Path) {

    let mut file1 = File::open(file1_path).unwrap_or_else(|err| {
        error!("Could not open file `{}` for comparison: {}",
               file1_path.display(),
               err)
    });

    let mut file2 = File::open(file2_path).unwrap_or_else(|err| {
        error!("Could not open file `{}` for comparison: {}",
               file2_path.display(),
               err)
    });

    let file1_meta = file1.metadata().unwrap_or_else(|err| {
        error!("Could get file metadata of `{}` for comparison: {}",
               file1_path.display(),
               err)
    });

    let file2_meta = file2.metadata().unwrap_or_else(|err| {
        error!("Could get file metadata of `{}` for comparison: {}",
               file2_path.display(),
               err)
    });

    if file1_meta.len() != file2_meta.len() {
        error!("Files `{}` and `{}` have different length",
               file1_path.display(),
               file2_path.display())
    }

    let mut bytes_left = file1_meta.len() as usize;

    const BUFFER_SIZE: usize = 4096;
    let mut buf1 = [0u8; BUFFER_SIZE];
    let mut buf2 = [0u8; BUFFER_SIZE];

    while bytes_left > 0 {
        let bytes_to_read = ::std::cmp::min(bytes_left, BUFFER_SIZE);
        file1.read_exact(&mut buf1[0 .. bytes_to_read]).unwrap();
        file2.read_exact(&mut buf2[0 .. bytes_to_read]).unwrap();

        if &buf1[0 .. bytes_to_read] != &buf2[0 .. bytes_to_read] {
            error!("Files `{}` and `{}` have different content",
                   file1_path.display(),
                   file2_path.display())
        }

        bytes_left -= bytes_to_read;
    }
}
