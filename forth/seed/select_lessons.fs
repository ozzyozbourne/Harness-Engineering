\ Seed hook `select_lessons`: {"task": string, "lessons": [{"id", "text", "score"}]}
\ -> JSON array of ids: the 8 highest-scored lessons that share a word of 4+
\ letters with the task, else the top 3 by score (ties: lower id first).
variable in-a  variable in-u  variable task-a  variable task-u
variable n  variable want  variable picked
create ids    512 cells allot
create scores 512 cells allot
create hits   512 cells allot
create used   512 cells allot
: id@    ( i -- id )    cells ids + @ ;
: score@ ( i -- s )     cells scores + @ ;

: alnum? ( c -- f )
  dup [char] a [char] z 1+ within  over [char] A [char] Z 1+ within or
  swap [char] 0 [char] 9 1+ within or ;
\ Splits off the next alphanumeric word; wl = 0 when the text is exhausted.
: next-word ( a u -- a' u' wa wl )
  begin dup 0> if over c@ alnum? 0= else false then while 1 /string repeat
  over 0
  begin 2 pick over > if over over + c@ alnum? else false then while 1+ repeat
  2swap 2 pick /string 2swap ;
\ Is the word wa wu one of the 4+ letter words of text ta tu?
: word-in? ( wa wu ta tu -- f )
  begin next-word dup while
    dup 4 >= if 5 pick 5 pick s= if 2drop 2drop true exit then else 2drop then
  repeat
  2drop 2drop 2drop false ;
: shares? ( la lu -- f )
  begin next-word dup while
    dup 4 >= if task-a @ task-u @ word-in? if 2drop true exit then else 2drop then
  repeat
  2drop 2drop false ;

: field ( i key-a key-u -- a u )
  <b s" lessons." b+ rot bn+ [char] . bc+ b+ b>   in-a @ in-u @ 2swap json-get ;
: load ( -- )
  in-a @ in-u @ s" lessons" json-len 0 max 512 min n !
  n @ 0 ?do
    i s" id" field s>n drop     i cells ids + !
    i s" score" field s>n drop  i cells scores + !
    i s" text" field lower shares?  i cells hits + !
    0 i cells used + !
  loop ;
\ Best unused lesson (highest score, then lowest id); hits only if want is set.
: best ( -- idx|-1 )
  -1
  n @ 0 ?do
    i cells used + @ 0=  want @ 0= i cells hits + @ or  and if
      dup 0< if drop i else
        i score@ over score@ >
        i score@ 2 pick score@ =  i id@ 3 pick id@ <  and  or
        if drop i then
      then
    then
  loop ;
: take ( max -- )
  0 ?do
    best dup 0< if drop leave then
    -1 over cells used + !
    picked @ if [char] , bc+ then  id@ bn+  1 picked +!
  loop ;
: run ( json-addr json-len -- out-addr out-len )
  in-u ! in-a !
  in-a @ in-u @ s" task" json-get lower task-u ! task-a !
  load  0 picked !
  <b [char] [ bc+
  true want ! 8 take
  picked @ 0= if false want ! 3 take then
  [char] ] bc+ b> ;
