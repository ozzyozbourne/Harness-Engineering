#lang racket/base
;; Kernel tools (always present, not redefinable) plus dispatch to userspace
;; tools, which are rebuilt from the live registry on every request.
(require racket/list racket/string racket/port racket/file racket/path racket/runtime-path json
         "util.rkt" "core.rkt" "base-prompt.rkt")
(provide tool-specs run-tool kernel-tool-specs)

(define-runtime-path kernel-src-dir ".")

(define (obj props required) (hasheq 'type "object" 'properties props 'required required))
(define (str d) (hasheq 'type "string" 'description d))
(define (int d) (hasheq 'type "integer" 'description d))

(define propose-description #<<EOF
Propose a new version of a userspace definition. It is validated in a fresh scratch sandbox (the source must evaluate, define the entry point, and pass every test), then goes live immediately as a trial and is kept after its next clean uses, or reverted on the first error. Userspace language: Racket, evaluated in a racket/base sandbox (no #lang line; you may (require racket/string racket/list json ...); no file writes or network; time and memory limits).
Contracts:
- tool: (define (run input) ...) where input is a hasheq with symbol keys (the JSON tool input; JSON null is the symbol 'null). Must return a string. Give an input_schema.
- hook compose_system: (define (compose-system notes lessons) ...) with lists of strings; returns the supplemental system prompt string (the base prompt is always prepended by the kernel).
- hook select_lessons: (define (select-lessons task lessons) ...) where lessons are hasheqs with 'id 'text 'score; returns a list of ids.
- note: source is plain text appended to the supplemental system prompt.
Tests: [{"input": {...}, "expect": "substring of the output"}]. Hooks get their arguments as input ({"notes": [...], "lessons": [...]} or {"task": "...", "lessons": [...]}); select_lessons output is compared JSON-encoded, e.g. "[2,1]". Tools and hooks need at least one test.
EOF
  )

(define kernel-tool-specs
  (list
   (hasheq 'name "bash"
           'description "Run a shell command in the working directory. Returns combined stdout/stderr and the exit code, truncated to 20 KB."
           'input_schema (obj (hasheq 'command (str "Shell command") 'timeout_s (int "Timeout in seconds (default 60)"))
                              '("command")))
   (hasheq 'name "read_file" 'description "Read a file (truncated to 50 KB)."
           'input_schema (obj (hasheq 'path (str "File path")) '("path")))
   (hasheq 'name "write_file"
           'description "Write a file (creating parent directories). Refuses the harness state dir and kernel source."
           'input_schema (obj (hasheq 'path (str "File path") 'content (str "Full file content")) '("path" "content")))
   (hasheq 'name "list_defs" 'description "List userspace definitions: name, kind, live version, status, description."
           'input_schema (obj (hasheq) '()))
   (hasheq 'name "show_def" 'description "Show one version of a definition (default: live) with source and tests."
           'input_schema (obj (hasheq 'name (str "Definition name") 'version (int "Version (optional)")) '("name")))
   (hasheq 'name "propose_def" 'description propose-description
           'input_schema
           (obj (hasheq 'name (str "Definition name ([A-Za-z0-9_-]; hooks: compose_system or select_lessons)")
                        'kind (hasheq 'type "string" 'enum '("tool" "note" "hook"))
                        'source (str "Racket source (or note text)")
                        'description (str "One-line description")
                        'tests (hasheq 'type "array"
                                       'items (obj (hasheq 'input (hasheq 'type "object") 'expect (str "Expected substring"))
                                                   '("input" "expect")))
                        'input_schema (hasheq 'type "object" 'description "JSON Schema for a tool's input (tools only)"))
                '("name" "kind" "source" "description")))
   (hasheq 'name "rollback_def" 'description "Reinstall an older version of a definition as a new trial version."
           'input_schema (obj (hasheq 'name (str "Definition name") 'version (int "Version to reinstall")) '("name" "version")))
   (hasheq 'name "remember" 'description "Record a durable, reusable lesson in knowledge."
           'input_schema (obj (hasheq 'text (str "The lesson")) '("text")))
   (hasheq 'name "recall" 'description "Find lessons and verdicts sharing words with the query, highest score first."
           'input_schema (obj (hasheq 'query (str "Query")) '("query")))))

(define (tool-specs k) (append kernel-tool-specs (userspace-tools k)))

;; ---------------------------------------------------------------- helpers

(define (need-string input key)
  (define v (hash-ref input key #f))
  (unless (string? v) (error (format "missing string field ~a" key)))
  v)

(define (opt-int input key default)
  (define v (hash-ref input key #f))
  (cond [(exact-integer? v) v]
        [(and (real? v) (positive? v)) (inexact->exact (ceiling v))]
        [(and (string? v) (string->number v)) => (λ (n) (if (exact-integer? n) n default))]
        [else default]))

(define (run-bash command timeout-s)
  (define-values (sp out in _err)
    (parameterize ([subprocess-group-enabled #t])
      (subprocess #f #f 'stdout "/bin/sh" "-c" command)))
  (close-output-port in)
  (define buf (open-output-bytes))
  (define reader (thread (λ () (copy-port out buf))))
  (define finished? (sync/timeout timeout-s sp))
  (unless finished? (subprocess-kill sp #t))
  (sync/timeout 5 reader)
  (kill-thread reader)
  (close-input-port out)
  (define text (truncate-text (bytes->string/utf-8 (get-output-bytes buf) #\uFFFD) 20480))
  (if finished?
      (values #f (format "~a\n[exit code ~a]" text (subprocess-status sp)))
      (values #t (format "~a\n[timed out after ~a s]" text timeout-s))))

;; Resolve symlinks on the longest existing prefix of p.
(define (real-path p)
  (define full (simplify-path (path->complete-path p)))
  (let loop ([base full] [rest '()])
    (cond
      [(or (directory-exists? base) (file-exists? base) (link-exists? base))
       (define nb (with-handlers ([exn:fail? (λ (_) base)]) (normalize-path base)))
       (simplify-path (apply build-path nb rest))]
      [else
       (define-values (parent name _) (split-path base))
       (if (path? parent) (loop parent (cons name rest)) full)])))

(define (inside? p dir)
  (define pe (explode-path p))
  (define de (explode-path dir))
  (and (<= (length de) (length pe)) (equal? (take pe (length de)) de)))

(define (write-file-tool k path content)
  (define target (real-path path))
  (cond
    [(inside? target (real-path (kernel-dir k)))
     (values #t "refused: path is inside the harness state dir")]
    [(inside? target (real-path kernel-src-dir))
     (values #t "refused: path is inside the harness kernel source")]
    [(equal? target (real-path base-prompt-file))
     (values #t "refused: the base prompt is immutable")]
    [else
     (make-parent-directory* target)
     (call-with-output-file target #:exists 'truncate/replace (λ (o) (write-string content o)))
     (values #f (format "wrote ~a bytes to ~a" (bytes-length (string->bytes/utf-8 content)) path))]))

;; ---------------------------------------------------------------- dispatch

;; Returns (values error? text).
(define (execute k name input)
  (case name
    [("bash") (run-bash (need-string input 'command) (opt-int input 'timeout_s 60))]
    [("read_file") (values #f (truncate-text (file->string (need-string input 'path)) 51200))]
    [("write_file") (write-file-tool k (need-string input 'path) (need-string input 'content))]
    [("list_defs") (values #f (defs-text k))]
    [("show_def")
     (let ([d (get-def k (need-string input 'name))])
       (values (not d) (show-def-text k (need-string input 'name) (opt-int input 'version #f))))]
    [("propose_def") (let-values ([(ok? msg) (propose! k input)]) (values (not ok?) msg))]
    [("rollback_def")
     (let-values ([(ok? msg) (rollback! k (need-string input 'name) (opt-int input 'version #f))])
       (values (not ok?) msg))]
    [("remember") (let-values ([(id msg) (add-lesson! k (need-string input 'text) "model")])
                    (values #f msg))]
    [("recall") (values #f (recall-text k (need-string input 'query)))]
    [else
     (if (userspace-tool? k name)
         (let-values ([(ok? out) (use-def! k name input)]) (values (not ok?) out))
         (values #t (format "unknown tool ~a" name)))]))

(define (arg-brief name input)
  (define (s key) (let ([v (hash-ref input key #f)]) (if (string? v) (one-line v 80) "")))
  (case name
    [("bash") (s 'command)]
    [("read_file" "write_file") (s 'path)]
    [("propose_def" "show_def" "rollback_def")
     (string-trim (format "~a ~a" (s 'name) (let ([v (hash-ref input 'version #f)]) (if v (format "v~a" v) ""))))]
    [("remember") (s 'text)]
    [("recall") (s 'query)]
    [("list_defs") ""]
    [else (json-brief input 80)]))

(define (result-brief name err? out)
  (cond
    [(member name '("propose_def" "rollback_def"))
     (let ([m (regexp-match #px"^(trial v\\d+|rejected v\\d+: .*|invalid proposal: .*)" out)])
       (one-line (if m (regexp-replace #px"^(trial v\\d+):.*" (cadr m) "\\1") out) 120))]
    [err? (string-append "error: " (one-line out 120))]
    [(equal? name "bash") (let ([m (regexp-match #px"\\[(exit code \\d+)\\]$" out)]) (if m (cadr m) "ok"))]
    [(member name '("read_file" "list_defs" "show_def" "recall")) (format "~a chars" (string-length out))]
    [else (one-line out 80)]))

(define (run-tool k name input)
  (define input* (if (hash? input) input (hasheq)))
  (define-values (err? out)
    (with-handlers ([always? (λ (e) (values #t (string-append "error: " (error-text e))))])
      (execute k name input*)))
  (trace! "[tool] ~a~a -> ~a" name
          (let ([a (arg-brief name input*)]) (if (string=? a "") "" (string-append " " a)))
          (result-brief name err? out))
  (values err? out))
