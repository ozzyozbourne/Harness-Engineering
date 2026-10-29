Same 15 suites and 2 trials as the default run, through OpenRouter. Raw logs are not kept in git.

# Eval results: z-ai/glm-5.3-flash (2026-10-29)

Model `z-ai/glm-5.3-flash`, 2 trial(s), baseline `prime`, pass bar = 80% of the baseline's overall pass rate.

| Harness | Overall | vs prime | Coding | Learning transfer | Median s/episode | Tokens | API errors | Meets bar |
|---|---|---|---|---|---|---|---|---|
| prime | 79% | 100% | 96% | 50% | 6.9 | 2,548,442 | 0 | yes |
| clojure | 92% | 116% | 96% | 83% | 24.6 | 880,669 | 0 | yes |
| lean | 92% | 116% | 96% | 83% | 21.0 | 1,204,741 | 0 | yes |
| racket | 94% | 118% | 100% | 83% | 25.0 | 2,320,845 | 0 | yes |
| rust | 90% | 113% | 92% | 83% | 23.8 | 837,109 | 0 | yes |
