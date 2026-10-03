//! Contract-extraction build-cost benchmark (TASK-082, PRD-CTR-REQ-004).
//!
//! Builds a synthetic repo (~300 files across all 12 supported languages,
//! ordinary symbols plus 2-5 framework calls per file, plus a TASK-088
//! document cohort: a ~5k-line OpenAPI spec, a proto IDL, a GraphQL SDL,
//! and one large non-OpenAPI JSON pinning the negative sniff case), then
//! measures:
//!   - T_build: warm `build_index` wall time (parse + extract + insert),
//!   - E: total `extract_contracts` wall time over trees parsed exactly as
//!     `parse_one_file` parses them (PRD-CTR-REQ-011 — same tree, no second
//!     parse is performed at indexing time; here trees are re-parsed purely
//!     to isolate the extractor's share of the build) plus the document
//!     scanners' `extract_document_contracts` share.
//!
//! Hard gate: E < 0.15 x T_build (TASK-082 acceptance: extraction adds
//! < 15% to index build time). Per-language p50/p95 extraction times are
//! reported. Results are written to bench/contracts-results.md.
//!
//! Run: cargo bench --bench contracts

mod fixture_config;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use fixture_config::build_index;
use wonk::contracts::{
    ContractOptions, DocumentKind, extract_contracts, extract_document_contracts,
    scannable_document_kind,
};
use wonk::indexer::{self, Lang};

const FILES_PER_LANG: usize = 25;
const EXTRACT_ROUNDS: usize = 5;

/// One document file in the TASK-088 cohort (name for the report).
struct DocFile {
    path: PathBuf,
    name: String,
    kind: DocumentKind,
}

/// Per-document extraction timing accumulated across rounds.
struct DocTiming {
    name: String,
    kind: &'static str,
    contracts: usize,
    times: Vec<Duration>,
}

fn main() -> Result<()> {
    let repo = tempfile::tempdir()?;
    let root = repo.path();
    fs::create_dir(root.join(".git"))?;
    fs::create_dir_all(root.join("src"))?;
    fs::create_dir_all(root.join("docs"))?;
    fs::create_dir(root.join(".wonk"))?;

    generate_repo(root)?;

    // Warm build primes fs caches, then the measured warm build.
    build_index(root, true)?;
    let t0 = Instant::now();
    let stats = build_index(root, true)?;
    let t_build = t0.elapsed();

    ensure!(
        stats.file_count >= FILES_PER_LANG * 12,
        "expected at least {} files, indexed {}",
        FILES_PER_LANG * 12,
        stats.file_count
    );
    ensure!(stats.contract_count > 0, "no contracts extracted");

    // E: re-parse each file the way parse_one_file does and time the
    // extractor alone; documents are scanned the way parse_document_file
    // does (kind gate + line scanners). Averaged over rounds for a stable
    // number. Reads/parses happen outside the measured window
    // (PRD-CTR-REQ-011 means indexing never re-parses).
    let files = collect_source_files(root)?;
    ensure!(!files.is_empty(), "no source files found");
    let docs = collect_document_files(root)?;

    let mut per_lang: HashMap<Lang, Vec<Duration>> = HashMap::new();
    let mut doc_timings: Vec<DocTiming> = docs
        .iter()
        .map(|d| DocTiming {
            name: d.name.clone(),
            kind: d.kind.as_str(),
            contracts: 0,
            times: Vec::new(),
        })
        .collect();
    let mut e_total = Duration::ZERO;
    let mut total_contracts = 0usize;
    for round in 0..EXTRACT_ROUNDS {
        for path in &files {
            let content = fs::read_to_string(path)?;
            let lang = indexer::detect_language(path).expect("source file language");
            let parse_source = if lang == Lang::Rust {
                indexer::preprocess_rust_macros(&content)
            } else {
                content.clone()
            };
            let mut parser = indexer::get_parser(lang);
            let tree = parser.parse(parse_source.as_bytes(), None).expect("parse");
            // Only the extractor is timed; parse/read happen outside the
            // measured window (PRD-CTR-REQ-011 means indexing never re-parses).
            let extract_start = Instant::now();
            let contracts =
                extract_contracts(&tree, &parse_source, lang, &ContractOptions::default());
            let took = extract_start.elapsed();
            e_total += took;
            if round == EXTRACT_ROUNDS - 1 {
                per_lang.entry(lang).or_default().push(took);
            }
            if round == 0 {
                total_contracts += contracts.len();
            }
        }
        for (i, doc) in docs.iter().enumerate() {
            let content = fs::read_to_string(&doc.path)?;
            let extract_start = Instant::now();
            let contracts =
                extract_document_contracts(doc.kind, &content, &ContractOptions::default());
            let took = extract_start.elapsed();
            e_total += took;
            if round == EXTRACT_ROUNDS - 1 {
                doc_timings[i].times.push(took);
            }
            if round == 0 {
                doc_timings[i].contracts = contracts.len();
                total_contracts += contracts.len();
            }
        }
    }
    let e_avg = e_total / EXTRACT_ROUNDS as u32;

    let ratio = e_avg.as_secs_f64() / t_build.as_secs_f64();
    ensure!(
        ratio < 0.15,
        "extraction cost {:.1}% of build time exceeds the 15% gate (E={:?}, T={:?})",
        ratio * 100.0,
        e_avg,
        t_build
    );

    let report = render_report(&ReportInputs {
        indexed_files: stats.file_count,
        indexed_contracts: stats.contract_count,
        extracted_contracts: total_contracts,
        e: e_avg,
        t_build,
        ratio,
        per_lang: &per_lang,
        doc_timings: &doc_timings,
    });
    print!("{report}");

    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/contracts-results.md");
    fs::write(&out, report)?;
    println!("written to {}", out.display());
    Ok(())
}

/// Everything the report renderer needs.
struct ReportInputs<'a> {
    indexed_files: usize,
    indexed_contracts: usize,
    extracted_contracts: usize,
    e: Duration,
    t_build: Duration,
    ratio: f64,
    per_lang: &'a HashMap<Lang, Vec<Duration>>,
    doc_timings: &'a [DocTiming],
}

fn render_report(r: &ReportInputs) -> String {
    let ReportInputs {
        indexed_files,
        indexed_contracts,
        extracted_contracts,
        e,
        t_build,
        ratio,
        per_lang,
        doc_timings,
    } = r;
    let mut out = String::new();
    out.push_str("# Contract extraction build-cost results (TASK-082 + TASK-087 + TASK-088)\n\n");
    out.push_str("Synthetic repo: 12 languages x 25 files, ordinary symbols plus 2-5\n");
    out.push_str("framework idioms per file (HTTP/env from TASK-082; ~2 message-kind\n");
    out.push_str("idioms — queue/websocket/job — per file from TASK-087), plus a\n");
    out.push_str("TASK-088 document cohort: ~5k-line OpenAPI spec, proto IDL, GraphQL\n");
    out.push_str("SDL, and one large non-OpenAPI JSON (negative sniff case; stays\n");
    out.push_str("un-indexed).\n");
    out.push_str("`cargo bench --bench contracts`.\n\n");
    out.push_str("| metric | value |\n|---|---|\n");
    out.push_str(&format!("| files indexed | {indexed_files} |\n"));
    out.push_str(&format!(
        "| contracts (build_index) | {indexed_contracts} |\n"
    ));
    out.push_str(&format!(
        "| contracts (extraction pass) | {extracted_contracts} |\n"
    ));
    out.push_str(&format!(
        "| E — extract_contracts total (avg of {EXTRACT_ROUNDS}) | {:.3} ms |\n",
        e.as_secs_f64() * 1e3
    ));
    out.push_str(&format!(
        "| T_build — warm build_index | {:.1} ms |\n",
        t_build.as_secs_f64() * 1e3
    ));
    out.push_str(&format!(
        "| E / T_build | {:.2}% (gate: < 15%) |\n",
        ratio * 100.0
    ));
    out.push_str("\nE covers both surfaces: per-language `extract_contracts` over the\n");
    out.push_str("grammar files plus the document scanners' `extract_document_contracts`\n");
    out.push_str("(see the per-document table). The grpc pre-pass tree walk runs only for\n");
    out.push_str("the seven languages with gRPC facts, so C/C++/Ruby/PHP/C# files pay no\n");
    out.push_str("walk at all.\n");
    out.push_str("\nPer-language extraction p50/p95 (last round):\n\n");
    out.push_str("| language | p50 | p95 | files |\n|---|---|---|---|\n");
    let mut langs: Vec<&Lang> = per_lang.keys().collect();
    langs.sort_by_key(|l| l.name());
    for lang in langs {
        let mut times = per_lang[lang].clone();
        times.sort();
        let p50 = times[times.len() / 2];
        let p95 = times[(times.len() as f64 * 0.95) as usize % times.len()];
        out.push_str(&format!(
            "| {} | {:.1} us | {:.1} us | {} |\n",
            lang.name(),
            p50.as_secs_f64() * 1e6,
            p95.as_secs_f64() * 1e6,
            times.len()
        ));
    }
    out.push_str("\nPer-document extraction p50 (last round; TASK-088 document path):\n\n");
    out.push_str("| document | kind | p50 | contracts |\n|---|---|---|---|\n");
    for d in *doc_timings {
        let mut times = d.times.clone();
        times.sort();
        let p50 = times[times.len() / 2];
        out.push_str(&format!(
            "| {} | {} | {:.1} us | {} |\n",
            d.name,
            d.kind,
            p50.as_secs_f64() * 1e6,
            d.contracts
        ));
    }
    out
}

fn collect_source_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                if entry.file_name() != ".git" && entry.file_name() != ".wonk" {
                    stack.push(path);
                }
            } else if indexer::detect_language(&path).is_some() {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Document files the scanner would read (same gate as `parse_document_file`:
/// extension + lock-file/size bounds via `scannable_document_kind`).
fn collect_document_files(root: &Path) -> Result<Vec<DocFile>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                if entry.file_name() != ".git" && entry.file_name() != ".wonk" {
                    stack.push(path);
                }
            } else if let Some(kind) = scannable_document_kind(&path) {
                let name = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                out.push(DocFile { path, name, kind });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn generate_repo(root: &Path) -> Result<()> {
    for i in 0..FILES_PER_LANG {
        fs::write(root.join(format!("src/js_{i:02}.js")), js_source(i as u32))?;
        fs::write(root.join(format!("src/ts_{i:02}.ts")), ts_source(i as u32))?;
        fs::write(
            root.join(format!("src/tsx_{i:02}.tsx")),
            tsx_source(i as u32),
        )?;
        fs::write(
            root.join(format!("src/py_{i:02}.py")),
            python_source(i as u32),
        )?;
        fs::write(
            root.join(format!("src/rs_{i:02}.rs")),
            rust_source(i as u32),
        )?;
        fs::write(root.join(format!("src/go_{i:02}.go")), go_source(i as u32))?;
        fs::write(
            root.join(format!("src/java_{i:02}.java")),
            java_source(i as u32),
        )?;
        fs::write(root.join(format!("src/c_{i:02}.c")), c_source(i as u32))?;
        fs::write(
            root.join(format!("src/cpp_{i:02}.cpp")),
            cpp_source(i as u32),
        )?;
        fs::write(
            root.join(format!("src/rb_{i:02}.rb")),
            ruby_source(i as u32),
        )?;
        fs::write(
            root.join(format!("src/php_{i:02}.php")),
            php_source(i as u32),
        )?;
        fs::write(
            root.join(format!("src/cs_{i:02}.cs")),
            csharp_source(i as u32),
        )?;
    }
    // TASK-088 document cohort: contract-bearing documents plus one large
    // negative case, so E and T_build both cover the document path.
    fs::write(root.join("docs/openapi.yaml"), openapi_spec())?;
    fs::write(root.join("docs/users.proto"), proto_idl())?;
    fs::write(root.join("docs/schema.graphql"), graphql_sdl())?;
    fs::write(root.join("docs/records.json"), large_non_openapi_json())?;
    Ok(())
}

/// ~5k-line OpenAPI spec (500 path items x get+post): the realistic upper
/// end of the OpenAPI line scanner.
fn openapi_spec() -> String {
    let mut s = String::from("openapi: 3.0.0\ninfo:\n  title: bench\n  version: 1.0.0\npaths:\n");
    for i in 0..500 {
        s.push_str(&format!(
            "  /resources/{i}:\n    get:\n      summary: fetch {i}\n      responses:\n        '200':\n          description: ok\n    post:\n      summary: create {i}\n      responses:\n        '201':\n          description: created\n"
        ));
    }
    s
}

/// Proto IDL: 10 services x 3 rpc methods.
fn proto_idl() -> String {
    let mut s = String::from("syntax = \"proto3\";\npackage bench.v1;\n");
    for i in 0..10 {
        s.push_str(&format!(
            "\nservice Service{i} {{\n  rpc GetItem{i}(GetItem{i}Request) returns (Item{i});\n  rpc ListItems{i}(ListItems{i}Request) returns (stream Item{i});\n  rpc DeleteItem{i}(DeleteItem{i}Request) returns (Empty);\n}}\n"
        ));
    }
    s
}

/// GraphQL SDL: Query + Mutation with 50 fields each.
fn graphql_sdl() -> String {
    let mut s = String::from("type Query {\n");
    for i in 0..50 {
        s.push_str(&format!("  field{i}(id: ID!): String\n"));
    }
    s.push_str("}\n\ntype Mutation {\n");
    for i in 0..50 {
        s.push_str(&format!("  setField{i}(id: ID!, value: String): String\n"));
    }
    s.push_str("}\n");
    s
}

/// ~300 KB / ~5k-line pretty-printed JSON that is NOT OpenAPI: pins the
/// negative sniff case — every line is scanned and the answer must still be
/// "not OpenAPI" in linear time (the file stays un-indexed).
fn large_non_openapi_json() -> String {
    let mut s = String::from("{\n  \"records\": [\n");
    for i in 0..5000 {
        s.push_str(&format!(
            "    {{\"id\": {i}, \"name\": \"record-{i}\", \"value\": {}.{:02}}},\n",
            i % 97,
            i % 891
        ));
    }
    s.push_str("    {\"id\": 5000, \"name\": \"record-5000\", \"value\": 1.0}\n  ]\n}\n");
    s
}

// Each generator emits ordinary code plus 2-5 contract sites (HTTP/env
// from TASK-082 and ~2 message-kind idioms from TASK-087); numbers vary
// per file so paths/topics differ and no accidental dedup masks the work.

fn js_source(i: u32) -> String {
    format!(
        r#"const express = require('express');
const app = express();
const cache = {{}};

function compute{i}(n) {{
  let total = 0;
  for (let k = 0; k < n; k++) {{
    total += k * {i};
  }}
  return total;
}}

function lookup{i}(key) {{
  return cache.get(key);
}}

app.get('/api/v{i}/users/:id', showUser);
app.post('/api/v{i}/orders', createOrder);
app.delete('/api/v{i}/orders/:id', deleteOrder);

producer.send({{ topic: 'events.js.v{i}', messages: [payload] }});
io.emit('updates.v{i}', payload);

async function sync{i}() {{
  const res = await fetch(`${{API_URL}}/api/v{i}/users`);
  const db = process.env.DATABASE_URL;
  return [res, db];
}}
"#
    )
}

fn ts_source(i: u32) -> String {
    format!(
        r#"interface Item{i} {{ id: number; name: string; }}

class Store{i} {{
  private items: Item{i}[] = [];

  add(item: Item{i}): void {{
    this.items.push(item);
  }}

  find(id: number): Item{i} | undefined {{
    return this.items.find(x => x.id === id);
  }}
}}

ch.consume('tasks.ts.v{i}', (msg) => handle(msg));
consumer.subscribe({{ topic: 'events.ts.v{i}' }});

async function pull{i}(): Promise<string> {{
  const data = await axios.get('/api/v{i}/items');
  const token = process.env['API_TOKEN'];
  return JSON.stringify(data) + token;
}}
"#
    )
}

fn tsx_source(i: u32) -> String {
    format!(
        r#"import {{ useEffect, useState }} from 'react';

export function Panel{i}({{ id }}: {{ id: string }}) {{
  const [data, setData] = useState<string>('');
  useEffect(() => {{
    fetch(`/api/v{i}/panels/${{id}}`).then(r => r.text()).then(setData);
    socket.on(`panel.v{i}`, (payload) => setData(payload));
  }}, [id]);
  return <div>{{data}}</div>;
}}
"#
    )
}

fn python_source(i: u32) -> String {
    format!(
        r#"from flask import Flask, Blueprint

app = Flask(__name__)
bp = Blueprint('mod_{i}', __name__, url_prefix='/mod{i}')


def compute_{i}(n: int) -> int:
    total = 0
    for k in range(n):
        total += k * {i}
    return total


@app.get('/api/v{i}/users/<int:uid>')
def get_user_{i}(uid: int):
    return {{'id': uid}}


@bp.route('/items', methods=['POST'])
def create_item_{i}():
    return {{}}


producer.produce('events.py.v{i}', value=payload)
ch.basic_publish(exchange='', routing_key='events.py.v{i}', body=payload)


def fetch_{i}():
    import os
    import requests
    r = requests.get('https://api.example.com/api/v{i}/users')
    key = os.environ['API_KEY']
    return r, key
"#
    )
}

fn rust_source(i: u32) -> String {
    format!(
        r#"#[derive(Debug, Clone)]
pub struct Item{i} {{
    pub id: u32,
    pub name: String,
}}

pub fn compute_{i}(n: u32) -> u32 {{
    let mut total = 0;
    for k in 0..n {{
        total += k * {i};
    }}
    total
}}

#[get("/api/v{i}/items/{{id}}")]
async fn show_item_{i}() -> &'static str {{
    "item"
}}

#[route("/api/v{i}/orders", method = "POST")]
async fn create_order_{i}() -> &'static str {{
    "ok"
}}

async fn queue_{i}() {{
    consumer.subscribe(&["events.rs.v{i}"])?;
    let rec = FutureRecord::to("events.rs.v{i}", 0, payload);
    producer.send(rec, Timeout::Never).await?;
}}

async fn call_{i}() {{
    let _r = reqwest::get("https://api.example.com/api/v{i}/items").await;
    let _k = std::env::var("SERVICE_TOKEN").unwrap_or_default();
}}
"#
    )
}

fn go_source(i: u32) -> String {
    format!(
        r#"package main

import "os"

func compute{i}(n int) int {{
	total := 0
	for k := 0; k < n; k++ {{
		total += k * {i}
	}}
	return total
}}

func routes{i}() {{
	r := gin.New()
	v1 := r.Group("/api/v{i}")
	v1.GET("/users/:id", getUser)
	v1.POST("/orders", createOrder)
	v1.DELETE("/orders/:id", deleteOrder)
}}

func messages{i}() {{
	nc.Publish("events.go.v{i}", data)
	c.AddFunc("*/5 * * * * *", poll{i})
}}

func call{i}() {{
	resp, _ := http.Get("https://api.example.com/api/v{i}/users")
	key := os.Getenv("GO_TOKEN")
	_, _ = resp, key
}}
"#
    )
}

fn java_source(i: u32) -> String {
    format!(
        r#"package demo;

import java.util.List;

public class Service{i} {{
    private final List<String> items = new java.util.ArrayList<>();

    public int compute(int n) {{
        int total = 0;
        for (int k = 0; k < n; k++) {{
            total += k * {i};
        }}
        return total;
    }}

    @GetMapping("/api/v{i}/users/{{id}}")
    public String getUser(@PathVariable String id) {{
        return id;
    }}

    @PostMapping("/api/v{i}/orders")
    public String create() {{
        return "ok";
    }}

    @KafkaListener(topics = "events.java.v{i}")
    public void onEvent(String msg) {{
    }}

    void publish() {{
        kafkaTemplate.send("events.java.v{i}", key, value);
    }}

    String call() {{
        String r = restTemplate.getForObject("https://api.example.com/api/v{i}/users", String.class);
        String token = System.getenv("JAVA_TOKEN");
        return r + token;
    }}
}}
"#
    )
}

fn c_source(i: u32) -> String {
    format!(
        r#"#include <stdlib.h>
#include <string.h>

static int compute_{i}(int n) {{
    int total = 0;
    for (int k = 0; k < n; k++) {{
        total += k * {i};
    }}
    return total;
}}

void fetch_{i}(void) {{
    CURL *h = curl_easy_init();
    curl_easy_setopt(h, CURLOPT_URL, "https://api.example.com/api/v{i}/items");
    char *token = getenv("C_TOKEN");
    (void)token;
}}
"#
    )
}

fn cpp_source(i: u32) -> String {
    format!(
        r#"#include <cstdlib>
#include <vector>
#include <string>

namespace svc{i} {{

class Engine {{
public:
    explicit Engine(int seed) : seed_(seed) {{}}
    int compute(int n) const {{
        int total = 0;
        for (int k = 0; k < n; k++) {{
            total += k * seed_;
        }}
        return total;
    }}
private:
    int seed_;
}};

std::string token_{i}() {{
    const char *t = getenv("CPP_TOKEN");
    return t == nullptr ? "" : t;
}}

}}  // namespace svc{i}
"#
    )
}

fn ruby_source(i: u32) -> String {
    format!(
        r#"module Mod{i}
  def self.compute(n)
    total = 0
    n.times {{ |k| total += k * {i} }}
    total
  end
end

get "/api/v{i}/users/:id" do
  content_type :json
  {{ id: params[:id] }}.to_json
end

post "/api/v{i}/orders" do
  status 201
end

queue_{i} = channel.queue("events.rb.v{i}")
queue_{i}.subscribe do |info, props, body|
  handle(body)
end

x.publish(payload, routing_key: "updates.rb.v{i}")

def pull_{i}
  res = HTTParty.get("https://api.example.com/api/v{i}/users")
  token = ENV['RUBY_TOKEN']
  [res, token]
end
"#
    )
}

fn php_source(i: u32) -> String {
    format!(
        r#"<?php

function compute_{i}($n) {{
    $total = 0;
    for ($k = 0; $k < $n; $k++) {{
        $total += $k * {i};
    }}
    return $total;
}}

class Ctrl{i} {{
    #[Route('/api/v{i}/items', methods: ['GET'])]
    public function list(): array {{
        return [];
    }}
}}

Route::get('/api/v{i}/users/{{id}}', [Ctrl{i}::class, 'list']);

function load_{i}() {{
    $r = Http::get('https://api.example.com/api/v{i}/users');
    $key = $_ENV['PHP_TOKEN'];
    return [$r, $key];
}}
"#
    )
}

fn csharp_source(i: u32) -> String {
    format!(
        r#"using System;
using System.Collections.Generic;

public class Service{i} {{
    private readonly List<string> _items = new();

    public int Compute(int n) {{
        var total = 0;
        for (var k = 0; k < n; k++) {{
            total += k * {i};
        }}
        return total;
    }}
}}

[Route("api/v{i}")]
public class UsersController{i} : ControllerBase
{{
    [HttpGet("users/{{id}}")]
    public string GetUser(string id) => id;

    [HttpPost("orders")]
    public string Create() => "ok";
}}

class Client{i} {{
    async Task Load() {{
        var r = await httpClient.GetAsync("/api/v{i}/users");
        var token = Environment.GetEnvironmentVariable("CS_TOKEN");
    }}
}}
"#
    )
}
