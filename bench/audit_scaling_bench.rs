//! Optimized, repeated public-API performance checks for audit remediation.
//! Run `cargo bench --bench audit_scaling` with the source/Cargo gate quiet.
use anyhow::{Result, ensure};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashSet;
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use wonk::types::{ContractCandidate, ContractKind, ContractRole};
use wonk::{bm25, config::Config, contracts, db, pipeline, ranker, rerank};

struct CountingAllocator;
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(new_size, Ordering::Relaxed);
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;
const WARMUPS: usize = 3;
const SAMPLES: usize = 25;

fn measure(mut operation: impl FnMut() -> Result<()>) -> Result<Value> {
    for _ in 0..WARMUPS {
        operation()?;
    }
    let mut times = Vec::with_capacity(SAMPLES);
    let mut allocs = Vec::with_capacity(SAMPLES);
    let mut bytes = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        ALLOCS.store(0, Ordering::Relaxed);
        BYTES.store(0, Ordering::Relaxed);
        let start = Instant::now();
        operation()?;
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        let a = ALLOCS.load(Ordering::Relaxed);
        let b = BYTES.load(Ordering::Relaxed);
        times.push(elapsed);
        allocs.push(a);
        bytes.push(b);
    }
    let mut sorted = times.clone();
    sorted.sort_by(f64::total_cmp);
    Ok(
        json!({"warmups":WARMUPS,"samples":SAMPLES,"p50_ms":sorted[12],"p95_ms":sorted[23],"raw_ms":times,"allocation_calls":allocs,"allocated_bytes":bytes}),
    )
}
fn classified(path: PathBuf) -> ranker::ClassifiedResult {
    ranker::ClassifiedResult {
        result: wonk::search::SearchResult {
            file: path,
            line: 1,
            col: 0,
            content: "gamma".into(),
        },
        category: ranker::ResultCategory::Other,
        annotation: None,
    }
}
fn index(root: &Path) -> Result<Connection> {
    db::open(&root.join("index.db"))
}

fn path_context(results: &mut Vec<Value>) -> Result<()> {
    for count in [2000, 10000, 20000] {
        let fixture = tempfile::tempdir()?;
        let mut conn = index(fixture.path())?;
        let paths: Vec<_> = (0..count)
            .map(|i| format!("src/module{i}/file{i}.rs"))
            .collect();
        {
            let tx = conn.transaction()?;
            for path in &paths {
                tx.execute("INSERT INTO files(path,language,hash,last_indexed,line_count) VALUES(?1,'rust','h',0,10)",[path])?;
                tx.execute("INSERT INTO file_churn(file,score,last_ts,last_author,primary_author) VALUES(?1,1.0,0,'a','a')",[path])?;
            }
            tx.commit()?;
        }
        for absolute in [false, true] {
            let candidates: Vec<_> = paths
                .iter()
                .map(|p| {
                    classified(if absolute {
                        Path::new("/repo").join(p)
                    } else {
                        PathBuf::from(p)
                    })
                })
                .collect();
            let data = measure(|| {
                let context = rerank::prepare_context(
                    rerank::ContextReqs::none().with_file_churn(),
                    "gamma",
                    &candidates,
                    Some(&conn),
                    &rerank::ContextSources::default(),
                );
                for row in &candidates {
                    ensure!(
                        context.churn_score(row.result.file.to_str().unwrap()) == Some(1.0),
                        "candidate path did not resolve"
                    );
                }
                black_box(context);
                Ok(())
            })?;
            results.push(json!({"finding":"F19","candidates":count,"indexed_files":count,"absolute":absolute,"path_bytes":paths.iter().map(String::len).sum::<usize>(),"measurement":data}));
        }
    }
    Ok(())
}
thread_local! {static POSTINGS: Cell<usize> = const { Cell::new(0) };}
fn trace_postings(event: rusqlite::trace::TraceEvent<'_>) {
    if let rusqlite::trace::TraceEvent::Row(statement) = event
        && statement.sql().contains("SELECT file, tf FROM term_stats")
    {
        POSTINGS.with(|count| count.set(count.get() + 1));
    }
}
fn lexical(results: &mut Vec<Value>) -> Result<()> {
    for count in [2000, 10000, 20000] {
        let fixture = tempfile::tempdir()?;
        let mut conn = index(fixture.path())?;
        {
            let tx = conn.transaction()?;
            for i in 0..count {
                let file = format!("src/file{i}.rs");
                tx.execute("INSERT INTO files(path,language,hash,last_indexed,line_count) VALUES(?1,'rust','h',0,10)",[&file])?;
                for term in ["pub", "fn", "gamma"] {
                    tx.execute(
                        "INSERT INTO term_stats(term,file,tf) VALUES(?1,?2,2)",
                        params![term, file],
                    )?;
                }
            }
            tx.execute("INSERT OR REPLACE INTO corpus_stats(id,n_docs,measured,total_lines) VALUES(1,?1,?1,?2)",params![count,count*10])?;
            tx.execute(
                "INSERT OR REPLACE INTO bm25_meta(key,value) VALUES('generation_ready','1')",
                [],
            )?;
            tx.commit()?;
        }
        conn.trace_v2(
            rusqlite::trace::TraceEventCodes::SQLITE_TRACE_ROW,
            Some(trace_postings),
        );
        let files = HashSet::from(["src/file0.rs".to_string()]);
        let data = measure(|| {
            POSTINGS.with(|value| value.set(0));
            let scores = bm25::file_bm25_scores(
                &conn,
                &files,
                "pub fn gamma",
                bm25::Bm25Params { k1: 1.2, b: 0.75 },
            )
            .expect("completed corpus");
            ensure!(
                scores.len() == 1 && scores["src/file0.rs"] > 0.0,
                "score missing"
            );
            ensure!(
                POSTINGS.with(Cell::get) == 3,
                "postings transfer must equal three candidate-term rows"
            );
            black_box(scores);
            Ok(())
        })?;
        ensure!(
            data["p95_ms"].as_f64().unwrap() < 10.0,
            "BM25 p95 >=10ms at {count} corpus files: {data}"
        );
        results.push(json!({"finding":"F21","corpus_files":count,"candidates":1,"common_terms":3,"postings_rows_transferred":3,"posting_memory_proxy_bytes":3*("src/file0.rs".len()+8),"gate":"p95 < 10ms","measurement":data}));
    }
    Ok(())
}
fn rpc_candidate(i: usize, role: ContractRole) -> ContractCandidate {
    let service = if role == ContractRole::Provider {
        format!("users.v1.Service{i}")
    } else {
        format!("Service{i}")
    };
    let method = if role == ContractRole::Provider {
        "GetUser"
    } else {
        "get_user"
    };
    ContractCandidate {
        kind: ContractKind::Grpc,
        role,
        qualifier: service.clone(),
        identifier: method.into(),
        canonical_id: format!("grpc::{service}::{method}"),
        params: vec![],
        owning_symbol: None,
        line: 1,
        confidence: 1.0,
    }
}
fn rpc(results: &mut Vec<Value>) -> Result<()> {
    for count in [1000, 2000, 4000] {
        let rows: Vec<_> = (0..count)
            .map(|i| rpc_candidate(i, ContractRole::Provider))
            .chain((0..count).map(|i| rpc_candidate(i, ContractRole::Consumer)))
            .collect();
        let scopes = [contracts::RpcJoinScope {
            workspace: "payments".into(),
            candidates: &rows,
        }];
        let data = measure(|| {
            let links = contracts::canonical_rpc_join(&scopes);
            ensure!(links.len() == count, "RPC matching count drifted");
            black_box(links);
            Ok(())
        })?;
        results.push(json!({"finding":"F20","providers":count,"consumers":count,"matches":count,"measurement":data}));
    }
    Ok(())
}
fn normalization(results: &mut Vec<Value>) -> Result<()> {
    for count in [16000, 32000, 64000] {
        for fragment in ["{", "${", "<", "(.:"] {
            let raw = format!("/{}é", fragment.repeat(count));
            let data = measure(|| {
                black_box(contracts::normalize_http_path(&raw));
                Ok(())
            })?;
            results.push(json!({"finding":"F22","unmatched_fragment":fragment,"repetitions":count,"input_bytes":raw.len(),"measurement":data}));
        }
    }
    for count in [16000, 32000, 64000] {
        for fragment in ["/<", "/<a", "/<é"] {
            let raw = format!("{}>", fragment.repeat(count));
            let data = measure(|| {
                let normalized = contracts::normalize_http_path(&raw).unwrap();
                ensure!(
                    normalized.params.len() == usize::from(fragment != "/<"),
                    "distant closer semantics"
                );
                black_box(normalized);
                Ok(())
            })?;
            results.push(json!({"finding":"F22","distant_closer_fragment":fragment,"repetitions":count,"input_bytes":raw.len(),"measurement":data}));
        }
    }
    Ok(())
}
fn bulk_source(provider: usize, consumer: usize) -> String {
    let mut source = "const app = express();\nfunction routes() {\n".to_string();
    for i in 0..20 {
        source.push_str(&format!("app.get('/v{provider}/p{i}', h);\n"));
    }
    source.push_str("}\nasync function callers() {\n");
    for i in 0..20 {
        source.push_str(&format!(
            "await fetch('https://api.io/v{consumer}/p{i}');\n"
        ));
    }
    source.push_str("}\n");
    source
}
fn registry_repo(
    base: &Path,
    registry: &Path,
    name: &str,
    provider: usize,
    consumer: usize,
) -> Result<PathBuf> {
    let root = base.join(name);
    fs::create_dir_all(root.join(".git"))?;
    fs::create_dir_all(root.join("src"))?;
    fs::create_dir_all(root.join(".wonk"))?;
    fs::write(root.join("src/app.js"), bulk_source(provider, consumer))?;
    fs::write(
        root.join(".wonk/config.toml"),
        "[contracts]\nworkspace = ['payments']\n",
    )?;
    let config = Config::load_with_paths(None, Some(&root))?;
    pipeline::build_index_with_config(&root, true, &config)?;
    let dest = registry.join(db::repo_hash(&root));
    fs::create_dir_all(&dest)?;
    fs::copy(root.join(".wonk/index.db"), dest.join("index.db"))?;
    fs::copy(root.join(".wonk/meta.json"), dest.join("meta.json"))?;
    Ok(root)
}
fn workspace(results: &mut Vec<Value>) -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let registry = fixture.path().join("registry");
    fs::create_dir(&registry)?;
    let own = registry_repo(fixture.path(), &registry, "own-api", 98, 0)?;
    for i in 0..10 {
        registry_repo(
            fixture.path(),
            &registry,
            &format!("sibling-{i}"),
            i,
            90 + i,
        )?;
    }
    let conn = db::open_existing(&own.join(".wonk/index.db"))?;
    let rows = contracts::list_contracts(&conn, &contracts::ContractQuery::default())?;
    ensure!(rows.len() == 40, "expected40 own contracts");
    let declared = vec!["payments".into()];
    let data = measure(|| {
        let resolution = contracts::resolve_workspace(&own, &rows, &declared, &registry)?;
        ensure!(
            resolution.links.len() == 40 && resolution.siblings.len() == 10,
            "resolved identity/count drift"
        );
        ensure!(
            resolution
                .status
                .values()
                .filter(|&&s| s == contracts::ConsumerStatus::Linked)
                .count()
                == 20,
            "own consumer status drift"
        );
        black_box(resolution);
        Ok(())
    })?;
    ensure!(
        data["p95_ms"].as_f64().unwrap() < 100.0,
        "11-repo resolution p95 >=100ms: {data}"
    );
    results.push(json!({"finding":"F23","repositories":11,"contracts_per_repository":40,"links":40,"linked_own_consumers":20,"gate":"warm p95 < 100ms","measurement":data}));
    Ok(())
}
fn feedback_delivery(results: &mut Vec<Value>) -> Result<()> {
    for count in [2000, 10000, 20000] {
        let fixture = tempfile::tempdir()?;
        let conn = index(fixture.path())?;
        let rows: Vec<_> = (0..count)
            .map(|i| rerank::ScoredResult {
                classified: classified(PathBuf::from(format!("src/file{i}.rs"))),
                score: 1.0,
                contributions: vec![rerank::Contribution {
                    signal: "kind",
                    value: 1.0,
                    weight: 1.0,
                    weighted: 1.0,
                }],
            })
            .collect();
        let ranked = rerank::RankedSearch {
            groups: vec![(ranker::ResultCategory::Other, rows)],
            query_class: Some(rerank::QueryClass::Symbol),
            near_duplicates: vec![],
            context: rerank::SharedContext::default(),
        };
        let config = wonk::config::FeedbackConfig {
            enabled: true,
            slate_retention: 1,
            author_features: false,
            ..Default::default()
        };
        let mut delivered = 0;
        let mut member_bytes = 0;
        let mut event_bytes = 0;
        let data = measure(|| {
            let mut page = wonk::delivery::select_mcp_search_page(&ranked, Some(4000), None, true)?;
            delivered = page.rows.len();
            ensure!(
                delivered > 0 && delivered < count,
                "budget must truncate broad query"
            );
            let selected = page.feedback_members();
            let slate = wonk::feedback::build_and_store_selected_slate(
                &conn, "gamma", &ranked, &selected, &config,
            )?;
            ensure!(
                slate.members.len() == delivered,
                "only delivered members may persist"
            );
            page.stamp(&slate);
            for (row, member) in page.rows.iter().zip(&slate.members) {
                ensure!(
                    row.output.identity.as_deref() == Some(member.identity.as_str()),
                    "stamp association drift"
                );
            }
            member_bytes = conn.query_row(
                "SELECT length(members) FROM feedback_slates WHERE token=?1",
                [&slate.token],
                |r| r.get::<_, i64>(0),
            )?;
            wonk::feedback::record_feedback(&conn, &slate.token, &["1".into()], "bench-session")?;
            event_bytes = conn.query_row(
                "SELECT length(features) FROM feedback_events ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get::<_, i64>(0),
            )?;
            black_box(page);
            Ok(())
        })?;
        results.push(json!({"finding":"F09","candidates":count,"mcp_budget_tokens":4000,"delivered_rows":delivered,"stored_members":delivered,"stored_member_bytes":member_bytes,"latest_event_feature_bytes":event_bytes,"stamp_corresponding_pairs":delivered,"measurement":data}));
    }
    Ok(())
}
fn main() -> Result<()> {
    let mut results = Vec::new();
    path_context(&mut results)?;
    lexical(&mut results)?;
    rpc(&mut results)?;
    normalization(&mut results)?;
    workspace(&mut results)?;
    feedback_delivery(&mut results)?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"profile":"optimized bench","warmups":WARMUPS,"samples":SAMPLES,"allocation_note":"System allocation requests incl realloc; cumulative allocated bytes, not resident/peak memory","results":results})
        )?
    );
    Ok(())
}
