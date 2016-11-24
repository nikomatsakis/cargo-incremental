use progress::Bar;
use regex::Regex;
use std::collections::BTreeSet;
use std::env;
use std::io::prelude::*;
use std::io::{self, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::fs::{File, OpenOptions};
use std::time;
use std::thread;

use super::Args;
use super::dfs;
use super::util;
use super::util::{cargo_build, CompilationStats, IncrementalOptions, TestResult,
                  TestCaseResult};

const CHECKOUT: &'static str = "checkout";
const NORMAL_BUILD: &'static str = "normal build";
const INCREMENTAL_BUILD: &'static str = "incremental build";
const COMPARE_BUILDS: &'static str = "compare incr/normal builds";
const NORMAL_TEST: &'static str = "normal test";
const INCREMENTAL_TEST: &'static str = "incremental test";
const COMPARE_TESTS: &'static str = "compare incr/normal tests";
const INCREMENTAL_BUILD_NO_CHANGE: &'static str = "incremental build / no change";
const INCREMENTAL_BUILD_NO_CACHE: &'static str = "incremental build / no cache";

const STAGES: &'static [&'static str] = &[CHECKOUT,
                                          NORMAL_BUILD,
                                          INCREMENTAL_BUILD,
                                          COMPARE_BUILDS,
                                          NORMAL_TEST,
                                          INCREMENTAL_TEST,
                                          COMPARE_TESTS,
                                          INCREMENTAL_BUILD_NO_CHANGE,
                                          INCREMENTAL_BUILD_NO_CACHE];

// Some file systems (e.g. HFS+ or FAT) record timestamps with rather low
// resolution, so we have to make sure to modify the test directory in intervals
// that the file system (and hence Cargo) will be able to handle.
const MIN_ITERATION_TIME_SECS: u64 = 2;

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
    let incr_options = if args.flag_just_current {
        IncrementalOptions::CurrentProject(&incr_dir)
    } else {
        IncrementalOptions::AllDeps(&incr_dir)
    };

    let incr_from_scratch_dir = work_dir.join("incr-from-scratch");

    let commits_dir = work_dir.join("commits");
    util::make_dir(&commits_dir);

    let cargo_dir = match cargo_toml_path.parent() {
        Some(p) => p,
        None => error!("Cargo.toml path has no parent: {}", args.flag_cargo),
    };

    let mut bar = Bar::new();
    let mut stats_normal = CompilationStats::default();
    let mut stats_incr = CompilationStats::default();
    let mut stats_incr_from_scratch = CompilationStats::default();

    let (mut tests_total, mut tests_passed) = (0, 0);

    for (index, commit) in commits.iter().enumerate() {
        let short_id = util::short_id(commit);
        let mut sub_task_runner = SubTaskRunner {
            progress_bar: &mut bar,
            commit_id: short_id.clone(),
            commit_index: index,
            cli_log: args.flag_cli_log,
            total_commit_count: commits.len(),
        };

        if args.flag_cli_log {
            println!("\nTESTING COMMIT {} ({} of {})", short_id, index + 1, commits.len());
        }

        sub_task_runner.run(CHECKOUT, || {
            util::checkout_commit(repo, commit);
            if args.flag_no_debuginfo {
                if let Err(err) = inject_no_debug_into_cargo_toml(&cargo_dir) {
                    error!("error while injecting no_debug into Cargo.toml: {}", err)
                }
            }
            ((), "OK")
        });

        let check_out_time = time::Instant::now();

        // NORMAL BUILD --------------------------------------------------------
        let normal_build_result = sub_task_runner.run(NORMAL_BUILD, || {
            let commit_dir = commits_dir.join(format!("{:04}-{}-normal-build", index, short_id));
            util::make_dir(&commit_dir);
            (cargo_build(&cargo_dir,
                         &commit_dir,
                         &target_normal_dir,
                         IncrementalOptions::None,
                         &mut stats_normal,
                         !args.flag_cli_log,
                         false),
             "OK")
        });

        // INCREMENTAL BUILD ---------------------------------------------------
        let incr_build_result = sub_task_runner.run(INCREMENTAL_BUILD, || {
            let commit_dir = commits_dir.join(format!("{:04}-{}-incr-build", index, short_id));
            util::make_dir(&commit_dir);
            (cargo_build(&cargo_dir,
                         &commit_dir,
                         &target_incr_dir,
                         incr_options,
                         &mut stats_incr,
                         !args.flag_cli_log,
                         false),
             "OK")
        });

        // COMPARE BUILD CLI OUTPUT --------------------------------------------
        sub_task_runner.run(COMPARE_BUILDS, || {
            if normal_build_result != incr_build_result {
                println!("OUTPUT OF NORMAL BUILD:\n");
                util::print_output(&normal_build_result.raw_output);

                println!("\nOUTPUT OF INCREMENTAL BUILD:\n");
                util::print_output(&incr_build_result.raw_output);

                error!("incremental build differed from normal build")
            } else {
                ((), "OK")
            }
        });

        // NORMAL TESTING ------------------------------------------------------
        let normal_test = sub_task_runner.run(NORMAL_TEST, || {
            if args.flag_skip_tests {
                return (None, "skipped");
            }

            let commit_dir = commits_dir.join(format!("{:04}-{}-normal-test", index, short_id));
            util::make_dir(&commit_dir);
            (Some(cargo_test(&cargo_dir,
                             &commit_dir,
                             &target_normal_dir,
                             IncrementalOptions::None)),
             "OK")
        });


        // INCREMENTAL TESTING -------------------------------------------------
        let incr_test = sub_task_runner.run(INCREMENTAL_TEST, || {
            if args.flag_skip_tests {
                return (None, "skipped");
            }

            let commit_dir = commits_dir.join(format!("{:04}-{}-incr-test", index, short_id));
            util::make_dir(&commit_dir);
            (Some(cargo_test(&cargo_dir,
                             &commit_dir,
                             &target_incr_dir,
                             incr_options)),
             "OK")
        });


        // COMPARE TEST RESULTS ------------------------------------------------
        sub_task_runner.run(COMPARE_TESTS, || {
            if args.flag_skip_tests {
                return ((), "skipped");
            }

            let normal_test = normal_test.clone().unwrap();
            let incr_test = incr_test.unwrap();

            if normal_test != incr_test {
                println!("OUTPUT OF NORMAL TESTS:\n");
                util::print_output(&normal_test.raw_output);

                println!("\nOUTPUT OF INCREMENTAL TESTS:\n");
                util::print_output(&incr_test.raw_output);

                error!("incremental tests differed from normal tests")
            } else {
                ((), "OK")
            }
        });


        // INCREMENTAL BUILD (FULL RE-USE) -------------------------------------
        sub_task_runner.run(INCREMENTAL_BUILD_NO_CHANGE, || {
            if incr_build_result.success {
                let commit_dir = commits_dir.join(format!("{:04}-{}-incr-build-full-re-use", index, short_id));
                util::make_dir(&commit_dir);

                // Delete Cargo's target directory so we don't run into Cargo's
                // smart re-using.
                util::remove_dir(&target_incr_dir);
                util::make_dir(&target_incr_dir);

                let mut full_reuse_stats = CompilationStats::default();
                assert_eq!(full_reuse_stats.modules_reused, 0);
                assert_eq!(full_reuse_stats.modules_total, 0);

                let result_no_change = cargo_build(&cargo_dir,
                                                   &commit_dir,
                                                   &target_incr_dir,
                                                   incr_options, // NOTE: we are using the same cache dir
                                                   &mut full_reuse_stats,
                                                   !args.flag_cli_log,
                                                   false);
                if result_no_change.success {
                    if full_reuse_stats.modules_reused != full_reuse_stats.modules_total {
                        error!("only {} modules out of {} re-used in full re-use test",
                                full_reuse_stats.modules_reused,
                                full_reuse_stats.modules_total)
                    }
                } else {
                    util::print_output(&result_no_change.raw_output);
                    error!("error during (no change) build!");
                }

                ((), "OK")
            } else {
                ((), "skipped")
            }
        });


        // INCREMENTAL BUILD (FROM SCRATCH) ------------------------------------
        sub_task_runner.run(INCREMENTAL_BUILD_NO_CACHE, || {
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
                let from_scratch_result = cargo_build(&cargo_dir,
                                                      &commit_dir,
                                                      &target_incr_from_scratch_dir,
                                                      incr_from_scratch_options,
                                                      &mut stats_incr_from_scratch,
                                                      !args.flag_cli_log,
                                                      false);
                if !from_scratch_result.success {
                    util::print_output(&from_scratch_result.raw_output);
                    error!("error during (incr-from-scratch) build!");
                }

                // CHECK THAT REGULAR AND FROM-SCRATCH INCREMENTAL COMPILATION YIELD THE
                // SAME RESULTS
                match compare_incr_comp_dirs(&incr_from_scratch_dir, &incr_dir) {
                    Ok(()) => ((), "OK"),
                    Err(err) => {
                        error!("{}\nTo reproduce execute: {}",
                               err,
                               args.to_cli_command())
                    }
                }
            } else {
                ((), "skipped")
            }
        });

        // UPDATE STATISTICS
        let test_results = normal_test.map(|x| x.results).unwrap_or(vec![]);
        tests_passed += test_results.iter().filter(|t| t.status == "ok").count();
        tests_total += test_results.len();

        if args.flag_no_debuginfo {
            // If we injected `debug = false` into the Cargo.toml, we better
            // reset the repo so it is clean for the next iteration.
            util::reset_repo(repo, commit);
        }

        while check_out_time.elapsed().as_secs() < MIN_ITERATION_TIME_SECS {
            thread::sleep(time::Duration::from_millis(200));
        }
    }

    if !args.flag_cli_log {
        bar.reach_percent(100);
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
            util::save_output(commit_dir, &output);
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
        raw_output: output,
    }
}

// Compare two incremental compilation cache directories:
//
// - For each crate directory in the reference directory, make sure that there
//   is a corresponding crate directory in the test directory
// - For each pair of crate directories, make sure they are equivalent
//
// The function aborts if it finds a difference.
fn compare_incr_comp_dirs(reference_dir: &Path,
                          tested_dir: &Path)
                          -> Result<(), String> {

    // The cache directory contains a sub-directory for each crate

    let reference_crate_dirs = util::dir_entries(reference_dir);
    let tested_crate_dirs = util::dir_entries(tested_dir);

    for reference_crate_dir in reference_crate_dirs {
        let reference_crate_id = reference_crate_dir.file_name().unwrap();

        let crate_dir_to_test = tested_crate_dirs.iter().find(|dir| {
            let crate_id = dir.file_name().unwrap();
            crate_id == reference_crate_id
        });

        let crate_dir_to_test = match crate_dir_to_test {
            Some(cd) => cd,
            None => {
                return Err(format!("no cache directory found for crate `{}`",
                                   reference_crate_id.to_string_lossy()));
            }
        };

        let reference_session_dir = try!(get_only_session_dir(&reference_crate_dir, None));

        // We have the reference session directory, now we want our test session
        // directory. It must be the one with exactly the same SVH as the
        // reference directory.
        let reference_session_dir_name = util::path_file_name(&reference_session_dir);
        let index = reference_session_dir_name.rfind("-").unwrap() + 1;
        let svh = Some(&reference_session_dir_name[index..]);
        let test_session_dir = try!(get_only_session_dir(&crate_dir_to_test, svh));

        try!(compare_incr_comp_session_dirs(&reference_session_dir, &test_session_dir));
    }

    Ok(())
}

// Compare two incr. comp. session directories:
//
// - Make sure that the two session directories contain exactly the same object
//   and bitcode files and that they have the same content.
// - Dep-graph and metadata files are not compared yet.
//
// The function aborts if it finds a difference.
fn compare_incr_comp_session_dirs(reference_crate_dir: &Path,
                                  crate_dir_to_test: &Path)
                                  -> Result<(), String> {

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

        return Err(message);
    }

    for file_name in ref_dir_file_names.iter() {
        // For now only compare compilation units (object files + bitcode).
        // Metadata, dep-graph, and exported hashes don't have a stable encoding
        // yet.
        if file_name.starts_with("cgu-") {
            let ref_file = reference_crate_dir.join(file_name);
            let test_file = crate_dir_to_test.join(file_name);

            try!(compare_files(&ref_file, &test_file));
        }
    }

    Ok(())
}

// From a crate-directory within the incremental compilation directory, get the
// sole session directory in there. If there is more than one directory,
// something is wrong and the function will abort.
fn get_only_session_dir(crate_dir: &Path,
                        svh: Option<&str>)
                        -> Result<PathBuf, String> {
    let dir_entries = util::dir_entries(crate_dir);

    return if let Some(svh) = svh {
        for entry in dir_entries {
            if entry.is_dir() {
                let dir_name = util::path_file_name(&entry);
                if dir_name.ends_with(svh) {
                    try!(check_well_formed_session_dir_name(&dir_name));
                    return Ok(entry);
                }
            }
        }

        Err(format!("Could not find session dir with SVH `{}` in `{}`.",
                    svh,
                    crate_dir.display()))
    } else {
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
            return Err(format!("Expected to find exactly one incr. comp. \
                                session directory in `{}` but found {}",
                               crate_dir.display(),
                               dirs_found));
        }

        let first_dir = first_dir.unwrap();
        let dir_name = util::path_file_name(&first_dir);
        try!(check_well_formed_session_dir_name(&dir_name));
        Ok(first_dir)
    };

    fn check_well_formed_session_dir_name(dir_name: &str) -> Result<(), String> {
        if !dir_name.starts_with("s-") {
            Err(format!("incr. comp. session directory has unexpected name `{}`",
                         dir_name))
        } else {
            Ok(())
        }
    }
}

// Compare two files byte-by-byte. The function aborts if it finds a difference.
fn compare_files(file1_path: &Path, file2_path: &Path) -> Result<(), String> {

    let mut file1 = try!(File::open(file1_path).map_err(|err| {
        format!("Could not open file `{}` for comparison: {}", file1_path.display(), err)
    }));

    let mut file2 = try!(File::open(file2_path).map_err(|err| {
        format!("Could not open file `{}` for comparison: {}", file2_path.display(), err)
    }));

    let file1_meta = try!(file1.metadata().map_err(|err| {
        format!("Could get file metadata of `{}` for comparison: {}", file1_path.display(), err)
    }));

    let file2_meta = try!(file2.metadata().map_err(|err| {
        format!("Could get file metadata of `{}` for comparison: {}", file2_path.display(), err)
    }));

    if file1_meta.len() != file2_meta.len() {
        return Err(format!("Files `{}` and `{}` have different length",
                           file1_path.display(),
                           file2_path.display()));
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
            return Err(format!("Files `{}` and `{}` have different content",
                               file1_path.display(),
                               file2_path.display()));
        }

        bytes_left -= bytes_to_read;
    }

    Ok(())
}


struct SubTaskRunner<'a> {
    progress_bar: &'a mut Bar,
    commit_index: usize,
    commit_id: String,
    cli_log: bool,
    total_commit_count: usize,
}

impl<'a> SubTaskRunner<'a> {

    fn run<F, T>(&mut self, task_label: &str, task: F) -> T
        where F: FnOnce() -> (T, &'static str)
    {
        let stage_index = STAGES.iter().position(|&x| x == task_label).unwrap();

        if self.cli_log {
            let stdout = ::std::io::stdout();
            let mut stdout = stdout.lock();
            write!(stdout, " - {} ... ", STAGES[stage_index]).unwrap();
            stdout.flush().unwrap();
        } else {
            let task_title = &format!("{} ({})", STAGES[stage_index], self.commit_id);
            self.progress_bar.set_job_title(task_title);
        }

        let (result, message) = task();

        if self.cli_log {
            println!("{}", message);
        } else {
            let num_stages = STAGES.len() as f32;
            let progress = (self.commit_index as f32 * num_stages) + (stage_index as f32);
            let total = (self.total_commit_count as f32) * num_stages;
            let percentage = progress / total * 100.0;
            self.progress_bar.reach_percent(percentage as i32);
        }

        result
    }
}

// This function injects a [profile.dev] into the given Cargo.toml that
// disables debuginfo. For now, it will just fail if there already is a
// [profile.dev] section.
fn inject_no_debug_into_cargo_toml(cargo_dir: &Path) -> io::Result<()> {

    let cargo_toml_path = cargo_dir.join("Cargo.toml");

    let mut file = try!(OpenOptions::new()
                                    .read(true)
                                    .write(true)
                                    .open(&cargo_toml_path));

    let mut contents = String::new();
    try!(file.read_to_string(&mut contents));

    if contents.contains("[profile.dev]") {
        let msg = format!("Cargo.toml already contains [profile.dev]: {}",
                           cargo_toml_path.display());
        return Err(io::Error::new(io::ErrorKind::Other, msg));
    }

    contents.push_str("\n");
    contents.push_str("[profile.dev]\n");
    contents.push_str("debug = false\n");
    contents.push_str("\n");

    try!(file.seek(SeekFrom::Start(0)));
    try!(file.write_all((&contents[..]).as_bytes()));

    Ok(())
}
