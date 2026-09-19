use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use engram::proof::t1772::{
    self, CANDIDATE_BASE, INPUT_ROOT, MANIFEST_NAME, MANIFEST_SHA256, PROOF_ROOT, ProofResult,
    SIGCONT_NUMBER, canonical_json_lf, collect_custody, manifest_result, require_exact_path,
    sha256_file, verify_input_manifest, write_canonical_json, write_canonical_jsonl,
};
use serde_json::{Value, json};

const BUILD_REVISION: &str = match option_env!("T1772_BUILD_REVISION") {
    Some(value) => value,
    None => "UNSET",
};

#[derive(Debug, Parser)]
#[command(name = "t1772-p0-controller")]
struct Args {
    #[arg(long)]
    source_root: PathBuf,
    #[arg(long)]
    source_revision: String,
    #[arg(long)]
    input_root: PathBuf,
    #[arg(long)]
    proof_root: PathBuf,
    #[arg(long)]
    tape_root: PathBuf,
    #[arg(long)]
    live_index: PathBuf,
    #[arg(long)]
    cursor_root: PathBuf,
    #[arg(long)]
    manifest: PathBuf,
    #[arg(long)]
    runner: PathBuf,
    #[arg(long)]
    runner_sha256: String,
    #[arg(long)]
    controller_sha256: String,
    #[arg(long)]
    candidate_binary: PathBuf,
    #[arg(long)]
    candidate_binary_sha256: String,
    #[arg(long)]
    baseline_binary: PathBuf,
    #[arg(long)]
    baseline_database: PathBuf,
    #[arg(long)]
    baseline_database_sha256: String,
}

struct LifecycleWriter {
    writer: BufWriter<File>,
    sequence: u64,
}

struct ManagedChild {
    child: Option<Child>,
}

impl ManagedChild {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn id(&self) -> u32 {
        self.child.as_ref().expect("managed child is present").id()
    }

    fn wait(&mut self) -> std::io::Result<ExitStatus> {
        let status = self
            .child
            .as_mut()
            .expect("managed child is present")
            .wait()?;
        self.child.take();
        Ok(status)
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl LifecycleWriter {
    fn create(path: &Path) -> ProofResult<Self> {
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        Ok(Self {
            writer: BufWriter::new(file),
            sequence: 0,
        })
    }

    fn record(&mut self, event: &str, detail: Value) -> ProofResult<()> {
        self.sequence += 1;
        let value = json!({
            "sequence": self.sequence,
            "event": event,
            "observed_unix_milliseconds": now_millis()?,
            "detail": detail
        });
        self.writer.write_all(&canonical_json_lf(&value)?)?;
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        Ok(())
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("t1772 controller failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> ProofResult<()> {
    let args = Args::parse();
    if !cfg!(target_os = "macos") {
        return Err(
            "the accepted P0 lane is Darwin Eezo; controller refuses other operating systems"
                .into(),
        );
    }
    validate_static_contract(&args)?;
    let input_precheck = verify_input_manifest(&args.input_root, &args.manifest)?;
    if args.proof_root.exists() {
        return Err(format!(
            "fresh-root invariant failed: {} exists",
            args.proof_root.display()
        )
        .into());
    }

    validate_source_checkout(&args)?;
    validate_executables(&args)?;

    // Full immutable input custody is observed while PROOF_ROOT is still absent.
    let custody_roots = vec![
        args.input_root.clone(),
        args.tape_root.clone(),
        args.baseline_binary.clone(),
        args.baseline_database.clone(),
    ];
    let immutable_pre = collect_custody(&custody_roots)?;
    let live_pre = observe_live_paths(&args.live_index, &args.cursor_root)?;
    let capacity_before = capacity(&args.proof_root.parent().ok_or("proof root has no parent")?)?;

    fs::create_dir(&args.proof_root)?;
    let disk_sampler = engram::proof::measurement::DiskSampler::start(
        &args.proof_root,
        &args.proof_root.join("staging-disk-samples.jsonl"),
        &args.proof_root.join("staging-disk-peak.json"),
    )?;
    for relative in ["logs", "manifests", "controller", "publication"] {
        fs::create_dir(args.proof_root.join(relative))?;
    }
    write_canonical_jsonl(
        &args.proof_root.join("manifests/input-pre.jsonl"),
        manifest_result(&input_precheck),
    )?;
    write_canonical_jsonl(
        &args
            .proof_root
            .join("manifests/immutable-custody-pre.jsonl"),
        immutable_pre
            .iter()
            .map(|entry| serde_json::to_value(entry).unwrap()),
    )?;
    write_canonical_json(
        &args.proof_root.join("manifests/live-observation-pre.json"),
        &live_pre,
    )?;
    write_canonical_json(
        &args.proof_root.join("manifests/capacity-before.json"),
        &capacity_before,
    )?;

    let controller_root = args.proof_root.join("controller");
    let mut lifecycle = LifecycleWriter::create(&controller_root.join("lifecycle.jsonl"))?;
    lifecycle.record("ordinary_caller_observed", ordinary_caller(&args)?)?;
    lifecycle.record(
        "preflight_passed",
        json!({
            "manifest_sha256": MANIFEST_SHA256,
            "input_entries": input_precheck.len(),
            "proof_root_was_absent": true,
            "candidate_base": CANDIDATE_BASE,
            "source_revision": args.source_revision,
            "capacity": capacity_before
        }),
    )?;

    let stdout_path = args.proof_root.join("logs/runner.stdout");
    let stderr_path = args.proof_root.join("logs/runner.stderr");
    let stdout = File::options()
        .write(true)
        .create_new(true)
        .open(&stdout_path)?;
    let stderr = File::options()
        .write(true)
        .create_new(true)
        .open(&stderr_path)?;
    let argv = runner_argv(&args);
    let started = Instant::now();
    let mut command = Command::new(&args.runner);
    command
        .args(&argv[1..])
        .current_dir(&args.source_root)
        .env_clear()
        .env("T1772_START_SUSPENDED", "1")
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("TZ", "UTC")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let mut child = ManagedChild::new(command.spawn()?);
    let child_pid = child.id() as libc::pid_t;
    lifecycle.record(
        "runner_spawned",
        json!({
            "pid": child_pid,
            "argv": argv,
            "environment": {"T1772_START_SUSPENDED":"1", "LC_ALL":"C", "LANG":"C", "TZ":"UTC"},
            "stdout_path": stdout_path,
            "stderr_path": stderr_path
        }),
    )?;

    let mut wait_status = 0i32;
    let waited = unsafe { libc::waitpid(child_pid, &mut wait_status, libc::WUNTRACED) };
    if waited != child_pid || !libc::WIFSTOPPED(wait_status) {
        return Err(format!("runner did not enter the reviewed post-exec stopped state: waitpid={waited} status={wait_status}").into());
    }
    lifecycle.record(
        "runner_stopped_observed",
        json!({
            "pid": child_pid,
            "stop_signal": libc::WSTOPSIG(wait_status),
            "csops": observe_csops(child_pid)
        }),
    )?;

    if SIGCONT_NUMBER != 19 {
        return Err(
            format!("Darwin SIGCONT must be 19, compiled value is {SIGCONT_NUMBER}").into(),
        );
    }
    let kill_result = unsafe { libc::kill(child_pid, SIGCONT_NUMBER) };
    if kill_result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    lifecycle.record(
        "sigcont_sent",
        json!({
            "pid": child_pid,
            "signal_name":"SIGCONT",
            "signal_number":19,
            "csops_after_resume": observe_csops(child_pid)
        }),
    )?;

    let status = child.wait()?;
    lifecycle.record(
        "runner_completed",
        json!({
            "pid": child_pid,
            "success": status.success(),
            "exit_code": status.code(),
            "elapsed_milliseconds": started.elapsed().as_millis(),
            "stdout_path": stdout_path,
            "stdout_sha256": sha256_file(&stdout_path)?,
            "stderr_path": stderr_path,
            "stderr_sha256": sha256_file(&stderr_path)?
        }),
    )?;
    if !status.success() {
        return Err(format!("runner exited unsuccessfully: {status}").into());
    }

    let summary_path = args.proof_root.join("runner/runner-summary.json");
    let summary: Value = serde_json::from_reader(File::open(&summary_path)?)?;
    if summary["status"] != "passed" || summary["publication_performed"] != false {
        return Err("runner summary is not a passing non-publication result".into());
    }

    let input_postcheck = verify_input_manifest(&args.input_root, &args.manifest)?;
    let immutable_post = collect_custody(&custody_roots)?;
    let custody_comparison = compare_existing_immutable(&immutable_pre, &immutable_post)?;
    let live_post = observe_live_paths(&args.live_index, &args.cursor_root)?;
    let capacity_after = capacity(&args.proof_root)?;
    write_canonical_jsonl(
        &args.proof_root.join("manifests/input-post.jsonl"),
        manifest_result(&input_postcheck),
    )?;
    write_canonical_jsonl(
        &args
            .proof_root
            .join("manifests/immutable-custody-post.jsonl"),
        immutable_post
            .iter()
            .map(|entry| serde_json::to_value(entry).unwrap()),
    )?;
    write_canonical_json(
        &args
            .proof_root
            .join("manifests/immutable-custody-comparison.json"),
        &custody_comparison,
    )?;
    write_canonical_json(
        &args.proof_root.join("manifests/live-observation-post.json"),
        &live_post,
    )?;
    write_canonical_json(
        &args.proof_root.join("manifests/live-observation-diff.json"),
        &diff_values(&live_pre, &live_post),
    )?;
    write_canonical_json(
        &args.proof_root.join("manifests/capacity-after.json"),
        &capacity_after,
    )?;
    lifecycle.record(
        "post_custody_passed",
        json!({
            "manifest_entries": input_postcheck.len(),
            "existing_immutable_paths_unchanged": true,
            "new_immutable_paths": custody_comparison["new_paths"],
            "live_differences_recorded": diff_values(&live_pre, &live_post),
            "capacity": capacity_after
        }),
    )?;

    // Stop and join observation before hashing outputs; sampler failure forbids
    // eligibility. Includes rebuilds, concurrency copies, temporary query copies,
    // sidecars, child output and post-custody capture, all inside PROOF_ROOT.
    let disk = disk_sampler.finish()?;
    lifecycle.record("staging_disk_measurement_completed", disk)?;

    // This local eligibility record never publishes a Tightbeam condition or
    // wakes Eezo; an accepted independent review must precede any external
    // readiness effect. The full output manifest is written once, after it.
    let runner_manifest_hash =
        sha256_file(&args.proof_root.join("runner/runner-output-manifest.jsonl"))?;
    let eligibility = json!({
        "schema":"t1772-publication-eligibility-v1",
        "eligible_for_independent_review":true,
        "eligible_for_readiness_publication":false,
        "reason":"independent review has not yet accepted this exact source, binary custody, invocation, and frozen output manifest",
        "required_order":["proof_passed","post_custody_passed","output_frozen","independent_review_accepted","readiness_published","eezo_launch_authorized"],
        "runner_output_manifest_sha256":runner_manifest_hash,
        "no_external_publication_performed":true,
        "no_live_state_targeted_for_write":true
    });
    lifecycle.record("review_package_eligible", eligibility.clone())?;
    drop(lifecycle);
    write_canonical_json(
        &args.proof_root.join("publication/review-eligibility.json"),
        &eligibility,
    )?;
    let output_manifest = proof_output_manifest(&args.proof_root)?;
    write_canonical_jsonl(
        &args.proof_root.join("manifests/full-proof-output.jsonl"),
        output_manifest,
    )?;
    Ok(())
}

fn validate_static_contract(args: &Args) -> ProofResult<()> {
    require_exact_path(&args.input_root, INPUT_ROOT, "INPUT_ROOT")?;
    require_exact_path(&args.proof_root, PROOF_ROOT, "PROOF_ROOT")?;
    if args.manifest != args.input_root.join(MANIFEST_NAME) {
        return Err("manifest path is not exact".into());
    }
    if args.source_revision != BUILD_REVISION || BUILD_REVISION == "UNSET" {
        return Err(format!(
            "source revision {} does not match embedded build revision {BUILD_REVISION}",
            args.source_revision
        )
        .into());
    }
    if args.runner_sha256.len() != 64
        || args.controller_sha256.len() != 64
        || args.candidate_binary_sha256.len() != 64
    {
        return Err("every executable identity must be a full 64-hex SHA-256".into());
    }
    Ok(())
}

fn validate_source_checkout(args: &Args) -> ProofResult<()> {
    let head = git_output(&args.source_root, &["rev-parse", "HEAD"])?;
    if head != args.source_revision {
        return Err(format!(
            "checkout HEAD {head} differs from source revision {}",
            args.source_revision
        )
        .into());
    }
    let tree = git_output(&args.source_root, &["status", "--porcelain"])?;
    if !tree.is_empty() {
        return Err("source checkout is dirty".into());
    }
    let status = Command::new("git")
        .args([
            "merge-base",
            "--is-ancestor",
            CANDIDATE_BASE,
            &args.source_revision,
        ])
        .current_dir(&args.source_root)
        .status()?;
    if !status.success() {
        return Err("source revision is not descended from the accepted candidate".into());
    }
    Ok(())
}

fn git_output(root: &Path, args: &[&str]) -> ProofResult<String> {
    let output = Command::new("git").args(args).current_dir(root).output()?;
    if !output.status.success() {
        return Err(format!("git {:?} failed", args).into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn validate_executables(args: &Args) -> ProofResult<()> {
    if args.baseline_database.canonicalize()? == args.live_index.canonicalize()? {
        return Err("baseline must be a frozen non-live database, not the live index".into());
    }
    if sha256_file(&args.baseline_database)? != args.baseline_database_sha256
        || fs::metadata(&args.baseline_database)?.len() != t1772::BASELINE_BYTES
    {
        return Err("baseline snapshot custody mismatch".into());
    }
    let current = std::env::current_exe()?;
    for (name, path, expected) in [
        (
            "controller",
            current.as_path(),
            args.controller_sha256.as_str(),
        ),
        ("runner", args.runner.as_path(), args.runner_sha256.as_str()),
        (
            "candidate",
            args.candidate_binary.as_path(),
            args.candidate_binary_sha256.as_str(),
        ),
        (
            "baseline",
            args.baseline_binary.as_path(),
            engram::proof::performance::BASELINE_BINARY_SHA256,
        ),
    ] {
        let observed = sha256_file(path)?;
        if observed != expected {
            return Err(format!("{name} executable SHA-256 mismatch: {observed}").into());
        }
    }
    Ok(())
}

fn runner_argv(args: &Args) -> Vec<String> {
    vec![
        args.runner.to_string_lossy().into_owned(),
        "--candidate-base".into(),
        CANDIDATE_BASE.into(),
        "--input-root".into(),
        args.input_root.to_string_lossy().into_owned(),
        "--proof-root".into(),
        args.proof_root.to_string_lossy().into_owned(),
        "--tape-root".into(),
        args.tape_root.to_string_lossy().into_owned(),
        "--manifest".into(),
        args.manifest.to_string_lossy().into_owned(),
        "--candidate-binary".into(),
        args.candidate_binary.to_string_lossy().into_owned(),
        "--baseline-binary".into(),
        args.baseline_binary.to_string_lossy().into_owned(),
        "--baseline-database".into(),
        args.baseline_database.to_string_lossy().into_owned(),
        "--baseline-database-sha256".into(),
        args.baseline_database_sha256.clone(),
    ]
}

fn ordinary_caller(args: &Args) -> ProofResult<Value> {
    let argv = std::env::args().collect::<Vec<_>>();
    Ok(json!({
        "controller_pid": std::process::id(),
        "caller_pid": unsafe { libc::getppid() },
        "uid": unsafe { libc::getuid() },
        "gid": unsafe { libc::getgid() },
        "cwd": std::env::current_dir()?,
        "argv": argv,
        "source_root": args.source_root,
        "source_revision": args.source_revision,
        "controller_executable": std::env::current_exe()?,
        "controller_sha256": args.controller_sha256,
        "controller_csops": observe_csops(std::process::id() as libc::pid_t),
        "canonical_bytes_contract": t1772::CANONICAL_BYTES_CONTRACT
    }))
}

#[cfg(target_os = "macos")]
fn observe_csops(pid: libc::pid_t) -> Value {
    unsafe extern "C" {
        fn csops(
            pid: libc::pid_t,
            ops: libc::c_uint,
            useraddr: *mut libc::c_void,
            usersize: libc::size_t,
        ) -> libc::c_int;
    }
    let mut flags: u32 = 0;
    let result = unsafe {
        csops(
            pid,
            0,
            (&mut flags as *mut u32).cast(),
            std::mem::size_of::<u32>(),
        )
    };
    let errno = if result == 0 {
        0
    } else {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
    };
    json!({"command":"csops(pid, CS_OPS_STATUS=0, &flags, sizeof(flags))", "pid":pid, "result":result, "errno":errno, "flags":flags, "flags_hex":format!("0x{flags:08x}")})
}

#[cfg(not(target_os = "macos"))]
fn observe_csops(pid: libc::pid_t) -> Value {
    json!({"command":"csops(pid, CS_OPS_STATUS=0, &flags, sizeof(flags))", "pid":pid, "result":"unsupported-on-non-Darwin"})
}

fn observe_live_paths(index: &Path, cursor_root: &Path) -> ProofResult<Value> {
    let mut paths = vec![
        index.to_path_buf(),
        PathBuf::from(format!("{}-wal", index.display())),
        PathBuf::from(format!("{}-shm", index.display())),
    ];
    if cursor_root.exists() {
        collect_paths(cursor_root, &mut paths)?;
    }
    paths.sort();
    paths.dedup();
    let mut rows = Vec::new();
    for path in paths {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => rows.push(json!({"path":path,"state":"present","bytes":metadata.len(),"mtime_seconds":metadata.mtime(),"mtime_nanoseconds":metadata.mtime_nsec(),"inode":metadata.ino(),"device":metadata.dev(),"sha256":sha256_file(&path)?})),
            Ok(metadata) => rows.push(json!({"path":path,"state":"present-non-file","bytes":metadata.len(),"mtime_seconds":metadata.mtime(),"inode":metadata.ino(),"device":metadata.dev()})),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => rows.push(json!({"path":path,"state":"absent"})),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(json!({"access":"read-only observation; never a write target", "rows":rows}))
}

fn collect_paths(root: &Path, paths: &mut Vec<PathBuf>) -> ProofResult<()> {
    paths.push(root.to_path_buf());
    if fs::symlink_metadata(root)?.is_dir() {
        let mut children = fs::read_dir(root)?
            .map(|entry| entry.map(|item| item.path()))
            .collect::<Result<Vec<_>, _>>()?;
        children.sort();
        for child in children {
            collect_paths(&child, paths)?;
        }
    }
    Ok(())
}

fn compare_existing_immutable(
    before: &[t1772::CustodyEntry],
    after: &[t1772::CustodyEntry],
) -> ProofResult<Value> {
    let after_map = after
        .iter()
        .map(|entry| (&entry.path, entry))
        .collect::<BTreeMap<_, _>>();
    for entry in before {
        let Some(observed) = after_map.get(&entry.path) else {
            return Err(format!("immutable input disappeared: {}", entry.path).into());
        };
        if entry.file_type == "file" || entry.file_type == "symlink" {
            if entry.file_type != observed.file_type
                || entry.bytes != observed.bytes
                || entry.sha256 != observed.sha256
                || entry.symlink_target != observed.symlink_target
            {
                return Err(format!("immutable input bytes changed: {}", entry.path).into());
            }
        }
    }
    let before_paths = before
        .iter()
        .map(|entry| &entry.path)
        .collect::<BTreeSet<_>>();
    let new_paths = after
        .iter()
        .filter(|entry| !before_paths.contains(&entry.path))
        .map(|entry| &entry.path)
        .collect::<Vec<_>>();
    Ok(
        json!({"existing_paths_unchanged":true,"before_count":before.len(),"after_count":after.len(),"new_paths":new_paths}),
    )
}

fn diff_values(before: &Value, after: &Value) -> Value {
    json!({"changed":before != after,"before":before,"after":after,"attribution":"differences are observed only; the controller never opens these paths for write"})
}

fn capacity(path: &Path) -> ProofResult<Value> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(c_path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stats = unsafe { stats.assume_init() };
    let available = (stats.f_bavail as u128)
        .checked_mul(stats.f_frsize as u128)
        .ok_or("capacity overflow")?;
    Ok(
        json!({"path":path,"available_bytes":available,"unit":"bytes","source":"statvfs.f_bavail * statvfs.f_frsize"}),
    )
}

fn proof_output_manifest(root: &Path) -> ProofResult<Vec<Value>> {
    let mut paths = Vec::new();
    collect_paths(root, &mut paths)?;
    paths.sort();
    let manifest_path = root.join("manifests/full-proof-output.jsonl");
    let mut values = Vec::new();
    for path in paths {
        if !path.is_file() || path == manifest_path {
            continue;
        }
        values.push(json!({"path":path.strip_prefix(root)?.to_string_lossy(),"bytes":fs::metadata(&path)?.len(),"sha256":sha256_file(&path)?}));
    }
    Ok(values)
}

fn now_millis() -> ProofResult<u128> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
}
