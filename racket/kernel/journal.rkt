#lang racket/base
;; Append-only journal.jsonl: one JSON object per line, each with type and ts.
(require json racket/file "util.rkt")
(provide journal-append! journal-read)

(define (journal-append! path ev)
  (define full (hash-set ev 'ts (now-iso)))
  (call-with-output-file path #:exists 'append
    (λ (o) (write-json full o) (newline o) (flush-output o)))
  full)

;; All events in order. A torn final line (crash mid-write) is skipped.
(define (journal-read path)
  (if (file-exists? path)
      (for*/list ([line (in-list (file->lines path))]
                  #:when (regexp-match? #px"\\S" line)
                  [ev (in-value (with-handlers ([exn:fail? (λ (_)
                                                             (trace! "[journal] skipping unreadable line")
                                                             #f)])
                                  (string->jsexpr line)))]
                  #:when (hash? ev))
        ev)
      '()))
