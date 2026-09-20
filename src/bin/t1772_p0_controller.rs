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
    oracle_root: PathBuf,
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
    #[arg(long)]
    comparator_receipt: PathBuf,
    #[arg(long)]
    comparator_receipt_sha256: String,
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
    let comparator = engram::proof::baseline_custody::verify_comparator(
        &args.baseline_database,
        &args.baseline_database_sha256,
        &args.comparator_receipt,
        &args.comparator_receipt_sha256,
        &args.input_root,
        &args.tape_root,
    )?;

    // Full immutable input custody is observed while PROOF_ROOT is still absent.
    let custody_roots = vec![
        args.input_root.clone(),
        args.oracle_root.clone(),
        args.tape_root.clone(),
        args.baseline_binary.clone(),
        PathBuf::from(engram::proof::baseline_custody::COMPARATOR_ROOT),
    ];
    let immutable_pre = collect_custody(&custody_roots)?;
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
    write_canonical_json(
        &args
            .proof_root
            .join("manifests/reconstructed-comparator.json"),
        &comparator,
    )?;
    write_canonical_json(
        &args.proof_root.join("manifests/oracle-inputs-pre.json"),
        &engram::proof::canonical_oracle::verify_inputs(&args.oracle_root)?,
    )?;
    // Immutable/input checks still precede root creation. Live CoW captures need
    // their staging directory and complete before any runner staging read.
    let live_pre = observe_live_paths(
        &args.live_index,
        &args.cursor_root,
        &args.proof_root.join("manifests/live-clones-pre"),
    )?;
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
    let runner_result = (|| -> ProofResult<()> {
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

        Ok(())
    })();
    // Reap/terminate a child even after a stop/resume/observation error, then
    // attempt every post boundary independently of the primary runner result.
    drop(child);
    let disk = retain_after_runner(
        &args,
        &custody_roots,
        &immutable_pre,
        &live_pre,
        runner_result,
        disk_sampler,
    )?;
    lifecycle.record(
        "post_custody_passed",
        json!({"all_observations_retained":true}),
    )?;
    lifecycle.record("staging_disk_measurement_completed", disk)?;
    lifecycle.record(
        "payload_finalization_started",
        json!({"eligibility_receipt_outside_payload":true}),
    )?;
    drop(lifecycle); // No lifecycle writes after its bytes enter the payload.
    finalize_eligibility(&args.proof_root)?;
    Ok(())
}

fn retain_after_runner(
    args: &Args,
    custody_roots: &[PathBuf],
    immutable_pre: &[t1772::CustodyEntry],
    live_pre: &Value,
    runner_result: ProofResult<()>,
    disk_sampler: engram::proof::measurement::DiskSampler,
) -> ProofResult<Value> {
    let post_result = retain_post_custody(args, custody_roots, immutable_pre, live_pre);
    let disk_result = disk_sampler.finish();
    let primary = runner_result.as_ref().err().map(ToString::to_string);
    let outcome = json!({"runner_error":primary,
        "post_custody_error":post_result.as_ref().err().map(ToString::to_string),
        "disk_error":disk_result.as_ref().err().map(ToString::to_string),
        "eligible_for_independent_review":false});
    let outcome_result = write_canonical_json(
        &args.proof_root.join("controller/post-run-outcome.json"),
        &outcome,
    );
    // Preserve the primary failure; secondary capture errors are retained above.
    runner_result?;
    post_result?;
    let disk = disk_result?;
    outcome_result?;
    Ok(disk)
}

// The final receipt is outside the payload it binds; neither manifest nor
// eligibility exists as a positive completion signal before payload persistence.
fn finalize_eligibility(root: &Path) -> ProofResult<()> {
    let runner_hash = sha256_file(&root.join("runner/runner-output-manifest.jsonl"))?;
    let output_manifest = proof_output_manifest(root)?;
    let payload_hash = write_canonical_jsonl(
        &root.join("manifests/full-proof-output.jsonl"),
        output_manifest,
    )?;
    write_canonical_json(
        &root.join("publication/review-eligibility.json"),
        &json!({
            "schema":"t1772-publication-eligibility-v2",
            "eligible_for_independent_review":true,
            "eligible_for_readiness_publication":false,
            "reason":"independent review and exact authorized native execution custody remain required",
            "required_order":["proof_passed","post_custody_passed","output_frozen","independent_review_accepted","readiness_published","eezo_launch_authorized"],
            "runner_output_manifest_sha256":runner_hash,
            "full_proof_output_manifest_sha256":payload_hash,
            "payload_exclusions":["manifests/full-proof-output.jsonl","publication/review-eligibility.json"],
            "cold_custody_amendment_sha256":engram::proof::baseline_custody::COLD_AMENDMENT_SHA256,
            "cold_copy_limitation":engram::proof::baseline_custody::COLD_LIMITATION,
            "no_external_publication_performed":true,"no_live_state_targeted_for_write":true
        }),
    )?;
    Ok(())
}

fn retain_post_custody(
    args: &Args,
    roots: &[PathBuf],
    before: &[t1772::CustodyEntry],
    live_pre: &Value,
) -> ProofResult<()> {
    // Only this post boundary collects errors instead of returning on first failure.
    fn save(
        root: &Path,
        name: &str,
        result: ProofResult<Value>,
        errors: &mut Vec<Value>,
    ) -> Option<Value> {
        match result {
            Ok(value) => {
                if let Err(error) = write_canonical_json(&root.join(name), &value) {
                    errors.push(json!({"observation":name,"error":error.to_string()}));
                }
                Some(value)
            }
            Err(error) => {
                errors.push(json!({"observation":name,"error":error.to_string()}));
                None
            }
        }
    }
    let root = &args.proof_root;
    let (after, mut errors) = t1772::collect_post_custody(roots);
    // Persist all available entries before comparing, including mutations.
    if let Err(error) = write_canonical_jsonl(
        &root.join("manifests/immutable-custody-post.jsonl"),
        after
            .iter()
            .map(|entry| serde_json::to_value(entry).expect("custody serializes")),
    ) {
        errors.push(json!({"observation":"immutable-custody-post","error":error.to_string()}));
    }
    let comparison = compare_existing_immutable(before, &after);
    let equal = comparison["existing_paths_unchanged"] == true;
    save(
        root,
        "manifests/immutable-custody-comparison.json",
        Ok(comparison),
        &mut errors,
    );
    save(
        root,
        "manifests/input-post.json",
        verify_input_manifest(&args.input_root, &args.manifest)
            .map(|entries| json!(manifest_result(&entries))),
        &mut errors,
    );
    save(
        root,
        "manifests/oracle-inputs-post.json",
        engram::proof::canonical_oracle::verify_inputs(&args.oracle_root),
        &mut errors,
    );
    if let Some(live) = save(
        root,
        "manifests/live-observation-post.json",
        observe_live_paths(
            &args.live_index,
            &args.cursor_root,
            &root.join("manifests/live-clones-post"),
        ),
        &mut errors,
    ) {
        save(
            root,
            "manifests/live-observation-diff.json",
            Ok(diff_values(live_pre, &live)),
            &mut errors,
        );
    }
    save(
        root,
        "manifests/capacity-after.json",
        capacity(root),
        &mut errors,
    );
    write_canonical_json(
        &root.join("manifests/post-observation-errors.json"),
        &json!({"errors":errors}),
    )?;
    if !equal || !errors.is_empty() {
        return Err("post-custody mismatch or observation error; retained manifests describe available evidence".into());
    }
    Ok(())
}

fn validate_static_contract(args: &Args) -> ProofResult<()> {
    require_exact_path(&args.input_root, INPUT_ROOT, "INPUT_ROOT")?;
    require_exact_path(
        &args.oracle_root,
        engram::proof::canonical_oracle::SUPPLEMENT_ROOT,
        "ORACLE_ROOT",
    )?;
    engram::proof::canonical_oracle::verify_inputs(&args.oracle_root)?;
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
    require_exact_path(
        &args.baseline_binary,
        engram::proof::performance::BASELINE_BINARY_PATH,
        "baseline binary",
    )?;
    if args.baseline_database.canonicalize()? == args.live_index.canonicalize()? {
        return Err("baseline must be a frozen non-live database, not the live index".into());
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
        "--oracle-root".into(),
        args.oracle_root.to_string_lossy().into_owned(),
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
        "--comparator-receipt".into(),
        args.comparator_receipt.to_string_lossy().into_owned(),
        "--comparator-receipt-sha256".into(),
        args.comparator_receipt_sha256.clone(),
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

fn observe_live_paths(index: &Path, cursor_root: &Path, clone_root: &Path) -> ProofResult<Value> {
    let boundary_started = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let mut rows = clone_live_index_files(index, clone_root)?;
    let mut paths = vec![cursor_root.to_path_buf()];
    if cursor_root.exists() {
        collect_paths(cursor_root, &mut paths)?;
    }
    paths.sort();
    paths.dedup();
    for path in paths {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => rows.push(json!({"path":path,"state":"present","type":"file","bytes":metadata.len(),"mtime_seconds":metadata.mtime(),"mtime_nanoseconds":metadata.mtime_nsec(),"inode":metadata.ino(),"device":metadata.dev(),"sha256":sha256_file(&path)?})),
            Ok(metadata) => rows.push(json!({"path":path,"state":"present-non-file","type":if metadata.is_dir(){"directory"}else if metadata.file_type().is_symlink(){"symlink"}else{"other"},"bytes":metadata.len(),"mtime_seconds":metadata.mtime(),"mtime_nanoseconds":metadata.mtime_nsec(),"inode":metadata.ino(),"device":metadata.dev()})),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => rows.push(json!({"path":path,"state":"absent"})),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(
        json!({"access":"live paths read-only; writes only to retained APFS staging clones",
        "boundary_started_unix_nanoseconds":boundary_started,
        "boundary_finished_unix_nanoseconds":SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
        "index_hash_scope":"retained per-file APFS CoW clones; no multi-file atomic SQLite snapshot claimed",
        "clone_root":clone_root,"rows":rows}),
    )
}

#[cfg(target_os = "macos")]
fn clone_live_index_files(index: &Path, clone_root: &Path) -> ProofResult<Vec<Value>> {
    use std::ffi::{CStr, CString};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    fn require_apfs(file: &File) -> ProofResult<()> {
        let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
        if unsafe { libc::fstatfs(file.as_raw_fd(), info.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let info = unsafe { info.assume_init() };
        if unsafe { CStr::from_ptr(info.f_fstypename.as_ptr()) }.to_bytes() != b"apfs" {
            return Err("live manifest clones require APFS; no copy fallback permitted".into());
        }
        Ok(())
    }
    let metadata_json = |m: &fs::Metadata| {
        json!({"type":if m.is_file(){"file"}else if m.is_dir(){"directory"}else if m.file_type().is_symlink(){"symlink"}else{"other"},"bytes":m.len(),
        "mtime_seconds":m.mtime(),"mtime_nanoseconds":m.mtime_nsec(),
        "inode":m.ino(),"device":m.dev(),"mode":m.mode()})
    };
    fs::create_dir(clone_root)?;
    let directory = File::open(clone_root)?;
    require_apfs(&directory)?;
    let mut rows = Vec::new();
    let mut clones = Vec::new();
    // Capture all three files before hashing any, keeping capture times close.
    // The read-only descriptor pins each source inode across pathname rotation.
    for (source_path, name) in [
        (index.to_path_buf(), "index.sqlite"),
        (
            PathBuf::from(format!("{}-wal", index.display())),
            "index.sqlite-wal",
        ),
        (
            PathBuf::from(format!("{}-shm", index.display())),
            "index.sqlite-shm",
        ),
    ] {
        let started = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let source = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&source_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                rows.push(json!({"path":source_path,"state":"absent","observed_unix_nanoseconds":started}));
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let before = source.metadata()?;
        if !before.is_file() || before.dev() != directory.metadata()?.dev() {
            return Err(
                "live manifest clone source must be a regular file on the staging filesystem"
                    .into(),
            );
        }
        require_apfs(&source)?;
        let destination = CString::new(name)?;
        if unsafe {
            libc::fclonefileat(
                source.as_raw_fd(),
                directory.as_raw_fd(),
                destination.as_ptr(),
                0,
            )
        } != 0
        {
            return Err(format!(
                "APFS clone failed for {}: {}",
                source_path.display(),
                std::io::Error::last_os_error()
            )
            .into());
        }
        let finished = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let clone_path = clone_root.join(name);
        fs::set_permissions(&clone_path, fs::Permissions::from_mode(0o400))?;
        let mut row = metadata_json(&before);
        row["path"] = json!(source_path);
        row["state"] = json!("present");
        row["clone_method"] =
            json!("APFS fclonefileat, read-only source descriptor, flags=0; no byte-copy fallback");
        row["clone_started_unix_nanoseconds"] = json!(started);
        row["clone_finished_unix_nanoseconds"] = json!(finished);
        row["source_metadata_after_clone"] = metadata_json(&source.metadata()?);
        row["source_path_after_clone"] = match fs::symlink_metadata(&source_path) {
            Ok(m) => metadata_json(&m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({"state":"absent"}),
            Err(e) => return Err(e.into()),
        };
        row["clone_path"] = json!(clone_path);
        row["hash_scope"] = json!("retained staging clone, not changing live path");
        clones.push((row, clone_path));
    }
    for (mut row, clone_path) in clones {
        row["clone_metadata"] = metadata_json(&fs::metadata(&clone_path)?);
        row["sha256"] = json!(sha256_file(&clone_path)?);
        rows.push(row);
    }
    Ok(rows)
}

#[cfg(not(target_os = "macos"))]
fn clone_live_index_files(_index: &Path, _clone_root: &Path) -> ProofResult<Vec<Value>> {
    Err("APFS live manifest cloning is supported only on Darwin; no live hashing fallback".into())
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
) -> Value {
    let after_map = after
        .iter()
        .map(|entry| (&entry.path, entry))
        .collect::<BTreeMap<_, _>>();
    let mut differences = Vec::new();
    for entry in before {
        match after_map.get(&entry.path) {
            None => differences.push(json!({"path":entry.path,"before":entry,"after":null})),
            Some(observed)
                if (entry.file_type == "file" || entry.file_type == "symlink")
                    && (entry.file_type != observed.file_type
                        || entry.bytes != observed.bytes
                        || entry.sha256 != observed.sha256
                        || entry.symlink_target != observed.symlink_target) =>
            {
                differences.push(json!({"path":entry.path,"before":entry,"after":observed}))
            }
            _ => {}
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
    json!({"existing_paths_unchanged":differences.is_empty(),"differences":differences,
        "before_count":before.len(),"after_count":after.len(),"new_paths":new_paths})
}

fn diff_values(before: &Value, after: &Value) -> Value {
    // Clone paths and observation clocks necessarily differ between boundaries.
    // Compare source identity/metadata and captured bytes, retain complete rows.
    let comparable = |value: &Value| {
        value["rows"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|row| {
                let fields = [
                    "path",
                    "state",
                    "type",
                    "bytes",
                    "mtime_seconds",
                    "mtime_nanoseconds",
                    "inode",
                    "device",
                    "sha256",
                    "source_metadata_after_clone",
                    "source_path_after_clone",
                ];
                let entry = fields
                    .into_iter()
                    .map(|key| (key.to_string(), row[key].clone()))
                    .collect::<serde_json::Map<_, _>>();
                (
                    row["path"].as_str().unwrap_or_default().to_string(),
                    Value::Object(entry),
                )
            })
            .collect::<BTreeMap<_, _>>()
    };
    json!({"changed":comparable(before) != comparable(after),"before":before,"after":after,
        "comparison_scope":"source identity/metadata and clone byte hashes; observation clocks/clone destinations excluded",
        "attribution":"differences are observed only; the controller never opens live paths for write; per-file clones are not a multi-file atomic database snapshot"})
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
        if !path.is_file()
            || path == manifest_path
            || path == root.join("publication/review-eligibility.json")
        {
            continue;
        }
        values.push(json!({"path":path.strip_prefix(root)?.to_string_lossy(),"bytes":fs::metadata(&path)?.len(),"sha256":sha256_file(&path)?}));
    }
    Ok(values)
}

fn now_millis() -> ProofResult<u128> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
}

#[cfg(test)]
mod live_clone_tests {
    use super::*;

    fn post_fixture(root: &Path) -> Args {
        Args {
            source_root: root.into(),
            source_revision: "fixture".into(),
            input_root: root.join("inputs"),
            oracle_root: root.join("oracle-inputs"),
            proof_root: root.join("proof"),
            tape_root: root.join("tapes"),
            live_index: root.join("live.sqlite"),
            cursor_root: root.join("cursors"),
            manifest: root.join("inputs/manifest"),
            runner: root.join("runner"),
            runner_sha256: "0".repeat(64),
            controller_sha256: "0".repeat(64),
            candidate_binary: root.join("candidate"),
            candidate_binary_sha256: "0".repeat(64),
            baseline_binary: root.join("baseline"),
            baseline_database: root.join("baseline.sqlite"),
            baseline_database_sha256: "0".repeat(64),
            comparator_receipt: root.join("receipt.json"),
            comparator_receipt_sha256: "0".repeat(64),
        }
    }

    #[test]
    fn failed_runner_and_immutable_mismatch_preserve_post_evidence() {
        for runner_failed in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let args = post_fixture(temp.path());
            fs::create_dir(&args.proof_root).unwrap();
            fs::create_dir(&args.input_root).unwrap();
            let input = args.input_root.join("blob");
            fs::write(&input, b"before").unwrap();
            let roots = vec![args.input_root.clone()];
            let pre = collect_custody(&roots).unwrap();
            fs::write(&input, b"changed").unwrap();
            let sampler = engram::proof::measurement::DiskSampler::start(
                &args.proof_root,
                &args.proof_root.join("samples.jsonl"),
                &args.proof_root.join("disk.json"),
            )
            .unwrap();
            let primary = if runner_failed {
                Err("runner failed fixture".into())
            } else {
                Ok(())
            };
            let error = retain_after_runner(&args, &roots, &pre, &json!({}), primary, sampler)
                .unwrap_err()
                .to_string();
            if runner_failed {
                assert_eq!(error, "runner failed fixture");
            } else {
                assert!(error.contains("post-custody"));
            }
            let post = fs::read_to_string(
                args.proof_root
                    .join("manifests/immutable-custody-post.jsonl"),
            )
            .unwrap();
            assert!(post.contains(&sha256_file(&input).unwrap()));
            let comparison: Value = serde_json::from_reader(
                File::open(
                    args.proof_root
                        .join("manifests/immutable-custody-comparison.json"),
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(comparison["existing_paths_unchanged"], false);
            assert_eq!(comparison["differences"].as_array().unwrap().len(), 1);
            assert!(
                args.proof_root
                    .join("manifests/post-observation-errors.json")
                    .is_file()
            );
            assert!(
                args.proof_root
                    .join("manifests/capacity-after.json")
                    .is_file()
            );
            assert!(
                !args
                    .proof_root
                    .join("publication/review-eligibility.json")
                    .exists()
            );
        }
    }

    #[test]
    fn eligibility_requires_persisted_payload_and_binds_its_hash() {
        for fail in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            fs::create_dir(root.join("runner")).unwrap();
            fs::create_dir(root.join("manifests")).unwrap();
            fs::write(root.join("runner/runner-output-manifest.jsonl"), b"{}\n").unwrap();
            if fail {
                fs::create_dir(root.join("manifests/full-proof-output.jsonl")).unwrap();
            }
            let result = finalize_eligibility(root);
            if fail {
                assert!(result.is_err());
                assert!(!root.join("publication/review-eligibility.json").exists());
            } else {
                result.unwrap();
                let value: Value = serde_json::from_reader(
                    File::open(root.join("publication/review-eligibility.json")).unwrap(),
                )
                .unwrap();
                assert_eq!(
                    value["full_proof_output_manifest_sha256"],
                    sha256_file(&root.join("manifests/full-proof-output.jsonl")).unwrap()
                );
                let payload =
                    fs::read_to_string(root.join("manifests/full-proof-output.jsonl")).unwrap();
                assert!(!payload.contains("review-eligibility.json"));
            }
        }
    }

    #[test]
    fn live_difference_ignores_capture_locations_but_keeps_byte_changes() {
        let before = json!({"rows":[{"path":"/live/index.sqlite","state":"present",
            "sha256":"a","clone_path":"/stage/pre/index.sqlite","clone_started_unix_nanoseconds":1}]});
        let mut after = before.clone();
        after["rows"][0]["clone_path"] = json!("/stage/post/index.sqlite");
        after["rows"][0]["clone_started_unix_nanoseconds"] = json!(2);
        assert_eq!(diff_values(&before, &after)["changed"], false);
        after["rows"][0]["sha256"] = json!("b");
        assert_eq!(diff_values(&before, &after)["changed"], true);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn apfs_boundary_clones_survive_source_changes() {
        let staging = tempfile::tempdir().unwrap();
        let index = staging.path().join("source.sqlite");
        let wal = staging.path().join("source.sqlite-wal");
        fs::write(&index, b"original-index").unwrap();
        fs::write(&wal, b"original-wal").unwrap();
        let clone_root = staging.path().join("clones");
        let rows = clone_live_index_files(&index, &clone_root).unwrap();
        assert_eq!(rows.iter().filter(|r| r["state"] == "absent").count(), 1);
        fs::write(&index, b"changed-index").unwrap();
        fs::remove_file(&wal).unwrap();
        assert_eq!(
            fs::read(clone_root.join("index.sqlite")).unwrap(),
            b"original-index"
        );
        assert_eq!(
            fs::read(clone_root.join("index.sqlite-wal")).unwrap(),
            b"original-wal"
        );
        for row in rows.iter().filter(|r| r["state"] == "present") {
            assert_eq!(
                row["sha256"],
                sha256_file(Path::new(row["clone_path"].as_str().unwrap())).unwrap()
            );
        }
    }
}
