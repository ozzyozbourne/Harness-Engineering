#lang racket/base
;; Small shared helpers: time, text, tracing.
(require racket/string racket/list racket/date json)
(provide now-iso content-words truncate-text one-line trace! trace-port
         json-brief error-text always?)

;; ISO-8601 UTC timestamp.
(define (now-iso)
  (define d (seconds->date (current-seconds) #f))
  (parameterize ([date-display-format 'iso-8601])
    (string-append (date->string d #t) "Z")))

;; Lowercased alphanumeric words of at least `min-len` characters.
(define (content-words s [min-len 4])
  (remove-duplicates
   (filter (λ (w) (>= (string-length w) min-len))
           (regexp-split #px"[^a-z0-9]+" (string-downcase s)))))

(define (truncate-text s n)
  (if (> (string-length s) n)
      (string-append (substring s 0 n)
                     (format "\n...[truncated ~a chars]" (- (string-length s) n)))
      s))

(define (one-line s [n 120])
  (define flat (string-normalize-spaces (regexp-replace* #px"[\r\n\t]+" s " ")))
  (if (> (string-length flat) n) (string-append (substring flat 0 n) "...") flat))

(define (json-brief v [n 200])
  (one-line (jsexpr->string v) n))

(define (always? e) (not (exn:break? e)))

(define (error-text e)
  (if (exn? e) (exn-message e) (format "raised ~e" e)))

;; Compact trace line on stderr (tests redirect it through trace-port).
(define trace-port (make-parameter #f))
(define (trace! fmt . args)
  (define err (or (trace-port) (current-error-port)))
  (write-string (one-line (apply format fmt args) 400) err)
  (newline err)
  (flush-output err))
