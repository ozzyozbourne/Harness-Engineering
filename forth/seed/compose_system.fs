\ Seed hook `compose_system`: {"notes": [string], "lessons": [string]} ->
\ the supplemental system prompt (notes under "## Notes", lessons under
\ "## Lessons"). The kernel always prepends the immutable base prompt.
variable in-a  variable in-u
: count-of ( key-a key-u -- n )  in-a @ in-u @ 2swap json-len 0 max ;
: item ( key-a key-u i -- a u )
  >r <b b+ [char] . bc+ r> bn+ b>   in-a @ in-u @ 2swap json-get trim ;
: run ( json-addr json-len -- out-addr out-len )
  in-u ! in-a !
  <b
  s" notes" count-of dup if s\" ## Notes\n" b+ then
  0 ?do  s" notes" i item b+  s\" \n\n" b+  loop
  s" lessons" count-of dup if s\" ## Lessons\n" b+ then
  0 ?do  s" - " b+  s" lessons" i item b+  10 bc+  loop
  b> trim ;
