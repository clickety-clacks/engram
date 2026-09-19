//! Frozen-manifest ordinary CLI measurements. Missing observations fail closed.
use super::measurement::{DiskSampler, percentiles};
use super::t1772::{
    BASELINE_BYTES, ProofResult, sha256_file, write_canonical_json, write_new_file,
};
use serde_json::{Value, json};
use std::fs::{self, File};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

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
    let hot_root = root.join("hot-databases");
    fs::create_dir(&hot_root)?;
    let hot = [
        hot_root.join("baseline.sqlite"),
        hot_root.join("candidate.sqlite"),
    ];
    for variant in 0..2 {
        immutable_copy(databases[variant], &hot[variant])?;
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
        super::t1772::safe_relative_path(id)?;
        for mode in ["hot", "filesystem-cold"] {
            let query_root = root.join(id).join(mode);
            fs::create_dir_all(&query_root)?;
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
                    let cold = run_root.join("immutable.sqlite");
                    let db = if mode == "filesystem-cold" {
                        // Copy is outside the timed command. Hash afterward, so
                        // verification does not add a pre-query sequential read.
                        immutable_copy(databases[variant], &cold)?;
                        &cold
                    } else {
                        &hot[variant]
                    };
                    let target = query["target"].as_str().ok_or("target absent")?;
                    let anchors =
                        crate::query::format::derive_anchor_candidates(&[target.to_string()]);
                    let binding = json!({"query_id":id,"variant":label,"cache_class":mode,
                        "phase":if warmup {"warmup"} else {"measured"},
                        "iteration":if warmup {iteration} else {iteration - 3},
                        "schedule_iteration":iteration,"order_in_pair":order,
                        "database_path":db,"database_sha256":database_hashes[variant],
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
                    if !warmup {
                        measured[variant].push(observation);
                    }
                    if mode == "filesystem-cold" {
                        if sha256_file(&cold)? != database_hashes[variant] {
                            return Err("query changed filesystem-cold immutable database".into());
                        }
                        // Remove only the disposable file created by this exact
                        // iteration; retain observations and all process outputs.
                        fs::remove_file(&cold)?;
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
                "candidate":candidate,"passed":passed}));
        }
    }
    for variant in 0..2 {
        if sha256_file(&hot[variant])? != database_hashes[variant]
            || sha256_file(binaries[variant])? != binary_hashes[variant]
        {
            return Err("performance input custody changed".into());
        }
    }
    write_canonical_json(
        &root.join("result.json"),
        &json!({
        "passed":all_passed,"manifest_sha256":manifest_hash,
        "counter_scope":super::statement_probe::COUNTER_SCOPE,
        "database_hash_custody":"source digests bound to copies; cold copies verified after each product/probe pair; hot copies verified after entire schedule",
        "warmups_per_query_variant_mode":3,"measured_iterations_per_query_variant_mode":30,
        "fresh_process_each_iteration":true,"rss_collector":"/usr/bin/time -l",
        "run_order":"query ID order, hot then filesystem-cold; alternate variant first by query index plus iteration index",
        "cache_policy":manifest["os_cache_policy"],"comparisons":comparisons}),
    )?;
    if !all_passed {
        return Err("performance acceptance thresholds failed".into());
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

fn immutable_copy(source: &Path, output: &Path) -> ProofResult<()> {
    // create_new reserves the destination; never overwrite or resume a copy.
    let mut destination = File::options().write(true).create_new(true).open(output)?;
    std::io::copy(&mut File::open(source)?, &mut destination)?;
    destination.sync_all()?;
    let mut permissions = destination.metadata()?.permissions();
    permissions.set_readonly(true);
    destination.set_permissions(permissions)?;
    Ok(())
}

fn measure(
    binary: &Path,
    db: &Path,
    tapes: &Path,
    query: &Value,
    root: &Path,
    binding: &Value,
) -> ProofResult<Value> {
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
            "LC_ALL":"C","LANG":"C","TZ":"UTC"},
        "stdout_sha256":sha256_file(&stdout_path)?,"stderr_sha256":sha256_file(&stderr_path)?}),
    )?;
    if !status.success() {
        return Err("ordinary CLI measurement failed".into());
    }
    let canonical_output = root.join("canonical-output.json");
    let output: Value = serde_json::from_reader(File::open(&stdout_path)?)?;
    write_canonical_json(&canonical_output, &output)?;
    if output["query"]["anchors"] != binding["derived_anchors"] {
        return Err("product CLI anchors differ from bound SQL probe anchors".into());
    }
    let probe_path = root.join("statement-probe.json");
    let probe = super::statement_probe::run(db, binding, &probe_path)?;
    Ok(json!({"elapsed_microseconds":elapsed,"peak_rss_bytes":rss,
        "counter_scope":super::statement_probe::COUNTER_SCOPE,
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
