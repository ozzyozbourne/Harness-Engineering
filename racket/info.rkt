#lang info
(define name "cmh")
;; Standard distribution only.
(define deps '("base" "sandbox-lib" "net-lib" "rackunit-lib"))
;; seed/ holds userspace source (.rktl, not modules); bin/ holds the wrapper.
(define test-omit-paths '("seed" "bin"))
