//! Contract-extraction build-cost benchmark (TASK-082, PRD-CTR-REQ-004).
//!
//! Builds a synthetic repo (~300 files across all 12 supported languages,
//! ordinary symbols plus 2-5 framework calls per file), then measures:
//!   - T_build: warm `build_index` wall time (parse + extract + insert),
//!   - E: total `extract_contracts` wall time over trees parsed exactly as
//!     `parse_one_file` parses them (PRD-CTR-REQ-011 — same tree, no second
//!     parse is performed at indexing time; here trees are re-parsed purely
//!     to isolate the extractor's share of the build).
//!
//! Hard gate: E < 0.15 x T_build (TASK-082 acceptance: extraction adds
//! < 15% to index build time). Per-language p50/p95 extraction times are
//! reported. Results are written to bench/contracts-results.md.
//!
//! Run: cargo bench --bench contracts

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use wonk::contracts::{ContractOptions, extract_contracts};
use wonk::indexer::{self, Lang};
use wonk::pipeline::build_index;

const FILES_PER_LANG: usize = 25;
const EXTRACT_ROUNDS: usize = 5;

fn main() -> Result<()> {
    let repo = tempfile::tempdir()?;
    let root = repo.path();
    fs::create_dir(root.join(".git"))?;
    fs::create_dir_all(root.join("src"))?;
    fs::create_dir(root.join(".wonk"))?;

    generate_repo(root)?;

    // Warm build primes fs caches, then the measured warm build.
    build_index(root, true)?;
    let t0 = Instant::now();
    let stats = build_index(root, true)?;
    let t_build = t0.elapsed();

    ensure!(
        stats.file_count >= FILES_PER_LANG * 12,
        "expected {} files, indexed {}",
        FILES_PER_LANG * 12,
        stats.file_count
    );
    ensure!(stats.contract_count > 0, "no contracts extracted");

    // E: re-parse each file the way parse_one_file does and time the
    // extractor alone. Averaged over rounds for a stable number.
    let files = collect_source_files(root)?;
    ensure!(!files.is_empty(), "no source files found");

    let mut per_lang: HashMap<Lang, Vec<Duration>> = HashMap::new();
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

    let report = render_report(
        stats.contract_count,
        total_contracts,
        e_avg,
        t_build,
        ratio,
        &per_lang,
    );
    print!("{report}");

    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/contracts-results.md");
    fs::write(&out, report)?;
    println!("written to {}", out.display());
    Ok(())
}

fn render_report(
    indexed_contracts: usize,
    extracted_contracts: usize,
    e: Duration,
    t_build: Duration,
    ratio: f64,
    per_lang: &HashMap<Lang, Vec<Duration>>,
) -> String {
    let mut out = String::new();
    out.push_str("# TASK-082 contract extraction build-cost results\n\n");
    out.push_str("Synthetic repo: 12 languages x 25 files, ordinary symbols plus 2-5\n");
    out.push_str("framework idioms per file. `cargo bench --bench contracts`.\n\n");
    out.push_str("| metric | value |\n|---|---|\n");
    out.push_str(&format!("| files indexed | {} |\n", FILES_PER_LANG * 12));
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
    Ok(())
}

// Each generator emits ordinary code plus 2-5 contract sites; numbers vary
// per file so paths differ and no accidental dedup masks the work.

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
