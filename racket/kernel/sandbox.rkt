#lang racket/base
;; Sandboxed evaluation of userspace Racket source. Every definition version
;; lives in its own racket/sandbox evaluator; validation uses a fresh scratch
;; evaluator that is thrown away. Only plain data crosses the boundary: inputs
;; are quoted jsexprs, results are checked to be strings / lists of ids.
(require racket/sandbox json)
(provide new-evaluator destroy-evaluator load-source! check-entry! invoke-entry test-output)

(define sandbox-time-limit (make-parameter 10))  ; seconds per evaluation
(define sandbox-memory-mb (make-parameter 128))  ; MB per evaluation

(define (new-evaluator)
  (parameterize ([sandbox-output #f]
                 [sandbox-error-output #f]
                 [sandbox-input #f]
                 [sandbox-eval-limits (list (sandbox-time-limit) (sandbox-memory-mb))]
                 [sandbox-memory-limit (* 2 (sandbox-memory-mb))])
    (make-evaluator 'racket/base)))

(define (destroy-evaluator ev) (kill-evaluator ev))

;; Evaluate all top-level forms of the source. A leading #lang line is dropped.
(define (load-source! ev source)
  (ev (regexp-replace #px"^\\s*#lang[^\n]*" source ""))
  (void))

(define (check-string sym result)
  (unless (string? result)
    (error (format "~a must return a string, got ~e" sym result)))
  (string->immutable-string result))

(define (check-lesson-ids _sym result)
  (unless (and (list? result) (andmap exact-integer? result))
    (error (format "select-lessons must return a list of lesson ids, got ~e" result)))
  result)

;; One entry point per kind of definition:
;;   symbol     the procedure the source must define
;;   arity      how many arguments it must accept
;;   signature  shown to the model when the definition is missing
;;   args       input jsexpr -> the argument list for the call
;;   check      (symbol result) -> the checked result, or raises
(struct entry (symbol arity signature args check))

(define tool-entry
  (entry 'run 1 "(define (run input) ...)"
         (λ (input) (list input))
         check-string))

(define select-lessons-entry
  (entry 'select-lessons 2 "(define (select-lessons task lessons) ...)"
         (λ (input) (list (hash-ref input 'task "") (hash-ref input 'lessons '())))
         check-lesson-ids))

(define compose-system-entry
  (entry 'compose-system 2 "(define (compose-system notes lessons) ...)"
         (λ (input) (list (hash-ref input 'notes '()) (hash-ref input 'lessons '())))
         check-string))

(define (entry-for kind name)
  (cond [(equal? kind "tool") tool-entry]
        [(and (equal? kind "hook") (equal? name "select_lessons")) select-lessons-entry]
        [else compose-system-entry]))

(define (check-entry! ev kind name)
  (define e (entry-for kind name))
  (define v (with-handlers ([exn:fail? (λ (_) #f)]) (ev (entry-symbol e))))
  (unless (and (procedure? v) (procedure-arity-includes? v (entry-arity e)))
    (error (string-append "source must define " (entry-signature e)))))

;; Call the entry point inside the evaluator (time and memory limits apply)
;; and check the shape of the result. Arguments cross as quoted data.
(define (invoke-entry ev kind name input)
  (define e (entry-for kind name))
  (define sym (entry-symbol e))
  (define call `(,sym ,@(for/list ([a (in-list ((entry-args e) input))]) `(quote ,a))))
  ((entry-check e) sym (ev call)))

;; Output used by tests: strings as-is, select_lessons JSON-encoded.
(define (test-output ev kind name input)
  (define r (invoke-entry ev kind name input))
  (if (string? r) r (jsexpr->string r)))
