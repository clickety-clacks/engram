//! Frozen-manifest ordinary CLI measurements. Missing observations fail closed.
use super::baseline_custody;
use super::measurement::{DiskSampler, percentiles};
use super::t1772::{ProofResult, sha256_file, write_canonical_json, write_new_file};
use serde_json::{Value, json};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

pub fn direct_projection(touches: &[crate::index::lineage::EvidenceFragmentRef]) -> Value {
    let mut rows = touches.iter().map(|touch| json!({
        "event_offset":touch.event_offset,"file_path":touch.file_path,
        "kind":match touch.kind { crate::index::lineage::EvidenceKind::Read=>"read", crate::index::lineage::EvidenceKind::Edit=>"edit" },
        "tape_id":touch.tape_id,"timestamp":touch.timestamp})).collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        a["timestamp"]
            .as_str()
            .cmp(&b["timestamp"].as_str())
            .then_with(|| a["tape_id"].as_str().cmp(&b["tape_id"].as_str()))
            .then_with(|| a["event_offset"].as_i64().cmp(&b["event_offset"].as_i64()))
            .then_with(|| a["kind"].as_str().cmp(&b["kind"].as_str()))
            .then_with(|| a["file_path"].as_str().cmp(&b["file_path"].as_str()))
    });
    Value::Array(rows)
}

pub const BASELINE_BINARY_SHA256: &str =
    "13088f949fa7920615ff8873c1040d4b7ec9180976a7121a63e0de726e47571d";
pub const BASELINE_BINARY_PATH: &str =
    "/Users/mike/src/worktrees/engram-t1772-index-repair/scratch/t1772/engram-baseline-7282151";
pub const BASELINE_EXECUTABLE_CUSTODY_SHA256: &str =
    "4178f5a1d9d3cfdda476708a3548985bdf850aefcb053715dcb04963784aa329";
pub const TELEMETRY_AMENDMENT_SHA256: &str =
    "376d581e12743643c5e5e4923355a530964e485d16437bd47298fb8ee23e9035";
const TEMP_LIMITATION: &str = "named files in dedicated SQLite temp directory, sampled every 100ms; unlinked files and allocations created/deleted between samples may be missed; zero observed is not zero total allocation";

pub struct Inputs<'a> {
    pub baseline_binary: &'a Path,
    pub baseline_database: &'a Path,
    pub baseline_database_sha256: &'a str,
    pub comparator_receipt: &'a Value,
    pub comparator_receipt_sha256: &'a str,
    pub candidate_binary: &'a Path,
    pub candidate_database: &'a Path,
    pub tape_root: &'a Path,
    pub manifest: &'a Path,
}

pub fn run(inputs: &Inputs<'_>, root: &Path) -> ProofResult<()> {
    if !cfg!(target_os = "macos") {
        return Err("performance RSS collector requires Darwin /usr/bin/time -l".into());
    }
    super::t1772::require_exact_path(
        inputs.baseline_binary,
        BASELINE_BINARY_PATH,
        "baseline binary",
    )?;
    if sha256_file(inputs.baseline_binary)? != BASELINE_BINARY_SHA256
        || sha256_file(inputs.baseline_database)? != inputs.baseline_database_sha256
        || Some(fs::metadata(inputs.baseline_database)?.len())
            != inputs.comparator_receipt["database_bytes"].as_u64()
    {
        return Err("baseline binary/database custody mismatch".into());
    }
    let manifest: Value = serde_json::from_reader(File::open(inputs.manifest)?)?;
    validate_manifest(&manifest)?;
    let expected_path = inputs
        .manifest
        .parent()
        .ok_or("manifest parent missing")?
        .join("p0-performance-expected-direct-touches.json");
    if sha256_file(&expected_path)?
        != "417d3aeabaf80a2196b51c3f0b7dad5381903a06d4017b7960410203100d3b31"
    {
        return Err("frozen direct-touch oracle custody mismatch".into());
    }
    let expected: Value = serde_json::from_reader(File::open(&expected_path)?)?;
    let manifest_hash = sha256_file(inputs.manifest)?;
    let source_revision =
        option_env!("T1772_BUILD_REVISION").ok_or("proof build source revision absent")?;
    let probe_binary_hash = sha256_file(&std::env::current_exe()?)?;
    fs::create_dir_all(root)?;
    let binaries = [inputs.baseline_binary, inputs.candidate_binary];
    let binary_hashes = [sha256_file(binaries[0])?, sha256_file(binaries[1])?];
    let databases = [inputs.baseline_database, inputs.candidate_database];
    let database_hashes = [
        inputs.baseline_database_sha256.to_string(),
        sha256_file(databases[1])?,
    ];
    write_canonical_json(
        &root.join("protocol-r5.json"),
        &json!({
        "version":5,"parent_manifest_sha256":manifest_hash,
        "comparison_label":baseline_custody::COMPARISON_LABEL,
        "reconstruction_amendment_sha256":baseline_custody::RECONSTRUCTION_AMENDMENT_SHA256,
        "comparator_receipt_sha256":inputs.comparator_receipt_sha256,
        "comparator":inputs.comparator_receipt,
        "parent_spec_sha256":"df43b6e63184d2e02803905b841dc46f5b6f3d0573c3b5a6ee68ca7d0ea81bea",
        "expected_projection_sha256":"417d3aeabaf80a2196b51c3f0b7dad5381903a06d4017b7960410203100d3b31",
        "amendment_sha256":baseline_custody::AMENDMENT_SHA256,
        "telemetry_amendment_sha256":TELEMETRY_AMENDMENT_SHA256,
        "source_revision":source_revision,
        "baseline_binary_sha256":BASELINE_BINARY_SHA256,
        "baseline_binary_path":BASELINE_BINARY_PATH,
        "baseline_executable_custody":{"artifact":"art_ebfcc161","sha256":BASELINE_EXECUTABLE_CUSTODY_SHA256},
        "preparation_order":"prepare/hash all 396 baseline cold clones before any timed CLI; hot baseline pre/post custody once per series; candidate pre/post custody each complete CLI/probe slot; cold baseline post-custody after entire timing matrix",
        "baseline_policy":"one writable CoW clone per hot series and one per cold slot; full pre/post file hashes retained; byte-equal initial copies inherit verified master logical state; each completed copy receives full post logical validation; only query_results may change",
        "candidate_policy":"database and its directory read-only; exact hash and directory custody after every slot",
        "cache_limit":"all cold baseline clone/hash preparation precedes timing; baseline post scans occur at hot-series boundaries or after the entire cold matrix; candidate slot custody and direct probes still warm cache; fresh CoW copy is not OS cache eviction and may share cached source pages",
        "baseline_cli_counters":"unavailable / non-comparable; no counter improvement comparison permitted",
        "baseline_projection":"raw reconstructed comparator output retained; legacy CLI semantics; separate direct projection unavailable; no result-equivalence claim",
        "candidate_counter_requirement":"each bound direct-touch SQL probe statement SORT=0 and AUTOINDEX=0",
        "temporary_byte_requirement":"candidate observed bytes=0; incomplete coverage accepted under telemetry amendment",
        "temporary_byte_limitation":TEMP_LIMITATION}),
    )?;
    write_io_plan(inputs, root)?;
    // Full controller preflight verified this accounting against the immutable
    // master. A full file hash of each fresh clone establishes its initial state.
    let master_accounting = &inputs.comparator_receipt["accounting"];
    write_canonical_json(
        &root.join("baseline-master-accounting.json"),
        master_accounting,
    )?;
    let mut deferred_baseline: Vec<(PathBuf, PathBuf)> = Vec::new();
    for query in manifest["queries"].as_array().ok_or("queries absent")? {
        let id = query["id"].as_str().ok_or("query ID absent")?;
        super::t1772::safe_relative_path(id)?;
        let query_root = root.join(id).join("filesystem-cold");
        fs::create_dir_all(&query_root)?;
        for iteration in 0..33 {
            let slot = query_root.join(format!("{iteration:02}-baseline"));
            fs::create_dir(&slot)?;
            let db = slot.join("copy/database.sqlite");
            prepare_baseline(inputs, &db, &slot)?;
            deferred_baseline.push((db, slot));
        }
    }
    let mut comparisons = Vec::new();
    let mut all_passed = true;
    for (query_index, query) in manifest["queries"]
        .as_array()
        .ok_or("queries absent")?
        .iter()
        .enumerate()
    {
        let id = query["id"].as_str().ok_or("id absent")?;
        let expected_touches = &expected["queries"]
            .as_array()
            .ok_or("oracle queries missing")?
            .iter()
            .find(|q| q["id"] == id)
            .ok_or("oracle query missing")?["touches"];
        if !expected_touches.is_array() {
            return Err("oracle touches missing".into());
        }
        super::t1772::safe_relative_path(id)?;
        for mode in ["hot", "filesystem-cold"] {
            let query_root = root.join(id).join(mode);
            fs::create_dir_all(&query_root)?;
            let hot = [
                query_root.join("baseline-copy/database.sqlite"),
                query_root.join("candidate-copy/database.sqlite"),
            ];
            if mode == "hot" {
                prepare_baseline(inputs, &hot[0], &query_root)?;
                let started = Instant::now();
                staging_copy(databases[1], &hot[1], false)?;
                write_canonical_json(
                    &query_root.join("candidate-preparation.json"),
                    &json!({"elapsed_microseconds":started.elapsed().as_micros(),
                        "copy":hot[1],"hash_check":"first slot pre-custody before launch"}),
                )?;
            }
            let mut measured: [Vec<Value>; 2] = [Vec::new(), Vec::new()];
            for iteration in 0..33 {
                let warmup = iteration < 3;
                for (order, variant) in alternating_order(query_index, iteration)
                    .into_iter()
                    .enumerate()
                {
                    let label = ["baseline", "candidate"][variant];
                    let run_root = query_root.join(format!("{iteration:02}-{label}"));
                    if mode != "filesystem-cold" || variant != 0 {
                        fs::create_dir(&run_root)?;
                    }
                    let prepare_start = Instant::now();
                    let cold = run_root.join("copy/database.sqlite");
                    let db = if mode == "filesystem-cold" {
                        if variant == 1 {
                            staging_copy(databases[1], &cold, false)?;
                        }
                        &cold
                    } else {
                        &hot[variant]
                    };
                    let initial_custody_path = if variant == 0 && mode == "hot" {
                        query_root.join("custody-before.json")
                    } else {
                        run_root.join("custody-before.json")
                    };
                    let before = if variant == 1 {
                        let files = baseline_custody::copy_files(db)?;
                        if database_hash(&files, db)? != database_hashes[1] {
                            return Err("candidate pre-slot master hash mismatch".into());
                        }
                        write_canonical_json(&initial_custody_path, &files)?;
                        write_canonical_json(
                            &run_root.join("preparation.json"),
                            &json!({
                            "elapsed_microseconds":prepare_start.elapsed().as_micros(),
                            "copy_bytes":fs::metadata(db)?.len(),"master_sha256":database_hashes[1],
                            "preparation_excluded_from_cli_timing":true}),
                        )?;
                        files
                    } else {
                        Value::Null
                    };
                    let target = query["target"].as_str().ok_or("target absent")?;
                    let anchors =
                        crate::query::format::derive_anchor_candidates(&[target.to_string()]);
                    let binding = json!({"query_id":id,"variant":label,"cache_class":mode,
                        "comparison_label":baseline_custody::COMPARISON_LABEL,
                        "reconstruction_amendment_sha256":baseline_custody::RECONSTRUCTION_AMENDMENT_SHA256,
                        "comparator_receipt_sha256":inputs.comparator_receipt_sha256,
                        "phase":if warmup {"warmup"} else {"measured"},
                        "iteration":if warmup {iteration} else {iteration - 3},
                        "schedule_iteration":iteration,"order_in_pair":order,
                        "database_path":db,"master_database_sha256":database_hashes[variant],
                        "database_sha256":if variant == 0 && mode == "hot" {Value::Null}else{json!(database_hashes[variant])},
                        "initial_custody_path":initial_custody_path,
                        "database_identity_scope":if variant == 0 && mode == "hot" {"series initial hash; mutable intermediate slot hash not observed"}else{"fresh or read-only copy pre-slot hash"},
                        "post_custody_path":if variant == 0 {if mode == "hot" {query_root.join("custody-after.json")}else{run_root.join("custody-after.json")}}else{run_root.join("custody-after-slot.json")},
                        "amendment_sha256":baseline_custody::AMENDMENT_SHA256,
                        "telemetry_amendment_sha256":TELEMETRY_AMENDMENT_SHA256,
                        "manifest_sha256":manifest_hash,"target":target,"flags":query["flags"],
                        "derived_anchors":anchors,"product_binary_sha256":binary_hashes[variant],
                        "product_source_revision":if variant == 0 {"72821518037a9d896f0b4d784fee146800902e78"}else{source_revision},
                        "probe_source_revision":if variant == 1 {json!(source_revision)}else{Value::Null},
                        "probe_binary_sha256":if variant == 1 {json!(probe_binary_hash)}else{Value::Null},
                        "collector_source_revision":source_revision,"collector_binary_sha256":probe_binary_hash});
                    let observation = measure(
                        binaries[variant],
                        db,
                        inputs.tape_root,
                        query,
                        &run_root,
                        &binding,
                        expected_touches,
                    );
                    let observation = match observation {
                        Ok(row) => row,
                        Err(error) => {
                            write_canonical_json(
                                &run_root.join("failure.json"),
                                &json!({
                                "status":"failed", "variant":label,"id":id,"mode":mode,
                                "iteration":iteration,"warmup":warmup,"error":error.to_string(),
                                "baseline_identity_must_not_be_substituted":true}),
                            )?;
                            return Err(error);
                        }
                    };
                    write_canonical_json(
                        &run_root.join("observation.json"),
                        &json!({
                        "binding":binding,"observation":observation}),
                    )?;
                    if variant == 1 {
                        let post_started = Instant::now();
                        let post_slot = baseline_custody::copy_files(db)?;
                        let post_microseconds = post_started.elapsed().as_micros();
                        write_canonical_json(
                            &run_root.join("custody-after-slot.json"),
                            &post_slot,
                        )?;
                        write_canonical_json(
                            &run_root.join("custody-comparison.json"),
                            &json!({
                            "passed":post_slot == before,"file_hash_microseconds":post_microseconds,
                            "scope":"complete candidate CLI/direct-probe slot; no mutation allowed"}),
                        )?;
                        if post_slot != before {
                            return Err(
                                "candidate changed during complete product/probe slot".into()
                            );
                        }
                    }
                    if !warmup {
                        measured[variant].push(observation);
                    }
                    if mode == "filesystem-cold" && variant == 1 {
                        discard_own_copy(db)?;
                    }
                }
            }
            if mode == "hot" {
                finish_baseline(&hot[0], &query_root, master_accounting)?;
            }
            let baseline = summarize(&measured[0])?;
            let candidate = summarize(&measured[1])?;
            let passed = ["p95", "p99"].iter().all(|p| {
                candidate["elapsed_microseconds"][p].as_u64()
                    <= baseline["elapsed_microseconds"][p].as_u64()
            }) && candidate["peak_rss_bytes"].as_u64()
                <= baseline["peak_rss_bytes"].as_u64()
                && candidate["observed_sqlite_temp_bytes"] == 0
                && candidate["direct_touch_sort"] == 0
                && candidate["direct_touch_autoindex"] == 0;
            all_passed &= passed;
            comparisons.push(json!({"query_id":id,"mode":mode,"baseline":baseline,
                "candidate":candidate,"passed":passed,
                "acceptance_scope":"amended performance protocol; baseline counters non-comparable; zero observed temp only; no baseline result-equivalence claim"}));
            if mode == "hot" {
                for db in &hot {
                    discard_own_copy(db)?;
                }
            }
        }
    }
    for (db, slot) in deferred_baseline {
        finish_baseline(&db, &slot, master_accounting)?;
        discard_own_copy(&db)?;
    }
    for variant in 0..2 {
        if sha256_file(databases[variant])? != database_hashes[variant]
            || sha256_file(binaries[variant])? != binary_hashes[variant]
        {
            return Err("performance input custody changed".into());
        }
    }
    write_canonical_json(
        &root.join("result.json"),
        &json!({
        "passed":all_passed,"manifest_sha256":manifest_hash,
        "comparison_label":baseline_custody::COMPARISON_LABEL,
        "reconstruction_amendment_sha256":baseline_custody::RECONSTRUCTION_AMENDMENT_SHA256,
        "comparator_receipt_sha256":inputs.comparator_receipt_sha256,
        "baseline_copy_amendment_sha256":baseline_custody::AMENDMENT_SHA256,
        "telemetry_amendment_sha256":TELEMETRY_AMENDMENT_SHA256,
        "acceptance_scope":"amended performance protocol; not an exhaustive telemetry or baseline result-equivalence claim",
        "baseline_cli_counters":"unavailable / non-comparable",
        "temporary_byte_limitation":TEMP_LIMITATION,
        "counter_scope":super::statement_probe::COUNTER_SCOPE,
        "database_hash_custody":"immutable masters verified before/after; candidate files and directory verified every slot; baseline each hot series/cold copy verified with full pre/post file custody and post schema/logical comparison to byte-equal initial master; cold post validation deferred until complete matrix",
        "warmups_per_query_variant_mode":3,"measured_iterations_per_query_variant_mode":30,
        "fresh_process_each_iteration":true,"rss_collector":"/usr/bin/time -l",
        "run_order":"query ID order, hot then filesystem-cold; alternate variant first by query index plus iteration index",
        "cache_policy":manifest["os_cache_policy"],"comparisons":comparisons}),
    )?;
    if !all_passed {
        return Err("amended performance thresholds failed".into());
    }
    Ok(())
}

pub fn alternating_order(query: usize, iteration: usize) -> [usize; 2] {
    if (query + iteration) % 2 == 0 {
        [0, 1]
    } else {
        [1, 0]
    }
}

fn validate_manifest(value: &Value) -> ProofResult<()> {
    if value["fresh_process_per_measurement"] != true
        || value["binary_identity_slots"]["baseline"]["sha256"] != BASELINE_BINARY_SHA256
    {
        return Err("unsupported performance manifest contract".into());
    }
    let queries = value["queries"].as_array().ok_or("queries absent")?;
    if queries.len() != 12 {
        return Err("frozen manifest requires twelve queries".into());
    }
    let mut previous = "";
    for query in queries {
        let id = query["id"].as_str().ok_or("id missing")?;
        if id <= previous
            || query["command"] != "explain"
            || query["target_kind"] != "literal"
            || query["warmups"] != 3
            || query["measured_iterations"] != 30
            || query["cache_modes"] != json!(["hot", "filesystem-cold"])
            || query["flags"]
                != json!({"depth":10,"forensics":false,"include_deleted":false,
                "max_edges":500,"max_fanout":50,"min_confidence":0.5,"pretty":false})
        {
            return Err(format!("unsupported frozen query contract: {id}").into());
        }
        previous = id;
    }
    Ok(())
}

fn database_hash<'a>(files: &'a Value, db: &Path) -> ProofResult<&'a str> {
    files
        .as_array()
        .ok_or("file custody absent")?
        .iter()
        .find(|r| r["path"] == db.to_string_lossy().as_ref() && r["file_type"] == "file")
        .and_then(|r| r["sha256"].as_str())
        .ok_or_else(|| "database hash missing from custody".into())
}

fn prepare_baseline(inputs: &Inputs<'_>, db: &Path, root: &Path) -> ProofResult<()> {
    let started = Instant::now();
    staging_copy(inputs.baseline_database, db, true)?;
    let clone_microseconds = started.elapsed().as_micros();
    let hashing = Instant::now();
    let files = baseline_custody::copy_files(db)?;
    if database_hash(&files, db)? != inputs.baseline_database_sha256 {
        return Err("baseline clone differs from verified master".into());
    }
    let hash_microseconds = hashing.elapsed().as_micros();
    let mut before = inputs.comparator_receipt["accounting"].clone();
    before["files"] = files;
    before["logical_state_source"] = json!({"scope":"inherited from independently verified master via observed complete clone hash equality",
        "comparator_receipt_sha256":inputs.comparator_receipt_sha256,
        "master_sha256":inputs.baseline_database_sha256});
    write_canonical_json(&root.join("custody-before.json"), &before)?;
    write_canonical_json(
        &root.join("baseline-preparation.json"),
        &json!({
        "clone_microseconds":clone_microseconds,"file_hash_microseconds":hash_microseconds,
        "copy_bytes":fs::metadata(db)?.len(),"copy":db,
        "method":"Darwin clonefile; no byte-copy fallback",
        "preparation_excluded_from_cli_timing":true}),
    )
}

fn finish_baseline(db: &Path, root: &Path, master: &Value) -> ProofResult<()> {
    let started = Instant::now();
    let mut after = baseline_custody::logical_snapshot(db)?;
    let logical_microseconds = started.elapsed().as_micros();
    let hashing = Instant::now();
    after["files"] = baseline_custody::copy_files(db)?;
    let hash_microseconds = hashing.elapsed().as_micros();
    write_canonical_json(&root.join("custody-after.json"), &after)?;
    let comparison = baseline_custody::compare(master, &after);
    write_canonical_json(
        &root.join("custody-comparison.json"),
        &json!({
        "passed":comparison.is_ok(),"error":comparison.as_ref().err().map(|e| e.to_string()),
        "logical_microseconds":logical_microseconds,"file_hash_microseconds":hash_microseconds,
        "allowed_query_results_changed":master["tables"]["query_results"] != after["tables"]["query_results"],
        "file_change_explanation":"baseline initialization/query_results persistence and associated pages/WAL/SHM only; unchanged schema and all other logical tables required"}),
    )?;
    comparison
}

fn write_io_plan(inputs: &Inputs<'_>, root: &Path) -> ProofResult<()> {
    let b = fs::metadata(inputs.baseline_database)?.len() as u128;
    let c = fs::metadata(inputs.candidate_database)?.len() as u128;
    write_canonical_json(
        &root.join("io-plan.json"),
        &json!({
        "scope":"static successful-path performance working-copy counts; logical bytes, not measured physical device I/O",
        "baseline_master_bytes":b,"candidate_master_bytes":c,
        "baseline_working_hashes_before":3984,"baseline_working_hashes_after":816,
        "candidate_working_hashes_before":5568,"candidate_working_hashes_after":1584,
        "baseline_working_hash_bytes_lower_bound_decimal":(816*b).to_string(),
        "candidate_working_hash_bytes_decimal":(1584*c).to_string(),
        "baseline_cold_required_hashes":792,"baseline_cold_hash_bytes_lower_bound_decimal":(792*b).to_string(),
        "baseline_working_logical_passes_before":1584,"baseline_working_logical_passes_after":408,
        "tables_per_logical_pass":7,"verified_master_logical_pass_reused":true,
        "baseline_clones":408,"baseline_full_byte_copies":0,
        "candidate_full_byte_copies":408,"candidate_copy_bytes_read_and_written_each_decimal":(408*c).to_string(),
        "peak_retained_baseline_working_databases":397,
        "working_logical_disk_lower_bound_before_growth_decimal":(397*b+c).to_string(),
        "other_costs":"master pre/post hashes, comparator preflight, corpus/config/output hashes, two rebuilds and two concurrency copies, live CoW clones, sidecars, raw artifacts and post-write growth remain additional; CoW allocated blocks can be shared and are not unique physical storage",
        "cache_policy":"baseline cold preparation before matrix; cold post scans after complete matrix; hot baseline post scan at series boundary; candidate per-slot custody/direct probes still condition cache; no OS eviction claim",
        "technical_review_status":"implementation choice, not PDO certification"}),
    )
}

fn staging_copy(source: &Path, output: &Path, writable_baseline: bool) -> ProofResult<()> {
    let directory = output.parent().ok_or("copy parent missing")?;
    fs::create_dir(directory)?;
    if writable_baseline {
        return clone_baseline(source, output);
    }
    // create_new reserves the candidate destination; no overwrite or resume.
    let mut destination = File::options().write(true).create_new(true).open(output)?;
    std::io::copy(&mut File::open(source)?, &mut destination)?;
    destination.sync_all()?;
    if !writable_baseline {
        let mut permissions = destination.metadata()?.permissions();
        permissions.set_readonly(true);
        destination.set_permissions(permissions)?;
        let mut permissions = fs::metadata(directory)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(directory, permissions)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn clone_baseline(source: &Path, output: &Path) -> ProofResult<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    let from = std::ffi::CString::new(source.as_os_str().as_bytes())?;
    let to = std::ffi::CString::new(output.as_os_str().as_bytes())?;
    // clonefile fails for an existing destination or unsupported filesystem.
    if unsafe { libc::clonefile(from.as_ptr(), to.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    fs::set_permissions(output, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn clone_baseline(_: &Path, _: &Path) -> ProofResult<()> {
    Err("baseline staging CoW clones require Darwin".into())
}

fn discard_own_copy(db: &Path) -> ProofResult<()> {
    use std::os::unix::fs::PermissionsExt;
    // Only a directory created_new by this run, after all custody is retained.
    baseline_custody::validate_copy_entries(db)?;
    let directory = db.parent().ok_or("copy parent missing")?;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    for entry in fs::read_dir(directory)? {
        fs::remove_file(entry?.path())?;
    }
    fs::remove_dir(directory)?;
    Ok(())
}

fn measure(
    binary: &Path,
    db: &Path,
    tapes: &Path,
    query: &Value,
    root: &Path,
    binding: &Value,
    expected_touches: &Value,
) -> ProofResult<Value> {
    let measurement_started = Instant::now();
    let home = root.join("home");
    let config = home.join(".engram");
    fs::create_dir_all(&config)?;
    // Explicit HOME and cwd avoid discovering the caller's workspace config.
    write_new_file(
        &config.join("config.yml"),
        serde_yaml::to_string(&json!({
        "db":db,"tapes_dir":tapes,"metrics":{"enabled":false}}))?
        .as_bytes(),
    )?;
    let temp = root.join("sqlite-temp");
    fs::create_dir(&temp)?;
    let sampler = DiskSampler::start(
        &temp,
        &root.join("temp-samples.jsonl"),
        &root.join("temp-peak.json"),
    )?;
    let stdout_path = root.join("stdout.json");
    let stderr_path = root.join("stderr-and-time.txt");
    let target = query["target"].as_str().ok_or("query target missing")?;
    let projection_env = if binding["variant"] == "candidate" {
        "1"
    } else {
        "0"
    };
    let argv = [
        "explain",
        target,
        "--depth",
        "10",
        "--max-fanout",
        "50",
        "--max-edges",
        "500",
        "--min-confidence",
        "0.5",
    ];
    let start = Instant::now();
    let status = Command::new("/usr/bin/time")
        .arg("-l")
        .arg(binary)
        .args(argv)
        .current_dir(&home)
        .env_clear()
        .env("HOME", &home)
        .env("TMPDIR", &temp)
        .env("SQLITE_TMPDIR", &temp)
        .env("T1772_DIRECT_TOUCH_PROJECTION", projection_env)
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("TZ", "UTC")
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            File::options()
                .write(true)
                .create_new(true)
                .open(&stdout_path)?,
        ))
        .stderr(Stdio::from(
            File::options()
                .write(true)
                .create_new(true)
                .open(&stderr_path)?,
        ))
        .status()?;
    let elapsed = start.elapsed().as_micros() as u64;
    let disk = sampler.finish()?;
    let baseline = binding["variant"] == "baseline";
    let stderr = fs::read_to_string(&stderr_path)?;
    let rss = parse_darwin_rss(&stderr)?;
    // Product completion is persisted before the separate SQL probe starts.
    write_canonical_json(
        &root.join("process.json"),
        &json!({"binding":binding,"argv":argv,"collector_argv":["/usr/bin/time","-l"],"binary":binary,
        "counter_scope":"none: product CLI is uninstrumented",
        "exit_code":status.code(),"success":status.success(),
        "elapsed_microseconds":elapsed,"peak_rss_bytes":rss,
        "environment":{"HOME":home,"TMPDIR":temp,"SQLITE_TMPDIR":temp,
            "T1772_DIRECT_TOUCH_PROJECTION":projection_env,
            "LC_ALL":"C","LANG":"C","TZ":"UTC"},
        "stdout_sha256":sha256_file(&stdout_path)?,"stderr_sha256":sha256_file(&stderr_path)?}),
    )?;
    if !status.success() {
        return Err("ordinary CLI measurement failed".into());
    }
    let validation_started = Instant::now();
    let canonical_output = root.join("canonical-output.json");
    let output: Value = serde_json::from_reader(File::open(&stdout_path)?)?;
    write_canonical_json(&canonical_output, &output)?;
    if output["query"]["anchors"] != binding["derived_anchors"] {
        return Err("product CLI anchors differ from bound SQL probe anchors".into());
    }
    let actual = output.get("t1772_direct_touches");
    let exact = actual == Some(expected_touches);
    write_canonical_json(
        &root.join("direct-touch-comparison.json"),
        &json!({
        "binding":binding,"expected":expected_touches,"actual":actual,
        "candidate_exact":if baseline {Value::Null}else{json!(exact)},
        "status":if baseline {"unavailable: frozen CLI merges direct and lineage sessions and may truncate; reconstructed comparator output retained, no direct-equivalence claim"}else if exact{"exact"}else{"mismatch"},
        "baseline_legacy_semantics":if baseline {json!("v3 feature fanout plus lineage/session formatting; raw result_id/rating_hint retained, write costs included; differences are not classified as correctness success")}else{Value::Null},
        "elapsed_microseconds":validation_started.elapsed().as_micros()}),
    )?;
    if !baseline && !exact {
        return Err("candidate invocation direct-touch projection differs from complete ordered frozen oracle".into());
    }
    let probe_path = root.join("statement-probe.json");
    let probe = if baseline {
        Value::Null
    } else {
        super::statement_probe::run(db, binding, &probe_path)?
    };
    // Warmups have the same zero-observed requirement as measured candidate slots.
    if !baseline && (disk["status"] != "observed" || disk["peak_observed_logical_bytes"] != 0) {
        return Err("candidate temporary-byte observation missing, failed or nonzero".into());
    }
    Ok(json!({"elapsed_microseconds":elapsed,"peak_rss_bytes":rss,
        "collector_setup_postprocessing_probe_microseconds":measurement_started.elapsed().as_micros().saturating_sub(elapsed as u128),
        "counter_scope":if baseline {"unavailable / non-comparable; no baseline probe"}else{super::statement_probe::COUNTER_SCOPE},
        "candidate_direct_touch_exact":if baseline {Value::Null}else{json!(exact)},
        "baseline_projection_status":if baseline {json!("unavailable; reconstructed comparator output retained without direct-equivalence claim")}else{Value::Null},
        "actual_cli_statement_counters":{"status":"unavailable / non-comparable","sort":null,"autoindex":null,
            "source_revision":binding["product_source_revision"],"binary_sha256":binding["product_binary_sha256"]},
        "complete_temporary_bytes":Value::Null,
        "temporary_byte_limitation":TEMP_LIMITATION,"zero_total_temporary_allocation_claimed":false,
        "temp_collector_identity":{"module":"src/proof/measurement.rs::DiskSampler","source_revision":binding["collector_source_revision"],"binary_sha256":binding["collector_binary_sha256"]},
        "temp_samples_path":root.join("temp-samples.jsonl"),"temp_collector_errors":[],
        "canonical_output_sha256":sha256_file(&canonical_output)?,
        "observed_sqlite_temp_bytes":disk["peak_observed_logical_bytes"],
        "temp_collection":disk,"direct_touch_sort":probe["direct_touch_sort"],
        "direct_touch_autoindex":probe["direct_touch_autoindex"],
        "direct_rows_visited":probe["direct_rows_visited"],"posting_rows_visited":probe["posting_rows_visited"],
        "statement_probe_sha256":if baseline {Value::Null}else{json!(sha256_file(&probe_path)?)}}))
}

pub fn parse_darwin_rss(text: &str) -> ProofResult<u64> {
    let rows = text
        .lines()
        .filter(|line| line.trim_end().ends_with(" maximum resident set size"))
        .collect::<Vec<_>>();
    if rows.len() != 1 {
        return Err("missing or ambiguous Darwin RSS observation".into());
    }
    Ok(rows[0]
        .split_whitespace()
        .next()
        .ok_or("RSS missing")?
        .parse()?)
}

fn summarize(rows: &[Value]) -> ProofResult<Value> {
    if rows.len() != 30 {
        return Err("measurement count is not thirty".into());
    }
    let times = rows
        .iter()
        .map(|r| r["elapsed_microseconds"].as_u64().ok_or("elapsed missing"))
        .collect::<Result<Vec<_>, _>>()?;
    let candidate = rows[0]["statement_probe_sha256"].is_string();
    let metrics = |field: &str| -> ProofResult<Value> {
        if !candidate {
            return Ok(Value::Null);
        }
        let values = rows
            .iter()
            .map(|r| r[field].as_u64().ok_or("candidate direct metric absent"))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(percentiles(&values)?)
    };
    Ok(json!({"elapsed_microseconds":percentiles(&times)?,
        "actual_cli_statement_counters":rows[0]["actual_cli_statement_counters"],
        "counter_scope":rows[0]["counter_scope"],
        "direct_rows_visited":metrics("direct_rows_visited")?,
        "posting_rows_visited":metrics("posting_rows_visited")?,
        "row_metric_scope":"candidate direct-probe rows delivered before dedup; baseline unavailable; not timed CLI/internal page visits",
        "comparative_cli_counter_improvement_claimed":false,
        "temporary_byte_limitation":TEMP_LIMITATION,"zero_total_temporary_allocation_claimed":false,
        "temp_collector_identity":rows[0]["temp_collector_identity"],
        "temp_requested_interval_milliseconds":100,"temp_collector_errors":[],
        "temp_maximum_sample_gap_microseconds":rows.iter().filter_map(|r|r["temp_collection"]["maximum_sample_gap_microseconds"].as_u64()).max(),
        "raw_temp_evidence":"each slot retains temp-samples.jsonl and temp-peak.json; observations identify dedicated paths",
        "peak_rss_bytes":rows.iter().filter_map(|r|r["peak_rss_bytes"].as_u64()).max(),
        "observed_sqlite_temp_bytes":rows.iter().filter_map(|r|r["observed_sqlite_temp_bytes"].as_u64()).max(),
        "direct_touch_sort":if candidate {json!(rows.iter().filter_map(|r|r["direct_touch_sort"].as_u64()).sum::<u64>())}else{Value::Null},
        "direct_touch_autoindex":if candidate {json!(rows.iter().filter_map(|r|r["direct_touch_autoindex"].as_u64()).sum::<u64>())}else{Value::Null}}))
}
