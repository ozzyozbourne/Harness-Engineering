;; Hook. Picks which lessons go into the prompt for a task.
;; task: string; lessons: list of hasheq with 'id 'text 'score.
;; Returns a list of ids: the 8 highest-scored lessons sharing a word of 4+
;; letters with the task, else the top 3 by score.
(define (words s)
  (filter (lambda (w) (>= (string-length w) 4))
          (regexp-split #px"[^a-z0-9]+" (string-downcase s))))

(define (first-n xs n)
  (if (or (null? xs) (= n 0)) '() (cons (car xs) (first-n (cdr xs) (- n 1)))))

(define (select-lessons task lessons)
  (define task-words (words task))
  (define ranked (sort lessons > #:key (lambda (l) (hash-ref l 'score 0))))
  (define matching
    (filter (lambda (l)
              (ormap (lambda (w) (and (member w task-words) #t))
                     (words (hash-ref l 'text ""))))
            ranked))
  (map (lambda (l) (hash-ref l 'id))
       (if (null? matching) (first-n ranked 3) (first-n matching 8))))
