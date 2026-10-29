#lang racket/base
;; Sandboxed evaluation of userspace Racket source. Every definition version
;; lives in its own racket/sandbox evaluator; validation uses a fresh scratch
;; evaluator that is thrown away. Only plain data crosses the boundary: inputs
;; are quoted jsexprs, results are checked to be strings / lists of ids.
(require racket/sandbox json)
(provide new-evaluator destroy-evaluator load-source! check-entry! invoke-entry
         test-output entry-signature sandbox-time-limit sandbox-memory-mb)

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

(define (select-hook? kind name) (and (equal? kind "hook") (equal? name "select_lessons")))

(define (entry kind name)
  (cond [(equal? kind "tool") (values 'run 1)]
        [(select-hook? kind name) (values 'select-lessons 2)]
        [else (values 'compose-system 2)]))

(define (entry-signature kind name)
  (cond [(equal? kind "tool") "(define (run input) ...)"]
        [(select-hook? kind name) "(define (select-lessons task lessons) ...)"]
        [else "(define (compose-system notes lessons) ...)"]))

(define (check-entry! ev kind name)
  (define-values (sym arity) (entry kind name))
  (define v (with-handlers ([exn:fail? (λ (_) #f)]) (ev sym)))
  (unless (and (procedure? v) (procedure-arity-includes? v arity))
    (error (string-append "source must define " (entry-signature kind name)))))

;; Call the entry point inside the evaluator (time and memory limits apply)
;; and check the shape of the result.
(define (invoke-entry ev kind name input)
  (define-values (sym _arity) (entry kind name))
  (define result
    (cond [(equal? kind "tool") (ev `(run (quote ,input)))]
          [(select-hook? kind name)
           (ev `(select-lessons (quote ,(hash-ref input 'task ""))
                                (quote ,(hash-ref input 'lessons '()))))]
          [else
           (ev `(compose-system (quote ,(hash-ref input 'notes '()))
                                (quote ,(hash-ref input 'lessons '()))))]))
  (cond
    [(select-hook? kind name)
     (unless (and (list? result) (andmap exact-integer? result))
       (error (format "select-lessons must return a list of lesson ids, got ~e" result)))
     result]
    [else
     (unless (string? result)
       (error (format "~a must return a string, got ~e" sym result)))
     (string->immutable-string result)]))

;; Output used by tests: strings as-is, select_lessons JSON-encoded.
(define (test-output ev kind name input)
  (define r (invoke-entry ev kind name input))
  (if (string? r) r (jsexpr->string r)))
