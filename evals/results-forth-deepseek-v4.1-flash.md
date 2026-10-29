Forth-in-WASM flavor with the forth_eval scratchpad tool, against Prime Agent in the same run: same 15 suites and 2 trials, through OpenCode Go. The run was interrupted once by an out-of-memory kill of the whole service (traced to a 5.5 GB process started inside an episode) and resumed; interrupted episodes were rerun. Raw logs are not kept in git.

# Eval results: forth (with forth_eval) vs prime, deepseek-v4.1-flash (2026-10-29)

Model `deepseek-v4.1-flash`, 2 trial(s), baseline `prime`, pass bar = 80% of the baseline's overall pass rate.

| Harness | Overall | vs prime | Coding | Learning transfer | Median s/episode | Tokens | API errors | Meets bar |
|---|---|---|---|---|---|---|---|---|
| prime | 88% | 100% | 100% | 67% | 16.4 | 2,671,217 | 0 | yes |
| forth | 100% | 114% | 100% | 100% | 29.5 | 3,666,810 | 0 | yes |
