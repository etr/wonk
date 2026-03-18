#!/usr/bin/env bash
#
# Token Savings Benchmark for wonk
#
# Compares wonk search output against raw rg for identical queries across
# real open-source repositories. Produces markdown tables showing token
# reduction, definition ranking quality, deduplication stats, and budget
# effectiveness.
#
# Usage: cd bench && ./token_bench.sh
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$SCRIPT_DIR/repos"
RESULTS_DIR="$SCRIPT_DIR/results"
WONK="${WONK:-$(cd "$SCRIPT_DIR/.." && pwd)/target/release/wonk}"
RG="${RG:-rg}"

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

REPO_NAMES=(ripgrep tokio httpx pydantic fastify)
REPO_URLS=(
    "https://github.com/BurntSushi/ripgrep.git"
    "https://github.com/tokio-rs/tokio.git"
    "https://github.com/encode/httpx.git"
    "https://github.com/pydantic/pydantic.git"
    "https://github.com/fastify/fastify.git"
)
REPO_LANGS=("Rust" "Rust" "Python" "Python" "JavaScript")

# Queries per repo (space-separated strings, 5 per repo)
QUERIES_ripgrep="search match regex parse printer"
QUERIES_tokio="spawn runtime task poll waker"
QUERIES_httpx="Client transport request response redirect"
QUERIES_pydantic="BaseModel validator field schema serializer"
QUERIES_fastify="register route handler plugin hooks"

BUDGETS=(500 1000 2000 4000)

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

log() { printf "\033[1;34m==> %s\033[0m\n" "$*" >&2; }
warn() { printf "\033[1;33mWARN: %s\033[0m\n" "$*" >&2; }
err()  { printf "\033[1;31mERROR: %s\033[0m\n" "$*" >&2; exit 1; }

check_prereqs() {
    command -v "$WONK" >/dev/null 2>&1 || err "wonk not found at $WONK — build with 'cargo build --release'"
    command -v "$RG" >/dev/null 2>&1   || err "rg (ripgrep) not found on PATH"
    command -v git >/dev/null 2>&1     || err "git not found on PATH"
}

get_queries() {
    local repo_name="$1"
    local var="QUERIES_${repo_name}"
    echo "${!var}"
}

# Measure approximate token count from a file: ceil(bytes / 4)
measure_tokens() {
    local file="$1"
    local bytes
    bytes=$(wc -c < "$file")
    echo $(( (bytes + 3) / 4 ))
}

count_lines() {
    local file="$1"
    wc -l < "$file" | tr -d ' '
}

# ---------------------------------------------------------------------------
# Core functions
# ---------------------------------------------------------------------------

clone_repos() {
    log "Cloning repositories..."
    mkdir -p "$REPO_DIR"
    for i in "${!REPO_NAMES[@]}"; do
        local name="${REPO_NAMES[$i]}"
        local url="${REPO_URLS[$i]}"
        local dest="$REPO_DIR/$name"
        if [[ -d "$dest/.git" ]]; then
            log "  $name — already cloned, skipping"
        else
            log "  $name — cloning from $url"
            git clone --depth 1 --single-branch "$url" "$dest" 2>&1 | sed 's/^/    /'
        fi
    done
}

index_repos() {
    log "Indexing repositories with wonk..."
    for name in "${REPO_NAMES[@]}"; do
        local dest="$REPO_DIR/$name"
        log "  Indexing $name..."
        (cd "$dest" && "$WONK" init -q 2>&1 | sed 's/^/    /')
    done
}

# Run rg from inside repo dir so paths are relative (like wonk)
# Output format: file:line:content
run_rg() {
    local repo_dir="$1" query="$2" output_file="$3"
    (cd "$repo_dir" && "$RG" --no-heading --line-number "$query" . \
        > "$output_file" 2>/dev/null) || true
}

# Run rg with JSON output (structured baseline for comparing against wonk json/toon)
run_rg_json() {
    local repo_dir="$1" query="$2" output_file="$3"
    (cd "$repo_dir" && "$RG" --json "$query" . \
        > "$output_file" 2>/dev/null) || true
}

# Run wonk search from inside repo dir
# Output format: file:line:content (to stdout), category headers to stderr
# Note: we do NOT use -q because it suppresses category headers we need
run_wonk() {
    local repo_dir="$1" query="$2" output_file="$3"
    (cd "$repo_dir" && "$WONK" search "$query" \
        > "$output_file" 2>"${output_file}.stderr") || true
}

# Run wonk search with json output format
run_wonk_json() {
    local repo_dir="$1" query="$2" output_file="$3"
    (cd "$repo_dir" && "$WONK" search --format json "$query" \
        > "$output_file" 2>"${output_file}.stderr") || true
}

# Run wonk search with toon output format
run_wonk_toon() {
    local repo_dir="$1" query="$2" output_file="$3"
    (cd "$repo_dir" && "$WONK" search --format toon "$query" \
        > "$output_file" 2>"${output_file}.stderr") || true
}

# Run wonk search with a budget
run_wonk_budget() {
    local repo_dir="$1" query="$2" budget="$3" output_file="$4"
    (cd "$repo_dir" && "$WONK" search --budget "$budget" "$query" \
        > "$output_file" 2>"${output_file}.stderr") || true
}

# Run wonk search with a budget and json output format
run_wonk_json_budget() {
    local repo_dir="$1" query="$2" budget="$3" output_file="$4"
    (cd "$repo_dir" && "$WONK" search --format json --budget "$budget" "$query" \
        > "$output_file" 2>"${output_file}.stderr") || true
}

# Run wonk search with a budget and toon output format
run_wonk_toon_budget() {
    local repo_dir="$1" query="$2" budget="$3" output_file="$4"
    (cd "$repo_dir" && "$WONK" search --format toon --budget "$budget" "$query" \
        > "$output_file" 2>"${output_file}.stderr") || true
}

# Check if wonk found definitions (header appears in stderr)
has_definitions() {
    local stderr_file="$1"
    [[ -f "$stderr_file" ]] || { echo 0; return; }
    if grep -q -- '-- definitions --' "$stderr_file" 2>/dev/null; then
        echo 1
    else
        echo 0
    fi
}

# Count dedup annotations in wonk output: "(+N other location"
count_dedup() {
    local file="$1"
    local count
    # grep -c outputs the count even on no-match (outputs "0" with exit code 1)
    count=$(grep -c '(+[0-9]* other location' "$file" 2>/dev/null) || true
    echo "${count:-0}"
}

# What percentage of rg output fits in a given token budget?
rg_budget_fit_pct() {
    local rg_file="$1" budget_tokens="$2"
    local budget_bytes=$(( budget_tokens * 4 ))
    local rg_bytes
    rg_bytes=$(wc -c < "$rg_file")
    if (( rg_bytes == 0 )); then
        echo 100
    elif (( rg_bytes <= budget_bytes )); then
        echo 100
    else
        echo $(( budget_bytes * 100 / rg_bytes ))
    fi
}

# Extract definition file:line: pairs from wonk sym JSON output.
# The trailing colon prevents "file:4:" matching "file:40:".
extract_def_pairs() {
    local repo_dir="$1" query="$2" output_file="$3"
    (cd "$repo_dir" && "$WONK" sym "$query" --format json 2>/dev/null) \
        | grep '"file"' \
        | sed 's/.*"file":"\([^"]*\)".*"line":\([0-9]*\).*/\1:\2:/' \
        > "$output_file" || true
}

# Count how many definition file:line: pairs appear in a search output file.
count_def_hits() {
    local search_file="$1" def_pairs_file="$2"
    if [[ ! -s "$def_pairs_file" ]]; then
        echo 0; return
    fi
    local count
    count=$(grep -cFf "$def_pairs_file" "$search_file" 2>/dev/null) || true
    echo "${count:-0}"
}

# ---------------------------------------------------------------------------
# Main benchmark loop
# ---------------------------------------------------------------------------

run_benchmark() {
    log "Running benchmarks..."
    mkdir -p "$RESULTS_DIR"

    local csv="$RESULTS_DIR/results.csv"
    echo "repo,lang,query,rg_tokens,rg_json_tokens,wonk_tokens,json_tokens,toon_tokens,rg_lines,wonk_lines,json_lines,toon_lines,reduction_pct,json_reduction_pct,toon_reduction_pct,json_vs_rgjson_pct,toon_vs_rgjson_pct,has_defs,dedup_count" > "$csv"

    local budget_csv="$RESULTS_DIR/budget.csv"
    echo "repo,query,budget,wonk_tokens,wonk_lines,json_tokens,json_lines,toon_tokens,toon_lines,rg_fit_pct,wonk_def_recall,json_def_recall,toon_def_recall,rg_def_recall,rg_json_def_recall,total_defs" > "$budget_csv"

    for i in "${!REPO_NAMES[@]}"; do
        local name="${REPO_NAMES[$i]}"
        local lang="${REPO_LANGS[$i]}"
        local repo_dir="$REPO_DIR/$name"
        local queries
        queries=$(get_queries "$name")

        log "  Benchmarking $name ($lang)..."

        for query in $queries; do
            local rg_out="$RESULTS_DIR/${name}_${query}_rg.txt"
            local rg_json_out="$RESULTS_DIR/${name}_${query}_rg_json.txt"
            local wonk_out="$RESULTS_DIR/${name}_${query}_wonk.txt"
            local json_out="$RESULTS_DIR/${name}_${query}_json.txt"
            local toon_out="$RESULTS_DIR/${name}_${query}_toon.txt"

            run_rg "$repo_dir" "$query" "$rg_out"
            run_rg_json "$repo_dir" "$query" "$rg_json_out"
            run_wonk "$repo_dir" "$query" "$wonk_out"
            run_wonk_json "$repo_dir" "$query" "$json_out"
            run_wonk_toon "$repo_dir" "$query" "$toon_out"

            local rg_tokens rg_json_tokens wonk_tokens json_tokens toon_tokens rg_lines wonk_lines json_lines toon_lines
            rg_tokens=$(measure_tokens "$rg_out")
            rg_json_tokens=$(measure_tokens "$rg_json_out")
            wonk_tokens=$(measure_tokens "$wonk_out")
            json_tokens=$(measure_tokens "$json_out")
            toon_tokens=$(measure_tokens "$toon_out")
            rg_lines=$(count_lines "$rg_out")
            wonk_lines=$(count_lines "$wonk_out")
            json_lines=$(count_lines "$json_out")
            toon_lines=$(count_lines "$toon_out")

            # Reduction vs plain rg
            local reduction_pct=0 json_reduction_pct=0 toon_reduction_pct=0
            if (( rg_tokens > 0 )); then
                reduction_pct=$(( (rg_tokens - wonk_tokens) * 100 / rg_tokens ))
                json_reduction_pct=$(( (rg_tokens - json_tokens) * 100 / rg_tokens ))
                toon_reduction_pct=$(( (rg_tokens - toon_tokens) * 100 / rg_tokens ))
            fi

            # Structured format reduction vs rg --json (fairer baseline)
            local json_vs_rgjson_pct=0 toon_vs_rgjson_pct=0
            if (( rg_json_tokens > 0 )); then
                json_vs_rgjson_pct=$(( (rg_json_tokens - json_tokens) * 100 / rg_json_tokens ))
                toon_vs_rgjson_pct=$(( (rg_json_tokens - toon_tokens) * 100 / rg_json_tokens ))
            fi

            local defs
            defs=$(has_definitions "${wonk_out}.stderr")

            local dedup
            dedup=$(count_dedup "$wonk_out")

            echo "$name,$lang,$query,$rg_tokens,$rg_json_tokens,$wonk_tokens,$json_tokens,$toon_tokens,$rg_lines,$wonk_lines,$json_lines,$toon_lines,$reduction_pct,$json_reduction_pct,$toon_reduction_pct,$json_vs_rgjson_pct,$toon_vs_rgjson_pct,$defs,$dedup" >> "$csv"

            # Extract ground-truth definitions for recall measurement
            local def_pairs="$RESULTS_DIR/${name}_${query}_defs.txt"
            extract_def_pairs "$repo_dir" "$query" "$def_pairs"
            local total_defs
            total_defs=$(count_lines "$def_pairs")

            # Budget sweep
            for budget in "${BUDGETS[@]}"; do
                local bout="$RESULTS_DIR/${name}_${query}_wonk_b${budget}.txt"
                local jout="$RESULTS_DIR/${name}_${query}_json_b${budget}.txt"
                local tout="$RESULTS_DIR/${name}_${query}_toon_b${budget}.txt"
                run_wonk_budget "$repo_dir" "$query" "$budget" "$bout"
                run_wonk_json_budget "$repo_dir" "$query" "$budget" "$jout"
                run_wonk_toon_budget "$repo_dir" "$query" "$budget" "$tout"

                local b_tokens b_lines rg_fit
                b_tokens=$(measure_tokens "$bout")
                b_lines=$(count_lines "$bout")
                rg_fit=$(rg_budget_fit_pct "$rg_out" "$budget")

                local j_tokens j_lines
                j_tokens=$(measure_tokens "$jout")
                j_lines=$(count_lines "$jout")

                local t_tokens t_lines
                t_tokens=$(measure_tokens "$tout")
                t_lines=$(count_lines "$tout")

                # Definition recall for wonk budgeted output
                local wonk_hits wonk_recall=0
                wonk_hits=$(count_def_hits "$bout" "$def_pairs")
                if (( total_defs > 0 )); then
                    wonk_recall=$(( wonk_hits * 100 / total_defs ))
                fi

                # Definition recall for json budgeted output
                local json_hits json_recall=0
                json_hits=$(count_def_hits "$jout" "$def_pairs")
                if (( total_defs > 0 )); then
                    json_recall=$(( json_hits * 100 / total_defs ))
                fi

                # Definition recall for toon budgeted output
                local toon_hits toon_recall=0
                toon_hits=$(count_def_hits "$tout" "$def_pairs")
                if (( total_defs > 0 )); then
                    toon_recall=$(( toon_hits * 100 / total_defs ))
                fi

                # Definition recall for rg truncated to the same byte budget
                local budget_bytes=$(( budget * 4 ))
                local rg_trunc="$RESULTS_DIR/${name}_${query}_rg_b${budget}.txt"
                head -c "$budget_bytes" "$rg_out" | sed 's|^\./||' > "$rg_trunc"
                local rg_hits rg_recall=0
                rg_hits=$(count_def_hits "$rg_trunc" "$def_pairs")
                if (( total_defs > 0 )); then
                    rg_recall=$(( rg_hits * 100 / total_defs ))
                fi

                # Definition recall for rg --json truncated to the same byte budget
                local rg_json_trunc="$RESULTS_DIR/${name}_${query}_rg_json_b${budget}.txt"
                head -c "$budget_bytes" "$rg_json_out" > "$rg_json_trunc"
                local rg_json_hits rg_json_recall=0
                rg_json_hits=$(count_def_hits "$rg_json_trunc" "$def_pairs")
                if (( total_defs > 0 )); then
                    rg_json_recall=$(( rg_json_hits * 100 / total_defs ))
                fi

                echo "$name,$query,$budget,$b_tokens,$b_lines,$j_tokens,$j_lines,$t_tokens,$t_lines,$rg_fit,$wonk_recall,$json_recall,$toon_recall,$rg_recall,$rg_json_recall,$total_defs" >> "$budget_csv"
            done
        done
    done
}

# ---------------------------------------------------------------------------
# Output: Markdown tables
# ---------------------------------------------------------------------------

print_results() {
    local csv="$RESULTS_DIR/results.csv"
    local budget_csv="$RESULTS_DIR/budget.csv"

    echo ""
    echo "# Token Savings Benchmark Results"
    echo ""
    echo "_Generated on $(date -u +"%Y-%m-%d %H:%M UTC")_"
    echo ""
    echo "Comparison of \`wonk search\` (grep, json, and toon formats) vs raw \`rg\` across 5 open-source repos."
    echo "Both tools run from the repo root. Grep format produces \`file:line:content\` output;"
    echo "json format produces NDJSON output; toon format produces structured TOON output."
    echo "Wonk adds structure-aware ranking (definitions first), deduplication of re-exports,"
    echo "and token budget support. Structured formats (json/toon) are compared against both"
    echo "plain \`rg\` and \`rg --json\` (the fairer structured baseline)."
    echo ""

    # Per-repo tables
    local current_repo=""
    while IFS=, read -r repo lang query rg_tokens rg_json_tokens wonk_tokens json_tokens toon_tokens rg_lines wonk_lines json_lines toon_lines reduction_pct json_reduction_pct toon_reduction_pct json_vs_rgjson_pct toon_vs_rgjson_pct has_defs dedup_count; do
        [[ "$repo" == "repo" ]] && continue  # skip header

        if [[ "$repo" != "$current_repo" ]]; then
            [[ -n "$current_repo" ]] && echo ""
            echo "## $repo ($lang)"
            echo ""
            echo "| Query | rg | rg json | wonk grep | wonk json | wonk toon | grep vs rg | json vs rg | toon vs rg | json vs rg json | toon vs rg json | Defs | Dedup |"
            echo "|-------|----|---------|-----------|-----------|-----------|------------|------------|------------|-----------------|-----------------|------|-------|"
            current_repo="$repo"
        fi

        local defs_str="no"
        [[ "$has_defs" -gt 0 ]] && defs_str="yes"

        printf "| %-13s | %9s | %9s | %9s | %9s | %9s | %9s%% | %9s%% | %9s%% | %14s%% | %14s%% | %-4s | %5s |\n" \
            "$query" "$rg_tokens" "$rg_json_tokens" "$wonk_tokens" "$json_tokens" "$toon_tokens" "$reduction_pct" "$json_reduction_pct" "$toon_reduction_pct" "$json_vs_rgjson_pct" "$toon_vs_rgjson_pct" "$defs_str" "$dedup_count"
    done < "$csv"

    echo ""

    # Budget sweep table
    echo "## Budget Effectiveness — Definition Recall"
    echo ""
    echo "At each token budget, how many of the query's definitions (from \`wonk sym\`)"
    echo "appear in the output? Wonk ranks definitions first; rg is naively truncated"
    echo "to the same byte count."
    echo ""
    echo "| Budget | Avg grep lines | Avg json lines | Avg toon lines | Grep recall | JSON recall | Toon recall | rg recall | rg json recall |"
    echo "|--------|----------------|----------------|----------------|-------------|-------------|-------------|-----------|----------------|"

    # Track budget=1000 recall for the summary section
    local summary_wonk_recall_1k=0 summary_json_recall_1k=0 summary_toon_recall_1k=0 summary_rg_recall_1k=0 summary_rg_json_recall_1k=0

    for budget in "${BUDGETS[@]}"; do
        local total_wonk_lines=0 total_json_lines=0 total_toon_lines=0 count=0
        local total_wonk_recall=0 total_json_recall=0 total_toon_recall=0 total_rg_recall=0 total_rg_json_recall=0
        while IFS=, read -r repo query b wonk_tokens wonk_lines json_tokens json_lines toon_tokens toon_lines rg_fit_pct wonk_recall json_recall toon_recall rg_recall rg_json_recall total_defs; do
            [[ "$repo" == "repo" ]] && continue
            if [[ "$b" == "$budget" ]]; then
                total_wonk_lines=$((total_wonk_lines + wonk_lines))
                total_json_lines=$((total_json_lines + json_lines))
                total_toon_lines=$((total_toon_lines + toon_lines))
                total_wonk_recall=$((total_wonk_recall + wonk_recall))
                total_json_recall=$((total_json_recall + json_recall))
                total_toon_recall=$((total_toon_recall + toon_recall))
                total_rg_recall=$((total_rg_recall + rg_recall))
                total_rg_json_recall=$((total_rg_json_recall + rg_json_recall))
                count=$((count + 1))
            fi
        done < "$budget_csv"

        if (( count > 0 )); then
            local avg_lines=$((total_wonk_lines / count))
            local avg_json_lines=$((total_json_lines / count))
            local avg_toon_lines=$((total_toon_lines / count))
            local avg_wonk_recall=$((total_wonk_recall / count))
            local avg_json_recall=$((total_json_recall / count))
            local avg_toon_recall=$((total_toon_recall / count))
            local avg_rg_recall=$((total_rg_recall / count))
            local avg_rg_json_recall=$((total_rg_json_recall / count))
            printf "| %6d | %14d | %14d | %14d | %10d%% | %10d%% | %10d%% | %8d%% | %13d%% |\n" \
                "$budget" "$avg_lines" "$avg_json_lines" "$avg_toon_lines" "$avg_wonk_recall" "$avg_json_recall" "$avg_toon_recall" "$avg_rg_recall" "$avg_rg_json_recall"
            if [[ "$budget" == "1000" ]]; then
                summary_wonk_recall_1k=$avg_wonk_recall
                summary_json_recall_1k=$avg_json_recall
                summary_toon_recall_1k=$avg_toon_recall
                summary_rg_recall_1k=$avg_rg_recall
                summary_rg_json_recall_1k=$avg_rg_json_recall
            fi
        fi
    done

    echo ""

    # Summary
    echo "## Summary"
    echo ""

    local total_reduction=0 total_json_reduction=0 total_toon_reduction=0 count=0
    local total_json_vs_rgjson=0 total_toon_vs_rgjson=0
    local min_reduction=999 max_reduction=-999
    local min_json_reduction=999 max_json_reduction=-999
    local min_toon_reduction=999 max_toon_reduction=-999
    local min_json_vs_rgjson=999 max_json_vs_rgjson=-999
    local min_toon_vs_rgjson=999 max_toon_vs_rgjson=-999
    local defs_yes=0 total_dedup=0
    local -a reductions=()
    local -a json_reductions=()
    local -a toon_reductions=()
    local -a json_vs_rgjson_reductions=()
    local -a toon_vs_rgjson_reductions=()

    while IFS=, read -r repo lang query rg_tokens rg_json_tokens wonk_tokens json_tokens toon_tokens rg_lines wonk_lines json_lines toon_lines reduction_pct json_reduction_pct toon_reduction_pct json_vs_rgjson_pct toon_vs_rgjson_pct has_defs dedup_count; do
        [[ "$repo" == "repo" ]] && continue
        total_reduction=$((total_reduction + reduction_pct))
        total_json_reduction=$((total_json_reduction + json_reduction_pct))
        total_toon_reduction=$((total_toon_reduction + toon_reduction_pct))
        total_json_vs_rgjson=$((total_json_vs_rgjson + json_vs_rgjson_pct))
        total_toon_vs_rgjson=$((total_toon_vs_rgjson + toon_vs_rgjson_pct))
        reductions+=("$reduction_pct")
        json_reductions+=("$json_reduction_pct")
        toon_reductions+=("$toon_reduction_pct")
        json_vs_rgjson_reductions+=("$json_vs_rgjson_pct")
        toon_vs_rgjson_reductions+=("$toon_vs_rgjson_pct")
        [[ "$reduction_pct" -lt "$min_reduction" ]] && min_reduction=$reduction_pct
        [[ "$reduction_pct" -gt "$max_reduction" ]] && max_reduction=$reduction_pct
        [[ "$json_reduction_pct" -lt "$min_json_reduction" ]] && min_json_reduction=$json_reduction_pct
        [[ "$json_reduction_pct" -gt "$max_json_reduction" ]] && max_json_reduction=$json_reduction_pct
        [[ "$toon_reduction_pct" -lt "$min_toon_reduction" ]] && min_toon_reduction=$toon_reduction_pct
        [[ "$toon_reduction_pct" -gt "$max_toon_reduction" ]] && max_toon_reduction=$toon_reduction_pct
        [[ "$json_vs_rgjson_pct" -lt "$min_json_vs_rgjson" ]] && min_json_vs_rgjson=$json_vs_rgjson_pct
        [[ "$json_vs_rgjson_pct" -gt "$max_json_vs_rgjson" ]] && max_json_vs_rgjson=$json_vs_rgjson_pct
        [[ "$toon_vs_rgjson_pct" -lt "$min_toon_vs_rgjson" ]] && min_toon_vs_rgjson=$toon_vs_rgjson_pct
        [[ "$toon_vs_rgjson_pct" -gt "$max_toon_vs_rgjson" ]] && max_toon_vs_rgjson=$toon_vs_rgjson_pct
        [[ "$has_defs" -gt 0 ]] && defs_yes=$((defs_yes + 1))
        total_dedup=$((total_dedup + dedup_count))
        count=$((count + 1))
    done < "$csv"

    if (( count > 0 )); then
        local avg_reduction=$((total_reduction / count))
        local avg_json_reduction=$((total_json_reduction / count))
        local avg_toon_reduction=$((total_toon_reduction / count))
        local avg_json_vs_rgjson=$((total_json_vs_rgjson / count))
        local avg_toon_vs_rgjson=$((total_toon_vs_rgjson / count))

        # Compute medians by sorting each reductions array
        local mid=$((count / 2))

        local sorted_str
        sorted_str=$(printf '%s\n' "${reductions[@]}" | sort -n)
        local -a sorted_arr
        mapfile -t sorted_arr <<< "$sorted_str"
        local median_reduction="${sorted_arr[$mid]}"

        local sorted_json_str
        sorted_json_str=$(printf '%s\n' "${json_reductions[@]}" | sort -n)
        local -a sorted_json_arr
        mapfile -t sorted_json_arr <<< "$sorted_json_str"
        local median_json_reduction="${sorted_json_arr[$mid]}"

        local sorted_toon_str
        sorted_toon_str=$(printf '%s\n' "${toon_reductions[@]}" | sort -n)
        local -a sorted_toon_arr
        mapfile -t sorted_toon_arr <<< "$sorted_toon_str"
        local median_toon_reduction="${sorted_toon_arr[$mid]}"

        local sorted_json_rgjson_str
        sorted_json_rgjson_str=$(printf '%s\n' "${json_vs_rgjson_reductions[@]}" | sort -n)
        local -a sorted_json_rgjson_arr
        mapfile -t sorted_json_rgjson_arr <<< "$sorted_json_rgjson_str"
        local median_json_vs_rgjson="${sorted_json_rgjson_arr[$mid]}"

        local sorted_toon_rgjson_str
        sorted_toon_rgjson_str=$(printf '%s\n' "${toon_vs_rgjson_reductions[@]}" | sort -n)
        local -a sorted_toon_rgjson_arr
        mapfile -t sorted_toon_rgjson_arr <<< "$sorted_toon_rgjson_str"
        local median_toon_vs_rgjson="${sorted_toon_rgjson_arr[$mid]}"

        local defs_pct=$((defs_yes * 100 / count))
        local avg_dedup=$((total_dedup / count))

        echo "**$count queries across ${#REPO_NAMES[@]} repos:**"
        echo ""
        echo "### Grep format (vs plain rg)"
        echo "- Average token reduction: **${avg_reduction}%**"
        echo "- Median token reduction: **${median_reduction}%**"
        echo "- Min / Max reduction: ${min_reduction}% / ${max_reduction}%"
        echo ""
        echo "### JSON format (vs plain rg)"
        echo "- Average token reduction: **${avg_json_reduction}%**"
        echo "- Median token reduction: **${median_json_reduction}%**"
        echo "- Min / Max reduction: ${min_json_reduction}% / ${max_json_reduction}%"
        echo ""
        echo "### Toon format (vs plain rg)"
        echo "- Average token reduction: **${avg_toon_reduction}%**"
        echo "- Median token reduction: **${median_toon_reduction}%**"
        echo "- Min / Max reduction: ${min_toon_reduction}% / ${max_toon_reduction}%"
        echo ""
        echo "### JSON format (vs rg --json)"
        echo "- Average token reduction: **${avg_json_vs_rgjson}%**"
        echo "- Median token reduction: **${median_json_vs_rgjson}%**"
        echo "- Min / Max reduction: ${min_json_vs_rgjson}% / ${max_json_vs_rgjson}%"
        echo ""
        echo "### Toon format (vs rg --json)"
        echo "- Average token reduction: **${avg_toon_vs_rgjson}%**"
        echo "- Median token reduction: **${median_toon_vs_rgjson}%**"
        echo "- Min / Max reduction: ${min_toon_vs_rgjson}% / ${max_toon_vs_rgjson}%"
        echo ""
        echo "### General"
        echo "- Queries with definitions found: **${defs_pct}%** (${defs_yes}/${count})"
        echo "- Average re-export dedup annotations per query: **${avg_dedup}**"
        echo ""
        echo "**Key insight:** At budget=1000, wonk grep captures ~${summary_wonk_recall_1k}% of definitions,"
        echo "wonk json captures ~${summary_json_recall_1k}%, wonk toon captures ~${summary_toon_recall_1k}%,"
        echo "while truncated rg captures ~${summary_rg_recall_1k}% and truncated rg --json captures ~${summary_rg_json_recall_1k}%."
    fi
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

main() {
    check_prereqs
    clone_repos
    index_repos
    run_benchmark
    print_results | tee "$RESULTS_DIR/report.md"
    log "Done! Full report: $RESULTS_DIR/report.md"
}

main "$@"
