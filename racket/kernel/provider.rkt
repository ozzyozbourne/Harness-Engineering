#lang racket/base
;; Model providers: anthropic (Messages API over HTTP(S)) and mock (scripted).
(require racket/list racket/string racket/port net/http-client net/url json "util.rkt")
(provide provider-call provider-name make-mock-provider load-mock-script mock-requests
         make-anthropic-provider anthropic-request messages-url
         claude-code-identity default-model)

;; fn: (system-strings tools messages) -> hasheq with 'content and 'stop_reason
(struct provider (name fn requests))
(define (provider-call p system tools messages) ((provider-fn p) system tools messages))

;; ---------------------------------------------------------------- mock

(define (tool-use? b) (and (hash? b) (equal? (hash-ref b 'type #f) "tool_use")))

;; Replays assistant content arrays in order and records every request.
(define (make-mock-provider script)
  (define requests (box '()))
  (define remaining script)
  (define n 0)
  (provider "mock"
            (λ (system tools messages)
              (set! n (add1 n))
              (set-box! requests (cons (hasheq 'system system 'tools tools 'messages messages)
                                       (unbox requests)))
              (when (null? remaining) (error 'mock "script exhausted at request ~a" n))
              (define content (car remaining))
              (set! remaining (cdr remaining))
              (hasheq 'content content
                      'stop_reason (if (ormap tool-use? content) "tool_use" "end_turn")))
            requests))

(define (mock-requests p) (reverse (unbox (provider-requests p))))

(define (load-mock-script file)
  (define s (call-with-input-file file read-json))
  (unless (and (list? s) (andmap list? s))
    (raise-user-error "mock script must be a JSON array of assistant content arrays"))
  s)

;; ---------------------------------------------------------------- anthropic

(define claude-code-identity "You are Claude Code, Anthropic's official CLI for Claude.")
(define default-model "claude-sonnet-5-5")

(define (env name)
  (define v (getenv name))
  (and v (not (string=? (string-trim v) "")) (string-trim v)))

(define (messages-url)
  (string-append (regexp-replace #px"/+$" (or (env "ANTHROPIC_BASE_URL") "https://api.anthropic.com") "")
                 "/v1/messages"))

;; Returns (values url headers body-jsexpr). With an OAuth token the first
;; system block must be the Claude Code identity; the base prompt follows.
(define (anthropic-request model system tools messages)
  (define key (env "ANTHROPIC_API_KEY"))
  (define token (and (not key) (env "CLAUDE_CODE_OAUTH_TOKEN")))
  (unless (or key token)
    (raise-user-error "anthropic provider: set ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN"))
  (define blocks (for/list ([s (if key system (cons claude-code-identity system))])
                   (hasheq 'type "text" 'text s)))
  (define headers
    (append (list "Content-Type: application/json" "anthropic-version: 2023-06-01")
            (if key
                (list (string-append "x-api-key: " key))
                (list (string-append "Authorization: Bearer " token)
                      "anthropic-beta: oauth-2025-04-20"))))
  (define body (hasheq 'model model 'max_tokens 32000 'system blocks 'messages messages))
  (values (messages-url) headers (if (null? tools) body (hash-set body 'tools tools))))

;; POST over http:// or https://. Returns (values status-code body-bytes).
(define (http-post url-string headers data timeout-s)
  (define u (string->url url-string))
  (define scheme (url-scheme u))
  (unless (member scheme '("http" "https"))
    (raise-user-error (format "ANTHROPIC_BASE_URL must be http:// or https://, got ~a" url-string)))
  (define ssl? (equal? scheme "https"))
  (define path
    (string-append "/" (string-join (for/list ([pp (url-path u)]
                                               #:when (string? (path/param-path pp))
                                               #:unless (string=? (path/param-path pp) ""))
                                      (path/param-path pp))
                                    "/")))
  (define ch (make-channel))
  (define th
    (thread
     (λ ()
       (channel-put
        ch
        (with-handlers ([always? values])
          (define-values (status _hdrs in)
            (http-sendrecv (url-host u) path #:ssl? (if ssl? 'secure #f)
                           #:port (or (url-port u) (if ssl? 443 80))
                           #:method "POST" #:headers headers #:data data))
          (begin0 (cons status (port->bytes in)) (close-input-port in)))))))
  (define r (sync/timeout timeout-s ch))
  (cond
    [(not r) (kill-thread th) (error 'anthropic "request timed out after ~a s" timeout-s)]
    [(pair? r)
     (define m (regexp-match #px#"^HTTP/\\S+ (\\d+)" (car r)))
     (values (if m (string->number (bytes->string/utf-8 (cadr m))) 0) (cdr r))]
    [else (raise r)]))

(define retryable '(408 429 500 502 503 504 529))

(define (make-anthropic-provider model)
  (provider
   "anthropic"
   (λ (system tools messages)
     (define-values (url headers body) (anthropic-request model system tools messages))
     (define data (jsexpr->bytes body))
     (let loop ([attempt 1])
       (define-values (code resp)
         (with-handlers ([(λ (e) (and (exn:fail? e) (not (exn:fail:user? e)) (< attempt 4)))
                          (λ (e) (trace! "[provider] network error (attempt ~a): ~a" attempt (exn-message e))
                            (values 'retry #f))])
           (http-post url headers data 600)))
       (cond
         [(eq? code 'retry) (sleep (expt 2 attempt)) (loop (add1 attempt))]
         [(= code 200) (bytes->jsexpr resp)]
         [(and (memv code retryable) (< attempt 4))
          (trace! "[provider] HTTP ~a, retrying" code)
          (sleep (expt 2 attempt))
          (loop (add1 attempt))]
         [else (error 'anthropic "HTTP ~a: ~a" code (one-line (bytes->string/utf-8 resp #\?) 600))])))
   (box '())))
