;; Userspace tool. Contract: (define (run input) ...) where input is a
;; jsexpr hasheq with symbol keys; returns a string.
(define (run input)
  (define text (hash-ref input 'text ""))
  (number->string (length (regexp-match* #px"\\S+" text))))
