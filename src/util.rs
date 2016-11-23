use git2::{Commit, Error as Git2Error, ErrorCode, Object, Repository, Status,
           STATUS_IGNORED, ResetType};
use git2::build::CheckoutBuilder;
use std::fs;
use std::io;
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use regex::Regex;
use std::env;
use std::str::FromStr;
use std::fs::File;
use std::thread::{self, JoinHandle};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
pub struct CompilationStats {
    pub build_time: f64, // in seconds
    pub modules_reused: u64,
    pub modules_total: u64,
}

#[derive(Copy, Clone, Debug)]
pub enum IncrementalOptions<'p> {
    None,
    AllDeps(&'p Path),
    CurrentProject(&'p Path),
}

#[derive(Eq, Debug, Clone)]
pub struct BuildResult {
    pub success: bool,
    pub messages: Vec<Message>,
    pub raw_output: Output,
}

impl PartialEq for BuildResult {
    fn eq(&self, other: &BuildResult) -> bool {
        self.success == other.success &&
        self.messages == other.messages
    }
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub struct Message {
    pub kind: String,
    pub message: String,
    pub location: String,
}

#[derive(Eq, Debug, Clone)]
pub struct TestResult {
    pub success: bool,
    pub results: Vec<TestCaseResult>,
    pub raw_output: Output,
}

impl PartialEq for TestResult {
    fn eq(&self, other: &TestResult) -> bool {
        self.success == other.success &&
        self.results == other.results
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Debug, Clone)]
pub struct TestCaseResult {
    pub test_name: String,
    pub status: String,
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

pub fn save_output(output_dir: &Path, output: &Output) {
    write_file(&output_dir.join("status"),
               format!("{}", output.status).as_bytes());
    write_file(&output_dir.join("stdout"), &output.stdout);
    write_file(&output_dir.join("stderr"), &output.stderr);
}

pub fn print_output(output: &Output) {
    println!("");
    println!("EXIT STATUS:");
    println!("=============");
    println!("{}", output.status);
    println!("");

    println!("STANDARD OUT");
    println!("============");
    println!("{}", into_string(output.stdout.clone()));
    println!("");

    println!("STANDARD ERR");
    println!("============");
    println!("{}", into_string(output.stderr.clone()));
}

pub fn make_dir(path: &Path) {
    match fs::create_dir_all(path) {
        Ok(()) => {}
        Err(err) => error!("cannot create work-directory `{}`: {}", path.display(), err),
    }
}

pub fn into_string(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(v) => v,
        Err(_) => error!("unable to parse output as utf-8"),
    }
}

pub fn open_repo(cargo_path: &Path) -> Result<Repository, Git2Error> {
    let mut git_path = cargo_path;

    loop {
        if git_path.is_dir() {
            match Repository::open(git_path) {
                Ok(r) => {
                    println!("repo at {}", git_path.display());
                    return Ok(r);
                }
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

pub fn reset_repo(repo: &Repository, commit: &Commit) {
    let mut cb = CheckoutBuilder::new();
    if let Err(err) = repo.reset(commit.as_object(),
                                 ResetType::Hard,
                                 Some(&mut cb)) {
        error!("encountered error while resetting repo: {}", err)
    }
}

pub fn checkout_commit(repo: &Repository, commit: &Commit) {
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

pub fn cargo_build(cargo_dir: &Path,
                   commit_dir: &Path,
                   target_dir: &Path,
                   incremental: IncrementalOptions,
                   stats: &mut CompilationStats,
                   should_save_output: bool,
                   stream_output: bool)
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

    let output = if stream_output {
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut process = cmd.spawn().unwrap_or_else(|err| {
            error!("failed to spawn `cargo build` process: {}", err)
        });

        let done = Arc::new(AtomicBool::new(false));

        let stdout_reader = spawn_stream_reader(done.clone(),
                                                process.stdout.take().unwrap(),
                                                |bytes| {
                                                    let stdout = io::stdout();
                                                    let mut stdout = stdout.lock();
                                                    stdout.write_all(bytes).unwrap();
                                                });

        let stderr_reader = spawn_stream_reader(done.clone(),
                                                process.stderr.take().unwrap(),
                                                |bytes| {
                                                    let stderr = io::stderr();
                                                    let mut stderr = stderr.lock();
                                                    stderr.write_all(bytes).unwrap();
                                                });

        let exit_status = process.wait().unwrap_or_else(|err| {
            error!("error while waiting for `cargo build` process to finish: {}",
                   err)
        });

        done.store(true, Ordering::SeqCst);

        let stdout = stdout_reader.join().unwrap_or_else(|_| {
            error!("error while reading child process stdout")
        });

        let stderr = stderr_reader.join().unwrap_or_else(|_| {
            error!("error while reading child process stderr")
        });

        Ok(Output {
            status: exit_status,
            stdout: stdout,
            stderr: stderr,
        })
    } else {
        cmd.output()
    };

    let output = match output {
        Ok(output) => {
            if should_save_output {
                save_output(commit_dir, &output);
            }

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
            if output.status.success() {
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

    return BuildResult {
        success: output.status.success(),
        messages: messages,
        raw_output: output,
    };

    fn spawn_stream_reader<S, F>(done_flag: Arc<AtomicBool>,
                                 mut stream: S,
                                 forward: F)
                                 -> JoinHandle<Vec<u8>>
        where S: Read+Send+'static,
              F: Fn(&[u8])+Send+'static
    {
        thread::spawn(move || {
            let mut data = Vec::new();
            let mut buffer = [0u8; 100];

            while !done_flag.load(Ordering::SeqCst) {
                let byte_count = stream.read(&mut buffer).unwrap_or_else(|_| {
                    error!("error reading from child process pipe")
                });

                forward(&buffer[0 .. byte_count]);
                data.extend(&buffer[0 .. byte_count]);
            }

            let size_before = data.len();
            stream.read_to_end(&mut data).unwrap_or_else(|_| {
                error!("error reading from child process pipe")
            });

            forward(&data[size_before..]);

            data
        })
    }
}

pub fn dir_entries(dir: &Path) -> Vec<PathBuf> {
    debug!("dir_entries({})", dir.display());
    let dir_iter = fs::read_dir(dir).unwrap_or_else(|err| {
        error!("could not read directory `{}`: {}", dir.display(), err)
    });

    dir_iter.map(|entry| {
        let entry = entry.unwrap_or_else(|err| {
            error!("could not read reference directory entry: {}", err)
        });

        let path = entry.path().canonicalize().unwrap();
        debug!("dir_entries: - {}", path.display());
        path
    })
    .collect()
}

pub fn path_file_name(entry: &Path) -> String {
    entry.file_name().unwrap().to_string_lossy().into_owned()
}
