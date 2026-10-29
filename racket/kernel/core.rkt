#lang racket/base
;; Kernel state: userspace registry, knowledge, journal replay, and the gate.
;; Every state change is an event: emit! appends it to the journal and applies
;; it with apply-event!, the same function replay uses, so a fresh process
;; rebuilds exactly the state the previous one had.
(require racket/list racket/string racket/file racket/runtime-path json
         "util.rkt" "journal.rkt" "sandbox.rkt")
(provide (struct-out kernel) (struct-out def) (struct-out lesson)
         open-kernel emit! get-def def-record live-status
         propose! rollback! use-def! record-use!
         select-lesson-ids compose-supplemental live-trial-notes userspace-tools
         userspace-tool? add-lesson! score-lessons! active-lessons recall-text
         defs-text show-def-text knowledge-text recent-tasks-text verdicts-text
         kernel-snapshot kernel-tool-names)

(define-runtime-path seed-dir "../seed")
(define hook-names '("compose_system" "select_lessons"))
;; Must list exactly the names in kernel-tool-specs (tools.rkt, which requires
;; this module); the test suite checks that they agree.
(define kernel-tool-names '("bash" "read_file" "write_file" "list_defs" "show_def"
                            "propose_def" "rollback_def" "remember" "recall"))
(define max-verdicts-in-prompt 30)

;; versions: version -> record (immutable hasheq). live/kept: version or #f.
(struct def (name kind versions [live #:mutable] [kept #:mutable] [soak-count #:mutable]))
(struct lesson (id text [score #:mutable] [retired? #:mutable]))
;; defs: name -> def. lessons: id -> lesson. evcache: name -> (version . evaluator)
;; for the version that was live when last used (see live-evaluator).
(struct kernel (dir journal provider soak reflect? defs lessons
                    [verdicts #:mutable] [tasks #:mutable] evcache))

(define (open-kernel dir #:provider [provider #f] #:soak [soak 3] #:reflect? [reflect? #t])
  (define d (simplify-path (path->complete-path dir)))
  (make-directory* d)
  (define k (kernel d (build-path d "journal.jsonl") provider soak reflect?
                    (make-hash) (make-hasheqv) '() '() (make-hash)))
  (define events (journal-read (kernel-journal k)))
  (for ([ev (in-list events)]) (apply-event! k ev))
  (when (null? events) (install-seeds! k))
  k)

(define (emit! k ev)
  (apply-event! k (journal-append! (kernel-journal k) ev)))

;; ---------------------------------------------------------------- replay

(define (apply-event! k ev)
  (define (f key [default #f]) (hash-ref ev key default))
  (define name (f 'name))
  (define d (and (string? name) (get-def k name)))
  (define v (f 'version))
  (case (f 'type)
    [("def_proposed")
     (define dd (or d (let ([new (def name (f 'kind) (make-hasheqv) #f #f 0)])
                        (hash-set! (kernel-defs k) name new)
                        new)))
     (hash-set! (def-versions dd) v
                (hasheq 'name name 'kind (f 'kind) 'version v
                        'source (f 'source "") 'description (f 'description "")
                        'tests (f 'tests '()) 'input_schema (f 'input_schema 'null)
                        'rollback_of (f 'rollback_of 'null) 'status "proposed"))]
    [("def_rejected") (when d (set-status! d v "rejected" (f 'error)))]
    [("def_trial")
     (when d (gate-decision! d v "trial" #f #:live v))]
    [("def_use")
     (when (and d (eq? (f 'ok) #t) (in-soak? d v))
       (set-def-soak-count! d (add1 (def-soak-count d))))]
    [("def_kept")
     (when d
       (set-def-kept! d v)
       (gate-decision! d v "kept" #f #:live v)
       (add-verdict! k (f 'verdict)))]
    [("def_reverted")
     (when d
       (let ([r (f 'restored)])
         (gate-decision! d v "reverted" (f 'error) #:live (and (exact-integer? r) r)))
       (add-verdict! k (f 'verdict)))]
    [("lesson_added")
     (hash-set! (kernel-lessons k) (f 'id) (lesson (f 'id) (f 'text) 0 #f))]
    [("lesson_scored")
     (let ([l (hash-ref (kernel-lessons k) (f 'id) #f)]) (when l (set-lesson-score! l (f 'score))))]
    [("lesson_retired")
     (let ([l (hash-ref (kernel-lessons k) (f 'id) #f)]) (when l (set-lesson-retired?! l #t)))]
    [("task_finished")
     ;; O(n) append per task; fine at the journal sizes cmh sees.
     (set-kernel-tasks! k (append (kernel-tasks k)
                                  (list (hasheq 'task_id (f 'task_id) 'task (f 'task "")
                                                'success (f 'success) 'summary (f 'summary "")))))]
    [else (void)]))

(define (set-status! d v status err)
  (define rec (hash-ref (def-versions d) v #f))
  (when rec
    (hash-set! (def-versions d) v
               (let ([r (hash-set rec 'status status)])
                 (if (string? err) (hash-set r 'error err) r)))))

;; Record a trial / kept / reverted decision for version v and make `live` the
;; live version. Every such decision restarts the soak count.
(define (gate-decision! d v status err #:live live)
  (set-status! d v status err)
  (set-def-live! d live)
  (set-def-soak-count! d 0))

;; True while version v is live and still on trial (its uses count toward soak).
(define (in-soak? d v)
  (and (eqv? (def-live d) v) (equal? (live-status d) "trial")))

(define (add-verdict! k v)
  (when (string? v) (set-kernel-verdicts! k (append (kernel-verdicts k) (list v)))))

(define (get-def k name) (hash-ref (kernel-defs k) name #f))
(define (def-record d v) (and d v (hash-ref (def-versions d) v #f)))
(define (live-record d) (and d (def-record d (def-live d))))
(define (live-status d) (let ([r (live-record d)]) (and r (hash-ref r 'status))))
(define (max-version d) (if d (apply max 0 (hash-keys (def-versions d))) 0))
(define (sorted-versions d) (sort (hash-keys (def-versions d)) <))
(define (sorted-defs k) (sort (hash-values (kernel-defs k)) string<? #:key def-name))
;; Live version records of one kind ("tool", "note" or "hook"), sorted by name.
(define (live-defs k kind)
  (for*/list ([d (in-list (sorted-defs k))]
              #:when (equal? (def-kind d) kind)
              [rec (in-value (live-record d))]
              #:when rec)
    rec))
(define (lessons-by-id k) (sort (hash-values (kernel-lessons k)) < #:key lesson-id))
(define (max-lesson-id k) (apply max 0 (hash-keys (kernel-lessons k))))

;; ---------------------------------------------------------------- seeds

(define (install-seeds! k)
  (define seeds (call-with-input-file (build-path seed-dir "seeds.json") read-json))
  (for ([s (in-list seeds)])
    (define name (hash-ref s 'name))
    (define kind (hash-ref s 'kind))
    (define source (file->string (build-path seed-dir (hash-ref s 'file))))
    (define tests (hash-ref s 'tests '()))
    (define err (validate kind name source tests))
    (when err (error 'cmh "seed ~a failed validation: ~a" name err))
    (emit! k (hasheq 'type "def_proposed" 'name name 'kind kind 'version 1 'source source
                     'description (hash-ref s 'description "") 'tests tests
                     'input_schema (hash-ref s 'input_schema 'null) 'rollback_of 'null 'seed #t))
    (emit! k (hasheq 'type "def_kept" 'name name 'version 1 'seed #t))))

;; ---------------------------------------------------------------- the gate

;; Validate in a fresh scratch evaluator. Returns #f or an error string.
(define (validate kind name source tests)
  (with-handlers ([always? error-text])
    (cond
      [(equal? kind "note")
       (when (blank? source) (error "a note's source must be non-empty text"))
       #f]
      [else
       (when (null? tests) (error "tools and hooks need at least one test"))
       (define ev (new-evaluator))
       (dynamic-wind
        void
        (λ ()
          (with-handlers ([always? (λ (e) (error (string-append "source failed to evaluate: "
                                                                (error-text e))))])
            (load-source! ev source))
          (check-entry! ev kind name)
          (for ([t (in-list tests)] [i (in-naturals 1)])
            (define expect (hash-ref t 'expect))
            (define out
              (with-handlers ([always? (λ (e) (error (format "test ~a raised: ~a" i (error-text e))))])
                (test-output ev kind name (hash-ref t 'input))))
            (unless (string-contains? out expect)
              (error (format "test ~a failed: expected ~s as a substring of the output ~s"
                             i expect (truncate-text out 500)))))
          #f)
        (λ () (destroy-evaluator ev)))])))

(define name-rx #px"^[A-Za-z0-9_-]{1,64}$")

;; Models sometimes send structured fields JSON-encoded as strings.
(define (as-json v)
  (if (string? v) (with-handlers ([exn:fail? (λ (_) v)]) (string->jsexpr v)) v))

(define (valid-test? t)
  (and (hash? t) (hash? (hash-ref t 'input #f)) (string? (hash-ref t 'expect #f))))

;; Returns (values ok? message).
(define (propose! k args #:rollback-of [rollback-of #f])
  (define name (hash-ref args 'name #f))
  (define kind (hash-ref args 'kind #f))
  (define source (hash-ref args 'source #f))
  (define description (let ([d (hash-ref args 'description "")]) (if (string? d) d "")))
  (define tests (let ([t (as-json (hash-ref args 'tests '()))]) (if (eq? t 'null) '() t)))
  (define schema (as-json (hash-ref args 'input_schema 'null)))
  (define d (and (string? name) (get-def k name)))
  (define problem
    (cond
      [(not (and (string? name) (regexp-match? name-rx name))) "name must match [A-Za-z0-9_-]{1,64}"]
      [(member name kernel-tool-names) (format "~a is a kernel tool and cannot be redefined" name)]
      [(not (member kind '("tool" "note" "hook"))) "kind must be tool, note, or hook"]
      [(and (equal? kind "hook") (not (member name hook-names)))
       (format "a hook must be named one of: ~a" (string-join hook-names ", "))]
      [(and d (not (equal? (def-kind d) kind))) (format "~a already exists with kind ~a" name (def-kind d))]
      [(not (string? source)) "source must be a string"]
      [(not (and (list? tests) (andmap valid-test? tests)))
       "tests must be a JSON array of {\"input\": {...}, \"expect\": \"...\"}"]
      [(and (equal? kind "tool") (not (or (eq? schema 'null) (hash? schema))))
       "input_schema must be a JSON Schema object"]
      [else #f]))
  (cond
    [problem (values #f (string-append "invalid proposal: " problem))]
    [else
     (define version (add1 (max-version d)))
     (emit! k (hasheq 'type "def_proposed" 'name name 'kind kind 'version version
                      'source source 'description description 'tests tests
                      'input_schema (if (equal? kind "tool")
                                        (if (hash? schema) schema (hasheq 'type "object"))
                                        'null)
                      'rollback_of (or rollback-of 'null)))
     (define err (validate kind name source tests))
     (cond
       [err
        (emit! k (hasheq 'type "def_rejected" 'name name 'version version 'error err))
        (values #f (format "rejected v~a: ~a" version err))]
       [else
        (define d* (get-def k name))
        (when (equal? (live-status d*) "trial")
          (revert! k d* (format "superseded by v~a before its soak finished" version) #f))
        (emit! k (hasheq 'type "def_trial" 'name name 'version version))
        (values #t (format "trial v~a: live now; kept after ~a clean uses, reverted on the first error"
                           version (kernel-soak k)))])]))

;; Reinstall an older version as a new trial version (same source, new number).
(define (rollback! k name version)
  (define d (get-def k name))
  (define rec (def-record d version))
  (cond
    [(not rec) (values #f (format "no version ~a of ~a" version name))]
    [(member (hash-ref rec 'status) '("rejected" "proposed"))
     (values #f (format "~a v~a was never live (~a)" name version (hash-ref rec 'status)))]
    [else
     (propose! k (hasheq 'name name 'kind (hash-ref rec 'kind) 'source (hash-ref rec 'source)
                         'description (hash-ref rec 'description) 'tests (hash-ref rec 'tests)
                         'input_schema (hash-ref rec 'input_schema))
               #:rollback-of version)]))

(define (describe d v)
  (format "def ~a v~a (~s)" (def-name d) v (hash-ref (def-record d v) 'description "")))

(define (revert! k d err input)
  (define v (def-live d))
  (define restored (def-kept d))
  (define verdict (format "~a reverted: ~a~a" (describe d v) (one-line err 300)
                          (if input (format " on input ~a" (json-brief input)) "")))
  (emit! k (hasheq 'type "def_reverted" 'name (def-name d) 'version v 'error err
                   'restored (or restored 'null) 'verdict verdict))
  (trace! "[gate] ~a v~a reverted: ~a -> ~a" (def-name d) v (one-line err 100)
          (if restored (format "v~a live" restored) "removed")))

(define (keep! k d)
  (define v (def-live d))
  (define verdict (format "~a kept after ~a clean uses" (describe d v) (def-soak-count d)))
  (emit! k (hasheq 'type "def_kept" 'name (def-name d) 'version v 'verdict verdict))
  (trace! "[gate] ~a v~a kept" (def-name d) v))

;; Count one use; during soak an error reverts and enough clean uses keep.
(define (record-use! k name version ok? err input)
  (emit! k (hasheq 'type "def_use" 'name name 'version version 'ok ok? 'error (if ok? 'null err)))
  (define d (get-def k name))
  (when (and d (in-soak? d version))
    (cond [(not ok?) (revert! k d err input)]
          [(>= (def-soak-count d) (kernel-soak k)) (keep! k d)])))

;; One cached evaluator per name, for the version used last. When another
;; version is live now, the stale evaluator is killed and a fresh one loaded.
(define (live-evaluator k d rec)
  (define cache (kernel-evcache k))
  (define name (def-name d))
  (define version (hash-ref rec 'version))
  (define cached (hash-ref cache name #f))
  (cond
    [(and cached (eqv? (car cached) version)) (cdr cached)]
    [else
     (when cached
       (destroy-evaluator (cdr cached))
       (hash-remove! cache name))
     (define ev (new-evaluator))
     (with-handlers ([always? (λ (e) (destroy-evaluator ev) (raise e))])
       (load-source! ev (hash-ref rec 'source)))
     (hash-set! cache name (cons version ev))
     ev]))

;; Late binding: look the name up in the registry on every call.
;; Returns (values ok? result-or-error-text).
(define (use-def! k name input)
  (define d (get-def k name))
  (define rec (live-record d))
  (cond
    [(not rec) (values #f (format "no live definition named ~a" name))]
    [else
     (define v (hash-ref rec 'version))
     (define-values (ok? out)
       (with-handlers ([always? (λ (e) (values #f (error-text e)))])
         (let ([ev (live-evaluator k d rec)])
           (values #t (invoke-entry ev (def-kind d) name input)))))
     (record-use! k name v ok? (if ok? #f out) input)
     (values ok? out)]))

(define (userspace-tool? k name)
  (for/or ([rec (in-list (live-defs k "tool"))])
    (equal? (hash-ref rec 'name) name)))

;; Tool specs for the live userspace tools, in the shape the model sees.
(define (userspace-tools k)
  (for/list ([rec (in-list (live-defs k "tool"))])
    (define name (hash-ref rec 'name))
    (hasheq 'name name
            'description (let ([s (hash-ref rec 'description "")])
                           (if (string=? s "") (format "userspace tool ~a" name) s))
            'input_schema (let ([s (hash-ref rec 'input_schema 'null)])
                            (if (hash? s) s (hasheq 'type "object"))))))

;; (name . version) of each note still on trial.
(define (live-trial-notes k)
  (for/list ([rec (in-list (live-defs k "note"))]
             #:when (equal? (hash-ref rec 'status) "trial"))
    (cons (hash-ref rec 'name) (hash-ref rec 'version))))

;; ---------------------------------------------------------------- hooks

;; Call a hook by name; on failure (which reverts a trial) try the now-live
;; version once more, then fall back to the kernel's built-in behavior.
(define (hook-call k name input fallback)
  (let loop ([tries 0])
    (cond
      [(and (< tries 2) (live-record (get-def k name)))
       (define-values (ok? out) (use-def! k name input))
       (cond [ok? out]
             [else (trace! "[hook] ~a failed: ~a" name (one-line out 160))
                   (loop (add1 tries))])]
      [else (fallback)])))

(define (lesson->jsexpr l) (hasheq 'id (lesson-id l) 'text (lesson-text l) 'score (lesson-score l)))

(define (select-lesson-ids k task)
  (define active (active-lessons k))
  (define ids (hook-call k "select_lessons"
                         (hasheq 'task task 'lessons (map lesson->jsexpr active))
                         (λ () (map lesson-id (take-up-to active 3)))))
  (define active-ids (map lesson-id active))
  (remove-duplicates (filter (λ (i) (memv i active-ids)) ids)))

(define (bullet s) (string-append "- " s))
(define (bullets items)
  (apply string-append (map (λ (s) (string-append (bullet s) "\n")) items)))

;; Fallback when no compose_system hook is live. It deliberately mirrors
;; seed/compose_system.rktl: seeds run sandboxed, so they cannot share code.
(define (default-compose notes lessons)
  (string-append
   (if (null? notes) "" (string-append "## Notes\n" (string-join notes "\n\n") "\n"))
   (if (and (pair? notes) (pair? lessons)) "\n" "")
   (if (null? lessons) "" (string-append "## Lessons\n" (bullets lessons)))))

(define (live-notes k)
  (for/list ([rec (in-list (live-defs k "note"))]) (hash-ref rec 'source)))

;; Supplemental system prompt: compose_system output, then the verdicts,
;; which always reach the model (they bypass select_lessons and the hook).
(define (compose-supplemental k lesson-ids)
  (define notes (live-notes k))
  (define lessons (for/list ([i (in-list lesson-ids)]) (lesson-text (hash-ref (kernel-lessons k) i))))
  (define body (hook-call k "compose_system" (hasheq 'notes notes 'lessons lessons)
                          (λ () (default-compose notes lessons))))
  (define recent (take-right-up-to (kernel-verdicts k) max-verdicts-in-prompt))
  (string-trim
   (string-append body
                  (if (null? recent)
                      ""
                      (string-append "\n\n## Verdicts on earlier userspace changes\n" (bullets recent))))))

;; ---------------------------------------------------------------- knowledge

(define (active-lessons k)
  (sort (filter (λ (l) (not (lesson-retired? l))) (hash-values (kernel-lessons k)))
        (λ (a b) (or (> (lesson-score a) (lesson-score b))
                     (and (= (lesson-score a) (lesson-score b)) (< (lesson-id a) (lesson-id b)))))))

(define (norm-text s) (string-downcase (string-normalize-spaces s)))

;; Returns (values id-or-#f message).
(define (add-lesson! k text source)
  (define t (string-normalize-spaces (if (string? text) text "")))
  (cond
    [(string=? t "") (values #f "empty lesson")]
    [(for/or ([l (in-hash-values (kernel-lessons k))]) (string=? (norm-text (lesson-text l)) (norm-text t)))
     (values #f "already known (duplicate of an existing lesson)")]
    [else
     (define id (add1 (max-lesson-id k)))
     (emit! k (hasheq 'type "lesson_added" 'id id 'text t 'source source))
     (values id (format "lesson #~a added" id))]))

(define (score-lessons! k ids success?)
  (for ([id (in-list ids)])
    (define l (hash-ref (kernel-lessons k) id #f))
    (when (and l (not (lesson-retired? l)))
      (define delta (if success? 1 -1))
      (define new (+ (lesson-score l) delta))
      (emit! k (hasheq 'type "lesson_scored" 'id id 'delta delta 'score new))
      (when (<= new -3)
        (emit! k (hasheq 'type "lesson_retired" 'id id))
        (trace! "[lesson] #~a retired at score ~a" id new)))))

(define (recall-text k query)
  (define qw (content-words (if (string? query) query "")))
  (define (hit? s) (for/or ([w (in-list (content-words s))]) (and (member w qw) #t)))
  (define ls (filter (λ (l) (hit? (lesson-text l))) (active-lessons k)))
  (define vs (filter hit? (reverse (kernel-verdicts k))))
  (if (and (null? ls) (null? vs))
      "no matching lessons or verdicts"
      (string-join
       (append (for/list ([l ls]) (format "lesson #~a [score ~a] ~a" (lesson-id l) (lesson-score l) (lesson-text l)))
               (for/list ([v vs]) (string-append "verdict: " v)))
       "\n")))

;; ---------------------------------------------------------------- views

(define (defs-text k)
  (define lines
    (for/list ([d (in-list (sorted-defs k))])
      (define rec (live-record d))
      (cond
        [rec
         (define status (hash-ref rec 'status))
         (format "~a  ~a  v~a  ~a~a  ~a" (def-name d) (def-kind d) (def-live d) status
                 (if (equal? status "trial") (format " (soak ~a/~a)" (def-soak-count d) (kernel-soak k)) "")
                 (hash-ref rec 'description ""))]
        [else
         (define v (max-version d))
         (format "~a  ~a  -  none live (latest v~a ~a)  ~a" (def-name d) (def-kind d) v
                 (hash-ref (def-record d v) 'status) (hash-ref (def-record d v) 'description ""))])))
  (if (null? lines) "(no definitions)" (string-join lines "\n")))

(define (show-def-text k name [version #f])
  (define d (get-def k name))
  (define v (or version (and d (def-live d)) (and d (max-version d))))
  (define rec (def-record d v))
  (cond
    [(not rec) (format "no definition ~a~a" name (if version (format " v~a" version) ""))]
    [else
     (string-append
      (format "~a v~a (~a, ~a)~a\n" name v (hash-ref rec 'kind) (hash-ref rec 'status)
              (if (eqv? v (def-live d)) " [live]" ""))
      (format "versions: ~a\n"
              (string-join (for/list ([i (in-list (sorted-versions d))])
                             (format "v~a ~a" i (hash-ref (def-record d i) 'status)))
                           ", "))
      (format "description: ~a\n" (hash-ref rec 'description ""))
      (if (hash-ref rec 'error #f) (format "error: ~a\n" (hash-ref rec 'error)) "")
      (if (hash? (hash-ref rec 'input_schema #f))
          (format "input_schema: ~a\n" (jsexpr->string (hash-ref rec 'input_schema)))
          "")
      (format "tests: ~a\n" (jsexpr->string (hash-ref rec 'tests '())))
      "source:\n" (hash-ref rec 'source ""))]))

(define (verdicts-text k)
  (if (null? (kernel-verdicts k))
      "(none)"
      (string-join (map bullet (kernel-verdicts k)) "\n")))

(define (knowledge-text k)
  (define ls (lessons-by-id k))
  (string-append
   "Lessons:\n"
   (if (null? ls)
       "(none)"
       (string-join (for/list ([l ls])
                      (bullet (format "#~a [score ~a~a] ~a" (lesson-id l) (lesson-score l)
                                      (if (lesson-retired? l) ", retired" "") (lesson-text l))))
                    "\n"))
   "\nVerdicts:\n" (verdicts-text k)))

(define (recent-tasks-text k [n 10])
  (define ts (kernel-tasks k))
  (if (null? ts)
      "(none)"
      (string-join
       (for/list ([t (take-right-up-to ts n)])
         (format "- #~a ~a: ~a -> ~a" (hash-ref t 'task_id) (if (hash-ref t 'success) "succeeded" "failed")
                 (one-line (hash-ref t 'task) 120) (one-line (hash-ref t 'summary) 160)))
       "\n")))

;; Comparable summary of all replayable state (used by the persistence test).
(define (kernel-snapshot k)
  (list
   (for/list ([d (in-list (sorted-defs k))])
     (list (def-name d) (def-kind d) (def-live d) (def-kept d) (def-soak-count d)
           (for/list ([v (in-list (sorted-versions d))])
             (cons v (hash-ref (def-record d v) 'status)))))
   (for/list ([l (in-list (lessons-by-id k))])
     (list (lesson-id l) (lesson-text l) (lesson-score l) (lesson-retired? l)))
   (kernel-verdicts k)
   (kernel-tasks k)))
