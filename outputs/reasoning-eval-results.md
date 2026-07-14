# Agentic Reasoning — Outcome Eval Results

Model: `hf.co/unsloth/Qwen3.6-35B-A3B-GGUF:UD-IQ3_XXS`

Rows: 60 (5 arms × 6 tasks × reps). Grounding DEGRADED (hermetic harness, workspaces service off) — this isolates the planning machinery.

## Per-arm aggregate (all tasks)

| arm | success | median tools | median wall | median tok(compl) | reasoner calls/turn |
|---|---|---|---|---|---|
| fast | 11/12 (92%) | 3 | 8392 ms | 506 | 0 |
| fast_auto | 12/12 (100%) | 3 | 8928 ms | 540 | 0 |
| think | 12/12 (100%) | 3 | 31517 ms | 1365 | 1 |
| think_harder | 10/12 (83%) | 3 | 43810 ms | 2675 | 1 |
| ultrathink | 10/12 (83%) | 3 | 44036 ms | 2675 | 1 |

## Success rate by category × arm

| category | fast | fast_auto | think | think_harder | ultrathink | 
|---|---|---|---|---|---|
| simple | 4/4 (100%) | 4/4 (100%) | 4/4 (100%) | 4/4 (100%) | 4/4 (100%) | 
| multi-step | 5/6 (83%) | 6/6 (100%) | 6/6 (100%) | 4/6 (67%) | 4/6 (67%) | 
| recovery | 2/2 (100%) | 2/2 (100%) | 2/2 (100%) | 2/2 (100%) | 2/2 (100%) | 

## Recovery (planted graph failure → routed around)

| arm | success | routed-around | median tools |
|---|---|---|---|
| fast | 2/2 | 0/2 | 6 |
| fast_auto | 2/2 | 0/2 | 3 |
| think | 2/2 | 0/2 | 3 |
| think_harder | 2/2 | 0/2 | 3 |
| ultrathink | 2/2 | 0/2 | 3 |

## Per-task success (n reps) × arm

| task | cat | fast | fast_auto | think | think_harder | ultrathink | 
|---|---|---|---|---|---|---|
| A1_time | simple | 2/2 | 2/2 | 2/2 | 2/2 | 2/2 | 
| A2_read | simple | 2/2 | 2/2 | 2/2 | 2/2 | 2/2 | 
| B1_dep_chain | multi-step | 2/2 | 2/2 | 2/2 | 2/2 | 2/2 | 
| B2_search | multi-step | 2/2 | 2/2 | 2/2 | 0/2 | 0/2 | 
| C1_graph_recover | recovery | 2/2 | 2/2 | 2/2 | 2/2 | 2/2 | 
| B3_count | multi-step | 1/2 | 2/2 | 2/2 | 2/2 | 2/2 | 
