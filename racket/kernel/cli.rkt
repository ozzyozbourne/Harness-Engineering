#lang racket/base
;; Command line: cmh [flags] <command>. (main argv) returns the exit code.
(require racket/cmdline racket/string racket/port
         "util.rkt" "core.rkt" "agent.rkt" "provider.rkt")
(provide main)

(define usage #<<EOF
cmh [--state DIR] [--provider anthropic|mock] [--script FILE] [--model M]
    [--soak N] [--no-reflect] <command>
  repl | run "TASK" | refine | defs | knowledge
EOF
  )

(define (main argv)
  (define state ".cmh")
  (define provider-kind "anthropic")
  (define script #f)
  (define model #f)
  (define soak 3)
  (define reflect? #t)
  (define args
    (command-line
     #:program "cmh"
     #:argv argv
     #:once-each
     [("--state") dir "State dir (default .cmh)" (set! state dir)]
     [("--provider") p "anthropic (default) or mock" (set! provider-kind p)]
     [("--script") file "Mock provider script" (set! script file)]
     [("--model") m "Model (default $CMH_MODEL or claude-sonnet-5-5)" (set! model m)]
     [("--soak") n "Clean uses before a trial is kept (default 3)"
                 (set! soak (or (string->number n) (raise-user-error "--soak needs a number")))]
     [("--no-reflect") "Disable reflection" (set! reflect? #f)]
     #:args args args))
  (with-handlers ([exn:fail:user? (λ (e) (eprintf "cmh: ~a\n" (exn-message e)) 2)])
    (define (make-provider)
      (case provider-kind
        [("mock") (unless script (raise-user-error "--provider mock needs --script FILE"))
                  (make-mock-provider (load-mock-script script))]
        [("anthropic") (make-anthropic-provider (or model (getenv "CMH_MODEL") default-model))]
        [else (raise-user-error (format "unknown provider ~a" provider-kind))]))
    (define (open needs-provider?)
      (open-kernel state #:provider (and needs-provider? (make-provider)) #:soak soak #:reflect? reflect?))
    (define (task! k text)
      (define-values (ok? answer) (run-task k text))
      (displayln answer)
      (flush-output)
      (if ok? 0 1))
    (cond
      [(null? args) (eprintf "~a\n" usage) 2]
      [else
       (case (car args)
         [("run")
          (when (null? (cdr args)) (raise-user-error "run needs a TASK"))
          (task! (open #t) (string-join (cdr args) " "))]
         [("refine") (let ([k (open #t)]) (task! k (refine-task-text k)))]
         [("defs") (displayln (defs-text (open #f))) 0]
         [("knowledge") (displayln (knowledge-text (open #f))) 0]
         [("repl") (repl (open #t))]
         [else (eprintf "unknown command ~a\n~a\n" (car args) usage) 2])])))

(define (repl k)
  (let loop ()
    (display "cmh> ")
    (flush-output)
    (define line (read-line))
    (cond
      [(eof-object? line) (newline) 0]
      [else
       (define l (string-trim line))
       (define parts (string-split l))
       (define (num s) (and s (string->number s)))
       (cond
         [(string=? l "") (loop)]
         [(equal? l "/quit") 0]
         [(equal? l "/defs") (displayln (defs-text k)) (loop)]
         [(equal? l "/knowledge") (displayln (knowledge-text k)) (loop)]
         [(and (equal? (car parts) "/show") (pair? (cdr parts)))
          (displayln (show-def-text k (cadr parts) (num (and (pair? (cddr parts)) (caddr parts)))))
          (loop)]
         [(and (equal? (car parts) "/rollback") (= (length parts) 3) (num (caddr parts)))
          (let-values ([(ok? msg) (rollback! k (cadr parts) (num (caddr parts)))])
            (trace! "[gate] rollback ~a -> ~a" (cadr parts) (one-line msg 120))
            (displayln msg))
          (loop)]
         [(equal? l "/refine") (let-values ([(ok? a) (run-task k (refine-task-text k))]) (displayln a)) (loop)]
         [(string-prefix? l "/") (displayln "commands: /defs /show NAME [VERSION] /knowledge /refine /rollback NAME VERSION /quit") (loop)]
         [else (let-values ([(ok? a) (run-task k l)]) (displayln a)) (loop)])])))
