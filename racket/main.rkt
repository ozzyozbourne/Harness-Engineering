#lang racket/base
;; Entry point: racket main.rkt <args>
(require "kernel/cli.rkt")
(module+ main
  (exit (main (current-command-line-arguments))))
