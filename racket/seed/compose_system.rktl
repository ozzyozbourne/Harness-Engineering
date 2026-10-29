;; Hook. Builds the supplemental system prompt. The kernel always prepends
;; the immutable base prompt, so this cannot remove it.
;; notes: list of strings; lessons: list of strings. Returns a string.
(define (bullets items)
  (apply string-append (map (lambda (s) (string-append "- " s "\n")) items)))

(define (join-notes notes)
  (if (null? notes)
      ""
      (let loop ([ns (cdr notes)] [acc (car notes)])
        (if (null? ns) acc (loop (cdr ns) (string-append acc "\n\n" (car ns)))))))

(define (compose-system notes lessons)
  (string-append
   (if (null? notes) "" (string-append "## Notes\n" (join-notes notes) "\n"))
   (if (and (pair? notes) (pair? lessons)) "\n" "")
   (if (null? lessons) "" (string-append "## Lessons\n" (bullets lessons)))))
