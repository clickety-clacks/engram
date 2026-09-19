//! Frozen-manifest ordinary CLI measurements. Missing observations fail closed.
use super::baseline_custody;
use super::measurement::{DiskSampler, percentiles};
use super::t1772::{
    BASELINE_BYTES, ProofResult, sha256_file, write_canonical_json, write_new_file,
};
use serde_json::{Value, json};
use std::fs::{self, File};
use std::path::Path;
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

pub struct Inputs<'a> {
    pub baseline_binary: &'a Path,
    pub baseline_database: &'a Path,
    pub baseline_database_sha256: &'a str,
    pub candidate_binary: &'a Path,
    pub candidate_database: &'a Path,
    pub tape_root: &'a Path,
    pub manifest: &'a Path,
}

pub fn run(inputs: &Inputs<'_>, root: &Path) -> ProofResult<()> {
    if !cfg!(target_os = "macos") {
        return Err("performance RSS collector requires Darwin /usr/bin/time -l".into());
    }
    if sha256_file(inputs.baseline_binary)? != BASELINE_BINARY_SHA256
        || sha256_file(inputs.baseline_database)? != inputs.baseline_database_sha256
        || fs::metadata(inputs.baseline_database)?.len() != BASELINE_BYTES
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
        &root.join("protocol-r2.json"),
        &json!({
        "version":2,"parent_manifest_sha256":manifest_hash,
        "parent_spec_sha256":"df43b6e63184d2e02803905b841dc46f5b6f3d0573c3b5a6ee68ca7d0ea81bea",
        "expected_projection_sha256":"417d3aeabaf80a2196b51c3f0b7dad5381903a06d4017b7960410203100d3b31",
        "amendment_sha256":baseline_custody::AMENDMENT_SHA256,
        "source_revision":option_env!("T1772_BUILD_REVISION"),
        "baseline_binary_sha256":BASELINE_BINARY_SHA256,
        "preparation_order":"create byte copy when required, hash copy, baseline read-only logical/schema custody or candidate file custody; invoke CLI; post-custody; validate output; SQL probes",
        "baseline_policy":"one writable copy per hot query series; fresh writable copy per cold slot; only query_results application content may change",
        "candidate_policy":"database and its directory read-only; exact hash and directory custody after every slot",
        "cache_limit":"copy/hash/custody reads can warm filesystem cache; filesystem-cold means a fresh copy, never cache eviction",
        "unresolved":["actual frozen CLI statement counters","complete temporary-file observation","frozen baseline pre-lineage output extraction"],
        "eligible_for_full_performance_pass":false}),
    )?;
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
                for variant in 0..2 {
                    let started = Instant::now();
                    staging_copy(databases[variant], &hot[variant], variant == 0)?;
                    if sha256_file(&hot[variant])? != database_hashes[variant] {
                        return Err("hot copy mismatch".into());
                    }
                    write_canonical_json(
                        &query_root.join(format!(
                            "{}-preparation.json",
                            ["baseline", "candidate"][variant]
                        )),
                        &json!({"elapsed_microseconds":started.elapsed().as_micros(),"copy":hot[variant],
                            "initial_sha256":database_hashes[variant],"bytes":fs::metadata(&hot[variant])?.len()}),
                    )?;
                }
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
                    fs::create_dir(&run_root)?;
                    let prepare_start = Instant::now();
                    let cold = run_root.join("copy/database.sqlite");
                    let db = if mode == "filesystem-cold" {
                        staging_copy(databases[variant], &cold, variant == 0)?;
                        if sha256_file(&cold)? != database_hashes[variant] {
                            return Err("cold copy mismatch".into());
                        }
                        &cold
                    } else {
                        &hot[variant]
                    };
                    let before = if variant == 0 {
                        baseline_custody::snapshot(db)?
                    } else {
                        baseline_custody::copy_files(db)?
                    };
                    let before_database_sha256 = sha256_file(db)?;
                    write_canonical_json(&run_root.join("custody-before.json"), &before)?;
                    write_canonical_json(
                        &run_root.join("preparation.json"),
                        &json!({
                        "elapsed_microseconds":prepare_start.elapsed().as_micros(),
                        "copy_bytes":fs::metadata(db)?.len(),"master_sha256":database_hashes[variant],
                        "preparation_excluded_from_cli_timing":true}),
                    )?;
                    let target = query["target"].as_str().ok_or("target absent")?;
                    let anchors =
                        crate::query::format::derive_anchor_candidates(&[target.to_string()]);
                    let binding = json!({"query_id":id,"variant":label,"cache_class":mode,
                        "phase":if warmup {"warmup"} else {"measured"},
                        "iteration":if warmup {iteration} else {iteration - 3},
                        "schedule_iteration":iteration,"order_in_pair":order,
                        "database_path":db,"master_database_sha256":database_hashes[variant],
                        "database_sha256":before_database_sha256,
                        "amendment_sha256":baseline_custody::AMENDMENT_SHA256,
                        "manifest_sha256":manifest_hash,"target":target,"flags":query["flags"],
                        "derived_anchors":anchors,"product_binary_sha256":binary_hashes[variant],
                        "probe_binary_sha256":probe_binary_hash});
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
                        let post_slot = baseline_custody::copy_files(db)?;
                        write_canonical_json(
                            &run_root.join("custody-after-slot.json"),
                            &post_slot,
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
                    if mode == "filesystem-cold" {
                        discard_own_copy(db)?;
                    }
                }
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
                "candidate":candidate,"observed_thresholds_passed":passed,"passed":false,
                "reason":"actual CLI counters, complete temporary bytes and baseline semantic extraction remain unresolved"}));
            if mode == "hot" {
                for db in &hot {
                    discard_own_copy(db)?;
                }
            }
        }
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
        "passed":false,"observed_thresholds_passed":all_passed,"manifest_sha256":manifest_hash,
        "unresolved":["actual frozen CLI statement counters","complete temporary-file observation","frozen baseline pre-lineage output extraction"],
        "counter_scope":super::statement_probe::COUNTER_SCOPE,
        "database_hash_custody":"immutable masters verified before/after; candidate files and directory verified every slot; baseline schema/all non-query_results tables verified every slot with file custody",
        "warmups_per_query_variant_mode":3,"measured_iterations_per_query_variant_mode":30,
        "fresh_process_each_iteration":true,"rss_collector":"/usr/bin/time -l",
        "run_order":"query ID order, hot then filesystem-cold; alternate variant first by query index plus iteration index",
        "cache_policy":manifest["os_cache_policy"],"comparisons":comparisons}),
    )?;
    Err("full performance gate blocked: actual baseline CLI counters, complete temporary-file observation and baseline direct projection unavailable; no eligibility permitted".into())
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

fn staging_copy(source: &Path, output: &Path, writable_baseline: bool) -> ProofResult<()> {
    let directory = output.parent().ok_or("copy parent missing")?;
    fs::create_dir(directory)?;
    // create_new reserves the destination; never overwrite or resume a copy.
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

fn discard_own_copy(db: &Path) -> ProofResult<()> {
    use std::os::unix::fs::PermissionsExt;
    // Only a directory created_new by this run, after all custody is retained.
    baseline_custody::copy_files(db)?;
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
    let post_started = Instant::now();
    let before: Value = serde_json::from_reader(File::open(root.join("custody-before.json"))?)?;
    let baseline = binding["variant"] == "baseline";
    let after = if baseline {
        baseline_custody::snapshot(db)?
    } else {
        baseline_custody::copy_files(db)?
    };
    write_canonical_json(&root.join("custody-after.json"), &after)?;
    let custody = if baseline {
        baseline_custody::compare(&before, &after)
    } else if before != after
        || sha256_file(db)?
            != binding["database_sha256"]
                .as_str()
                .ok_or("bound database hash missing")?
    {
        Err("candidate copy/directory changed".into())
    } else {
        Ok(())
    };
    write_canonical_json(
        &root.join("custody-comparison.json"),
        &json!({
        "passed":custody.is_ok(),"error":custody.as_ref().err().map(|e|e.to_string()),
        "elapsed_microseconds":post_started.elapsed().as_micros(),
        "allowed_query_results_changed":baseline && before["tables"]["query_results"] != after["tables"]["query_results"],
        "file_change_explanation":if baseline {"pinned baseline initialization/query_results persistence and associated SQLite page/WAL/SHM effects; all other table content and schema must remain identical"}else{"no database or directory mutation permitted"}}),
    )?;
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
    custody?;
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
        "status":if baseline {"unresolved: frozen CLI merges direct and lineage sessions and may truncate; no complete pre-lineage extractor"}else if exact{"exact"}else{"mismatch"},
        "baseline_legacy_semantics":if baseline {json!("v3 feature fanout plus lineage/session formatting; raw result_id/rating_hint retained, write costs included; differences are not classified as correctness success")}else{Value::Null},
        "elapsed_microseconds":validation_started.elapsed().as_micros()}),
    )?;
    if !baseline && !exact {
        return Err("candidate invocation direct-touch projection differs from complete ordered frozen oracle".into());
    }
    let probe_path = root.join("statement-probe.json");
    let probe = super::statement_probe::run(db, binding, &probe_path)?;
    Ok(json!({"elapsed_microseconds":elapsed,"peak_rss_bytes":rss,
        "collector_setup_postprocessing_probe_microseconds":measurement_started.elapsed().as_micros().saturating_sub(elapsed as u128),
        "counter_scope":super::statement_probe::COUNTER_SCOPE,
        "candidate_direct_touch_exact":if baseline {Value::Null}else{json!(exact)},
        "baseline_projection_status":if baseline {json!("unresolved; comparison cannot be accepted")}else{Value::Null},
        "actual_cli_statement_counters":Value::Null,
        "complete_temporary_bytes":Value::Null,
        "canonical_output_sha256":sha256_file(&canonical_output)?,
        "observed_sqlite_temp_bytes":disk["peak_observed_logical_bytes"],
        "temp_collection":disk,"direct_touch_sort":probe["direct_touch_sort"],
        "direct_touch_autoindex":probe["direct_touch_autoindex"],
        "direct_rows_visited":probe["direct_rows_visited"],"posting_rows_visited":probe["posting_rows_visited"],
        "statement_probe_sha256":sha256_file(&probe_path)?}))
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
    Ok(json!({"elapsed_microseconds":percentiles(&times)?,
        "counter_scope":super::statement_probe::COUNTER_SCOPE,
        "peak_rss_bytes":rows.iter().filter_map(|r|r["peak_rss_bytes"].as_u64()).max(),
        "observed_sqlite_temp_bytes":rows.iter().filter_map(|r|r["observed_sqlite_temp_bytes"].as_u64()).max(),
        "direct_touch_sort":rows.iter().filter_map(|r|r["direct_touch_sort"].as_u64()).sum::<u64>(),
        "direct_touch_autoindex":rows.iter().filter_map(|r|r["direct_touch_autoindex"].as_u64()).sum::<u64>()}))
}
