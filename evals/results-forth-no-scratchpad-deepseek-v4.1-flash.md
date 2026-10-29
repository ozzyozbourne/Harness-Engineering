Forth-in-WASM flavor before the forth_eval scratchpad tool, against Prime Agent: same 15 suites and 2 trials, through OpenCode Go. Raw logs are not kept in git.

# Eval results: forth vs prime, deepseek-v4.1-flash (2026-10-29)

Model `deepseek-v4.1-flash`, 2 trial(s), baseline `prime`, pass bar = 80% of the baseline's overall pass rate.

| Harness | Overall | vs prime | Coding | Learning transfer | Median s/episode | Tokens | API errors | Meets bar |
|---|---|---|---|---|---|---|---|---|
| prime | 98% | 100% | 100% | 94% | 12.1 | 2,434,264 | 0 | yes |
| forth | 100% | 102% | 100% | 100% | 22.1 | 7,805,356 | 0 | yes |
