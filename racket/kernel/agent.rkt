#lang racket/base
;; The agent loop: one task = prompt assembly, model/tool turns, outcome
;; scoring, note soak, and reflection.
(require racket/list racket/string json
         "util.rkt" "core.rkt" "tools.rkt" "provider.rkt" "base-prompt.rkt")
(provide run-task refine-task-text)

(define max-turns (make-parameter 40))

(define (block-type b) (and (hash? b) (hash-ref b 'type #f)))
(define (text-of content)
  (string-trim (string-join (for/list ([b content] #:when (equal? (block-type b) "text"))
                              (hash-ref b 'text ""))
                            "\n")))

;; System prompt as a list of blocks: the immutable base prompt always comes
;; first, then the supplemental prompt from compose_system plus verdicts.
(define (system-blocks k lesson-ids)
  (define supp (compose-supplemental k lesson-ids))
  (if (string=? supp "") (list base-prompt) (list base-prompt supp)))

;; Returns (values success? answer).
(define (run-task k task)
  (define task-id (add1 (length (kernel-tasks k))))
  (emit! k (hasheq 'type "task_started" 'task_id task-id 'task task))
  (define lesson-ids (select-lesson-ids k task))
  (define trial-notes (live-trial-notes k))
  (define system (system-blocks k lesson-ids))
  (define transcript '())                 ; compact lines for reflection, newest first
  (define (log! fmt . args) (set! transcript (cons (one-line (apply format fmt args) 300) transcript)))
  (log! "task: ~a" task)
  (define-values (stop answer kernel-error tool-errors) (run-turns k system task log!))
  (when kernel-error (trace! "[kernel] error: ~a" (one-line kernel-error 200)) (log! "kernel error: ~a" kernel-error))
  ;; A task succeeds when the model ended its turn normally, nothing broke in
  ;; the kernel, and its last two tool calls (if any) did not fail.
  (define success?
    (and (equal? stop "end_turn") (not kernel-error)
         (not (ormap values (take-up-to tool-errors 2)))))
  ;; Notes soak over tasks: a failed task counts as an error for trial notes.
  (for ([n trial-notes])
    (record-use! k (car n) (cdr n) success? (if success? #f (or kernel-error "task failed")) #f))
  (score-lessons! k lesson-ids success?)
  (emit! k (hasheq 'type "task_finished" 'task_id task-id 'task task 'success success?
                   'summary (one-line (or kernel-error answer) 300)))
  (trace! "[task] #~a ~a" task-id (if success? "succeeded" "failed"))
  (when (kernel-reflect? k) (reflect! k task success? (reverse transcript)))
  (values success? (if (and kernel-error (string=? answer "")) (string-append "error: " kernel-error) answer)))

;; Model/tool turns until the model stops calling tools, the provider fails,
;; or the turn limit is hit. Returns (values stop answer kernel-error tool-errors):
;; the last stop_reason, the last non-empty assistant text, the error that ended
;; the loop or #f, and one error flag per tool call, most recent first.
(define (run-turns k system task log!)
  (define p (kernel-provider k))
  (let loop ([messages (list (hasheq 'role "user" 'content task))]
             [turn 1] [stop #f] [answer ""] [tool-errors '()])
    (cond
      [(> turn (max-turns))
       (values stop answer (format "turn limit ~a reached" (max-turns)) tool-errors)]
      [else
       (define-values (resp provider-error)
         (with-handlers ([always? (λ (e) (values #f (error-text e)))])
           (values (provider-call p system (tool-specs k) messages) #f)))
       (cond
         [(not resp) (values stop answer provider-error tool-errors)]
         [else
          (define content (hash-ref resp 'content '()))
          (define stop* (hash-ref resp 'stop_reason #f))
          (define text (text-of content))
          (unless (string=? text "") (log! "assistant: ~a" text))
          (define answer* (if (string=? text "") answer text))
          (define uses (filter tool-use? content))
          (cond
            [(null? uses) (values stop* answer* #f tool-errors)]
            [else
             (unless (string=? text "") (trace! "[assistant] ~a" text))
             (define-values (results tool-errors*)
               (for/fold ([results '()] [errors tool-errors]
                          #:result (values (reverse results) errors))
                         ([u (in-list uses)])
                 (define-values (err? result) (run-tool-use k u log!))
                 (values (cons result results) (cons err? errors))))
             (loop (append messages (list (hasheq 'role "assistant" 'content content)
                                          (hasheq 'role "user" 'content results)))
                   (add1 turn) stop* answer* tool-errors*)])])])))

;; Run one tool_use block. Returns (values error? tool_result-block).
(define (run-tool-use k u log!)
  (define name (hash-ref u 'name ""))
  (define input (hash-ref u 'input (hasheq)))
  (define-values (err? out) (run-tool k name input))
  (log! "tool ~a ~a -> ~a~a" name (json-brief input 120) (if err? "ERROR " "") (one-line out 160))
  (values err?
          (hasheq 'type "tool_result" 'tool_use_id (hash-ref u 'id "")
                  'content (if (string=? out "") "(empty)" out) 'is_error err?)))

(define reflect-prompt #<<EOF
Reflect on the finished task below. Reply with only a JSON array of 0 to 3
short, reusable lessons (strings) that would help future tasks, for example
["Run the test suite before editing build files."]. Skip task-specific trivia;
reply [] if there is nothing durable to learn.

EOF
  )

(define (parse-lessons text)
  (define m (regexp-match #px"\\[.*\\]" text))
  (define v (and m (with-handlers ([exn:fail? (λ (_) #f)]) (string->jsexpr (car m)))))
  (if (list? v) (filter string? (take-up-to v 3)) #f))

(define (reflect! k task success? transcript)
  (with-handlers ([always? (λ (e) (trace! "[reflect] error: ~a" (one-line (error-text e) 200)))])
    (define body (string-append reflect-prompt
                                (format "Outcome: ~a\nTranscript:\n" (if success? "succeeded" "failed"))
                                (truncate-text (string-join transcript "\n") 12000)))
    (define resp (provider-call (kernel-provider k) (list base-prompt) '()
                                (list (hasheq 'role "user" 'content body))))
    (define lessons (parse-lessons (text-of (hash-ref resp 'content '()))))
    (cond
      [(not lessons) (trace! "[reflect] no JSON array in reply")]
      [else
       (for ([t lessons])
         (define-values (id msg) (add-lesson! k t "reflection"))
         (when id (trace! "[reflect] lesson #~a ~a" id (one-line t 100))))])))

(define (refine-task-text k)
  (string-append
   "Refine the harness. Review the recent task outcomes, the verdicts, and the current "
   "userspace below (use show_def to read sources). Then improve userspace with propose_def "
   "or knowledge with remember. Make at most 3 changes and prefer small ones; making no change "
   "is fine if nothing would clearly help. Finish with a short summary of what you changed.\n\n"
   "## Recent tasks\n" (recent-tasks-text k) "\n\n"
   "## Verdicts\n" (verdicts-text k) "\n\n"
   "## Userspace\n" (defs-text k) "\n"))
