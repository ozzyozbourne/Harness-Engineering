Same 15 suites and 2 trials as the default run, through OpenCode Go. Raw logs are not kept in git.

# Eval results: deepseek-v4.1-flash (2026-10-29)

Model `deepseek-v4.1-flash`, 2 trial(s), baseline `prime`, pass bar = 80% of the baseline's overall pass rate.

| Harness | Overall | vs prime | Coding | Learning transfer | Median s/episode | Tokens | API errors | Meets bar |
|---|---|---|---|---|---|---|---|---|
| prime | 98% | 100% | 100% | 94% | 13.6 | 2,740,579 | 0 | yes |
| clojure | 100% | 102% | 100% | 100% | 27.6 | 2,671,200 | 0 | yes |
| lean | 96% | 98% | 100% | 89% | 30.1 | 4,023,027 | 0 | yes |
| racket | 100% | 102% | 100% | 100% | 29.0 | 4,014,249 | 0 | yes |
| rust | 100% | 102% | 100% | 100% | 28.1 | 2,349,490 | 0 | yes |
