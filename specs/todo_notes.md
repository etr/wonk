Issue often seen on clients:

● plugin:wonk:wonk - wonk_ls (MCP)(path: "src")
  ⎿  Error: result (374,984 characters) exceeds maximum allowed tokens. Output has been saved to /home/etr/.claude/projects/-home-etr-progs-wonk/4478e0a4-7e64-4d57-9ed4-727b91838e34/tool-results/mcp-plugi
     n_wonk_wonk-wonk_ls-1771474681460.txt.
     Format: JSON array with schema: [{type: string, text: string}]

  Remaining minor suggestions (optional, non-blocking):
  - Add TOON format test for SemanticOutput (consistency with other output types)
  - Add integration test for the full wonk ask dispatch path
  - Consider select_nth_unstable partial sort in semantic_search for large indexes
  - Pre-size embedding vec with COUNT(*) query
  - Add query length validation before forwarding to Ollama
  - Update PRD-SEM-REQ-005 wording (--json → --format json)
