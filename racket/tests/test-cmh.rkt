#lang racket/base
;; The 8 required tests (mock provider, offline), plus provider wire checks.
(require rackunit racket/file racket/port racket/string racket/list racket/tcp racket/runtime-path json
         "../kernel/core.rkt" "../kernel/agent.rkt" "../kernel/provider.rkt"
         "../kernel/tools.rkt" "../kernel/base-prompt.rkt" "../kernel/util.rkt"
         "../kernel/cli.rkt")

(define-runtime-path kernel-core "../kernel/core.rkt")
(define quiet (open-output-nowhere))

(define-syntax-rule (with-state (dir) body ...)
  (let ([dir (make-temporary-directory "cmh-test-~a")])
    (dynamic-wind void
                  (λ () (parameterize ([trace-port quiet]) body ...))
                  (λ () (delete-directory/files dir #:must-exist? #f)))))

(define (wc-source fmt-expr)
  (format "(define (run input) (define n (length (regexp-match* #px\"\\\\S+\" (hash-ref input 'text \"\")))) ~a)"
          fmt-expr))
(define wc-tests (list (hasheq 'input (hasheq 'text "a b") 'expect "2")))
(define (propose-wc k source [tests wc-tests])
  (propose! k (hasheq 'name "word_count" 'kind "tool" 'source source 'description "test version"
                      'tests tests 'input_schema (hasheq 'type "object"))))
(define (use k name input) (let-values ([(ok? out) (use-def! k name input)]) (cons ok? out)))
(define (live-v k name) (def-live (get-def k name)))
(define (status k name) (live-status (get-def k name)))
(define (text-block s) (hasheq 'type "text" 'text s))
(define (tool-use id name input) (hasheq 'type "tool_use" 'id id 'name name 'input input))

(test-case "1 late binding: a new word_count version is used on the next call"
  (with-state (dir)
    (define k (open-kernel dir))
    (check-equal? (use k "word_count" (hasheq 'text "one two three")) '(#t . "3"))
    (define-values (ok? msg) (propose-wc k (wc-source "(format \"words=~a\" n)")))
    (check-true ok? msg)
    (check-equal? (live-v k "word_count") 2)
    (check-equal? (use k "word_count" (hasheq 'text "one two three")) '(#t . "words=3"))
    ;; also through kernel tool dispatch, as the model would call it
    (define-values (err? out) (run-tool k "word_count" (hasheq 'text "x y")))
    (check-false err?)
    (check-equal? out "words=2")))

(test-case "2 gate rejection: eval failure and test failure leave the live version unchanged"
  (with-state (dir)
    (define k (open-kernel dir))
    (define-values (ok1 msg1) (propose-wc k "(define (run input) (+ 1"))
    (check-false ok1)
    (check-regexp-match #rx"rejected v2" msg1)
    (define-values (ok2 msg2) (propose-wc k (wc-source "(number->string (+ 1 n))")))
    (check-false ok2)
    (check-regexp-match #rx"rejected v3: test 1 failed" msg2)
    (define-values (ok3 msg3) (propose-wc k "(define (walk input) \"2\")"))
    (check-false ok3)
    (check-regexp-match #rx"define \\(run input\\)" msg3)
    (check-equal? (live-v k "word_count") 1)
    (check-equal? (status k "word_count") "kept")
    (check-equal? (hash-ref (def-record (get-def k "word_count") 2) 'status) "rejected")
    (check-equal? (use k "word_count" (hasheq 'text "a b c")) '(#t . "3"))))

(test-case "3 soak revert: an error during soak reverts to the kept version with a verdict"
  (with-state (dir)
    (define k (open-kernel dir))
    (define-values (ok? _)
      (propose-wc k "(define (run input) (define t (hash-ref input 'text)) (if (equal? t \"boom\") (error 'wc \"exploded\") (number->string (string-length t))))"
                  (list (hasheq 'input (hasheq 'text "abc") 'expect "3"))))
    (check-true ok?)
    (check-equal? (status k "word_count") "trial")
    (check-equal? (use k "word_count" (hasheq 'text "ab")) '(#t . "2"))
    (define r (use k "word_count" (hasheq 'text "boom")))
    (check-false (car r))
    (check-regexp-match #rx"exploded" (cdr r))
    (check-equal? (live-v k "word_count") 1)
    (check-equal? (status k "word_count") "kept")
    (check-equal? (hash-ref (def-record (get-def k "word_count") 2) 'status) "reverted")
    (check-true (for/or ([v (kernel-verdicts k)])
                  (regexp-match? #rx"^def word_count v2 .*reverted: .*exploded.*on input" v)))
    (check-equal? (use k "word_count" (hasheq 'text "a b")) '(#t . "2"))
    ;; a brand-new definition with no kept version is removed on revert
    (propose! k (hasheq 'name "flaky" 'kind "tool" 'source "(define (run input) (if (hash-ref input 'bad #f) (car '()) \"ok\"))"
                        'description "" 'tests (list (hasheq 'input (hasheq) 'expect "ok"))))
    (check-equal? (status k "flaky") "trial")
    (use k "flaky" (hasheq 'bad #t))
    (check-false (live-v k "flaky"))
    (check-false (member "flaky" (map (λ (t) (hash-ref t 'name)) (tool-specs k))))))

(test-case "4 soak keep: after soak clean uses the version is kept"
  (with-state (dir)
    (define k (open-kernel dir #:soak 3))
    (propose-wc k (wc-source "(format \"n=~a\" n)"))
    (use k "word_count" (hasheq 'text "a"))
    (use k "word_count" (hasheq 'text "a b"))
    (check-equal? (status k "word_count") "trial")
    (check-equal? (def-soak-count (get-def k "word_count")) 2)
    (use k "word_count" (hasheq 'text "a b c"))
    (check-equal? (status k "word_count") "kept")
    (check-equal? (def-kept (get-def k "word_count")) 2)
    (check-true (ormap (λ (v) (regexp-match? #rx"^def word_count v2 .*kept" v)) (kernel-verdicts k)))
    ;; runtime errors from a kept version do not revert it
    (check-false (car (use k "word_count" (hasheq 'text 42))))
    (check-equal? (status k "word_count") "kept")
    (check-equal? (live-v k "word_count") 2)))

(test-case "5 persistence: a fresh kernel replays the same state"
  (with-state (dir)
    (define k (open-kernel dir))
    (propose-wc k (wc-source "(format \"n=~a\" n)"))
    (use k "word_count" (hasheq 'text "a"))                 ; trial, soak 1/3
    (propose! k (hasheq 'name "shout" 'kind "tool" 'source "(define (run input) (if (hash-ref input 'x #f) \"X\" (raise 'nope)))"
                        'description "" 'tests (list (hasheq 'input (hasheq 'x 1) 'expect "X"))))
    (use k "shout" (hasheq))                                 ; reverted -> removed, verdict
    (propose! k (hasheq 'name "style" 'kind "note" 'source "Prefer small diffs." 'description "style note"))
    (add-lesson! k "Run the tests before committing." "model")
    (score-lessons! k '(1) #t)
    (define k2 (open-kernel dir))
    (check-equal? (kernel-snapshot k2) (kernel-snapshot k))
    (check-equal? (def-soak-count (get-def k2 "word_count")) 1)
    (check-equal? (status k2 "word_count") "trial")
    (check-equal? (status k2 "style") "trial")
    (check-false (live-v k2 "shout"))
    (check-equal? (lesson-score (hash-ref (kernel-lessons k2) 1)) 1)
    (check-equal? (defs-text k2) (defs-text k))
    (check-equal? (knowledge-text k2) (knowledge-text k))
    ;; replay keeps working: the next clean uses keep v2 in the new process
    (use k2 "word_count" (hasheq 'text "a"))
    (use k2 "word_count" (hasheq 'text "a"))
    (check-equal? (status k2 "word_count") "kept")
    (check-equal? (status (open-kernel dir) "word_count") "kept")))

(test-case "6 hook redefinition changes the system prompt; the base prompt stays first"
  (with-state (dir)
    (define p (make-mock-provider (list (list (text-block "done")))))
    (define k (open-kernel dir #:provider p #:reflect? #f))
    (add-lesson! k "Always check the parser output." "model")
    (define-values (ok? msg)
      (propose! k (hasheq 'name "compose_system" 'kind "hook"
                          'source "(define (compose-system notes lessons) (apply string-append \"CUSTOM SUPPLEMENT\" (map (lambda (l) (string-append \" | \" l)) lessons)))"
                          'description "custom"
                          'tests (list (hasheq 'input (hasheq 'notes '() 'lessons '("L1")) 'expect "CUSTOM SUPPLEMENT | L1")))))
    (check-true ok? msg)
    (define-values (task-ok? answer) (run-task k "check the parser"))
    (check-true task-ok?)
    (check-equal? answer "done")
    (define sys (hash-ref (first (mock-requests p)) 'system))
    (check-equal? (first sys) base-prompt)
    (check-regexp-match #rx"cmh, a coding agent" (first sys))
    (check-regexp-match #rx"^CUSTOM SUPPLEMENT \\| Always check the parser output\\." (second sys))
    (check-equal? (def-soak-count (get-def k "compose_system")) 1) ; one hook use counted
    ;; a hook that breaks at runtime reverts and the default prompt returns
    (propose! k (hasheq 'name "compose_system" 'kind "hook"
                        'source "(define (compose-system notes lessons) (if (null? lessons) \"ok\" (vector-ref (vector) 3)))"
                        'description "broken"
                        'tests (list (hasheq 'input (hasheq 'notes '() 'lessons '()) 'expect "ok"))))
    (define supp (compose-supplemental k '(1)))
    (check-regexp-match #rx"## Lessons\n- Always check the parser output" supp)
    (check-equal? (live-v k "compose_system") 1)
    (check-regexp-match #rx"compose_system v3 .*reverted" supp)))   ; verdicts always shown

(test-case "7 lesson scoring: +1 on success, -1 on failure, retired at -3"
  (with-state (dir)
    (define (bad-turn) (list (tool-use "t1" "no_such_tool" (hasheq))))
    (define script
      (append (list (list (text-block "ok")))                       ; success
              (append* (for/list ([_ 4]) (list (bad-turn) (list (text-block "gave up")))))
              (list (list (text-block "ok")))))
    (define p (make-mock-provider script))
    (define k (open-kernel dir #:provider p #:reflect? #f))
    (add-lesson! k "Parsers need careful tests." "model")
    (define (lesson1) (hash-ref (kernel-lessons k) 1))
    (define-values (s1 _a) (run-task k "fix parsers"))
    (check-true s1)
    (check-equal? (lesson-score (lesson1)) 1)
    (for ([expected '(0 -1 -2 -3)])
      (define-values (s _) (run-task k "fix parsers"))
      (check-false s)
      (check-equal? (lesson-score (lesson1)) expected))
    (check-true (lesson-retired? (lesson1)))
    (check-equal? (active-lessons k) '())
    (run-task k "fix parsers")
    (check-equal? (lesson-score (lesson1)) -3)                     ; retired: no longer scored
    (check-false (regexp-match? #rx"Parsers need" (string-join (hash-ref (last (mock-requests p)) 'system))))
    (check-true (lesson-retired? (hash-ref (kernel-lessons (open-kernel dir)) 1)))))

(test-case "8 end-to-end mock run: propose a tool and call it within one task"
  (with-state (dir)
    (define script-file (build-path dir "script.json"))
    (define rev-src "(define (run input) (list->string (reverse (string->list (hash-ref input 'text)))))")
    (with-output-to-file script-file
      (λ ()
        (write-json
         (list
          (list (text-block "I'll make a tool.")
                (tool-use "a" "propose_def"
                          (hasheq 'name "reverse_text" 'kind "tool" 'source rev-src
                                  'description "Reverses a string"
                                  'input_schema (hasheq 'type "object" 'properties (hasheq 'text (hasheq 'type "string")))
                                  'tests (list (hasheq 'input (hasheq 'text "abc") 'expect "cba")))))
          (list (tool-use "b" "reverse_text" (hasheq 'text "hello")))
          (list (text-block "olleh"))
          (list (text-block "[\"Build a userspace tool for repeated string work.\"]"))))))
    (define state (path->string (build-path dir "state")))
    (define err (open-output-string))
    (define out
      (with-output-to-string
        (λ () (parameterize ([current-error-port err] [trace-port #f])
                (check-equal? (main (vector "--state" state "--provider" "mock" "--script"
                                            (path->string script-file) "run" "reverse hello"))
                              0)))))
    (check-equal? out "olleh\n")                                   ; stdout: only the answer
    (check-regexp-match #rx"\\[tool\\] propose_def reverse_text -> trial v1" (get-output-string err))
    (check-regexp-match #rx"\\[tool\\] reverse_text .* -> olleh" (get-output-string err))
    (define k (open-kernel state))
    (check-equal? (status k "reverse_text") "trial")
    (check-equal? (def-soak-count (get-def k "reverse_text")) 1)
    (check-regexp-match #rx"Build a userspace tool" (knowledge-text k))
    ;; running out of script is an error and run exits nonzero
    (define empty-script (build-path dir "empty.json"))
    (with-output-to-file empty-script (λ () (write-json '())))
    (check-equal? (parameterize ([current-output-port (open-output-nowhere)] [current-error-port (open-output-nowhere)])
                    (main (vector "--state" state "--provider" "mock" "--script" (path->string empty-script)
                                  "--no-reflect" "run" "x")))
                  1)))

;; ---------------------------------------------------------------- provider wire format

(define (with-env alist thunk)
  (define old (for/list ([p alist]) (cons (car p) (getenv (car p)))))
  (define (set-all! ps) (for ([p ps]) (if (cdr p) (putenv (car p) (cdr p))
                                          (environment-variables-set! (current-environment-variables)
                                                                      (string->bytes/utf-8 (car p)) #f))))
  (dynamic-wind (λ () (set-all! alist)) thunk (λ () (set-all! old))))

(test-case "anthropic: OAuth headers and system blocks; API key; base URL"
  (with-env '(("ANTHROPIC_API_KEY" . #f) ("CLAUDE_CODE_OAUTH_TOKEN" . "tok-test") ("ANTHROPIC_BASE_URL" . #f))
    (λ ()
      (define-values (url headers body) (anthropic-request "m" (list "BASE" "SUPP") '() '()))
      (check-equal? url "https://api.anthropic.com/v1/messages")
      (check-not-false (member "Authorization: Bearer tok-test" headers))
      (check-not-false (member "anthropic-beta: oauth-2025-04-20" headers))
      (check-not-false (member "anthropic-version: 2023-06-01" headers))
      (check-equal? (map (λ (b) (hash-ref b 'text)) (hash-ref body 'system))
                    (list claude-code-identity "BASE" "SUPP"))
      (check-equal? (hash-ref body 'max_tokens) 32000)))
  (with-env '(("ANTHROPIC_API_KEY" . "key-test") ("ANTHROPIC_BASE_URL" . "http://127.0.0.1:9/"))
    (λ ()
      (define-values (url headers body) (anthropic-request "m" (list "BASE") '() '()))
      (check-equal? url "http://127.0.0.1:9/v1/messages")
      (check-not-false (member "x-api-key: key-test" headers))
      (check-false (ormap (λ (h) (string-prefix? h "Authorization")) headers))
      (check-equal? (map (λ (b) (hash-ref b 'text)) (hash-ref body 'system)) '("BASE")))))

(test-case "anthropic: works over a plain http:// ANTHROPIC_BASE_URL"
  (define listener (tcp-listen 0 4 #t "127.0.0.1"))
  (define-values (_a port _b _c) (tcp-addresses listener #t))
  (define seen (box #f))
  (define server
    (thread
     (λ ()
       (define-values (in out) (tcp-accept listener))
       (define head (let loop ([acc '()])
                      (define l (read-line in 'return-linefeed))
                      (if (or (eof-object? l) (string=? l "")) (reverse acc) (loop (cons l acc)))))
       (define len (for/or ([h head]) (let ([m (regexp-match #px"(?i:^content-length: *(\\d+))" h)])
                                        (and m (string->number (cadr m))))))
       (define body (read-bytes len in))
       (set-box! seen (cons (car head) (bytes->jsexpr body)))
       (define resp #"{\"content\":[{\"type\":\"text\",\"text\":\"pong\"}],\"stop_reason\":\"end_turn\"}")
       (write-bytes (bytes-append #"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: "
                                  (string->bytes/utf-8 (number->string (bytes-length resp)))
                                  #"\r\nConnection: close\r\n\r\n" resp) out)
       (close-output-port out) (close-input-port in))))
  (with-env (list (cons "ANTHROPIC_BASE_URL" (format "http://127.0.0.1:~a" port))
                  (cons "ANTHROPIC_API_KEY" "key-test"))
    (λ ()
      (define r (provider-call (make-anthropic-provider "claude-haiku-5-5") (list "BASE") '()
                               (list (hasheq 'role "user" 'content "ping"))))
      (check-equal? (hash-ref r 'stop_reason) "end_turn")
      (define req (unbox seen))
      (check-equal? (car req) "POST /v1/messages HTTP/1.1")
      (check-equal? (hash-ref (cdr req) 'model) "claude-haiku-5-5")))
  (kill-thread server)
  (tcp-close listener))

(test-case "write_file refuses the state dir and kernel source"
  (with-state (dir)
    (define k (open-kernel dir))
    (define-values (e1 m1) (run-tool k "write_file" (hasheq 'path (path->string (build-path dir "journal.jsonl")) 'content "x")))
    (check-regexp-match #rx"refused" m1)
    (define-values (e2 m2) (run-tool k "write_file" (hasheq 'path (path->string kernel-core) 'content "x")))
    (check-regexp-match #rx"refused" m2)
    (define-values (e3 m3) (run-tool k "propose_def" (hasheq 'name "bash" 'kind "tool" 'source "" 'description "")))
    (check-regexp-match #rx"kernel tool" m3)))
